use std::sync::Arc;

use anyhow::anyhow;
use parking_lot::Mutex;
use tracing::{debug, info, warn};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelMessage};

use crate::hardware::usb::storage::{
    append_ft_upload_data, append_upload_data, claim_ft_upload_channel, claim_upload_channel,
    complete_ft_upload, complete_upload, get_ft_upload_progress, get_upload_progress,
    set_webrtc_read_handler,
};
use crate::ip_forward::IpForward;
use crate::jsonrpc::PROCESSOR;
use crate::remote_mount;
use crate::terminal::WebTerminal;
use crate::web::get_global_app_state;
use crate::webrtc::{spawn_dc_events, DcCallbacks};

/// Handle incoming RPC messages through WebRTC data channel
// pub async fn handle_rpc_message(msg: DataChannelMessage, session: &Session) {
//     debug!("Received RPC message: {} bytes", msg.data.len());

//     // Create JSON-RPC processor with default registry
//     let registry = Arc::new(create_default_registry());
//     let processor = JsonRpcProcessor::new(registry);

//     // Process the JSON-RPC message
//     processor.handle_message(msg, session).await;
// }

/// Handle local WebTerminal data channel (label `terminal`).
pub async fn handle_terminal_channel(channel: Arc<dyn DataChannel>, session_id: String) {
    let label = channel.label().await.unwrap_or_default();
    let channel_id = channel.id();
    info!("Terminal data channel '{}' (ID: {:?}) established", label, channel_id);

    match WebTerminal::attach_async(channel).await {
        Ok(handle) => {
            if let Some(session) = get_global_app_state().get_session_by_id(&session_id).await {
                if let Err(rejected) = session.install_terminal(handle) {
                    warn!("session {} already closing; stopping terminal", session_id);
                    rejected.stop_and_wait().await;
                } else {
                    info!("WebTerminal handler registered on session {}", session_id);
                }
            } else {
                warn!("WebTerminal attached but session {} gone; stopping terminal", session_id);
                handle.stop_and_wait().await;
            }
        }
        Err(e) => warn!("Failed to setup WebTerminal channel: {}", e),
    }
}

/// Handle IP forward DataChannel after consuming a one-shot ticket.
pub async fn handle_ip_forward_channel(channel: Arc<dyn DataChannel>, session_id: String) {
    let label = channel.label().await.unwrap_or_default();
    let channel_id = channel.id();
    info!("IP forward data channel '{}' (ID: {:?}) for session {}", label, channel_id, session_id);

    let params = match crate::ip_forward::consume(&label, &session_id).await {
        Ok(params) => params,
        Err(e) => {
            warn!("Rejecting ip forward channel '{}': {}", label, e);
            let _ = channel.close().await;
            return;
        }
    };

    if let Err(e) = IpForward::attach_async(channel.clone(), params).await {
        warn!("Failed to setup IpForward channel '{}': {}", label, e);
        let _ = channel.close().await;
    } else {
        info!("IpForward handler finished for '{}'", label);
    }
}

/// Handle serial data channel
pub async fn handle_serial_channel(channel: Arc<dyn DataChannel>) {
    let label = channel.label().await.unwrap_or_default();
    info!("Serial data channel '{}' established", label);

    // Set up message handler for serial data
    spawn_dc_events(
        channel,
        DcCallbacks {
            on_message: Some(Box::new(move |msg: RTCDataChannelMessage| {
                tokio::spawn(async move {
                    handle_serial_message(msg).await;
                });
            })),
            on_open: Some(Box::new(|| info!("Serial channel opened"))),
            on_close: Some(Box::new(|| info!("Serial channel closed"))),
            ..Default::default()
        },
    );
}

/// Handle serial messages
async fn handle_serial_message(msg: RTCDataChannelMessage) {
    debug!("Received serial data: {} bytes", msg.data.len());

    // TODO: Forward serial data to/from actual serial port
    // This would include:
    // - Serial port communication
    // - Baud rate configuration
    // - Hardware flow control
    // - Data format handling
}

/// Handle upload data channels (dynamic channels with upload_ prefix).
/// This function is already the channel's single task: each chunk and the
/// final rename run here, in order, before the next event is read.
pub async fn handle_upload_channel(channel: Arc<dyn DataChannel>) {
    let label = channel.label().await.unwrap_or_default();
    info!("Upload data channel '{}' established", label);

    if !label.starts_with("upload_") {
        warn!("Invalid upload channel label: {}", label);
        return;
    }

    let upload_id = label;
    info!("Starting file upload with ID: {}", upload_id);
    if let Err(e) = claim_upload_channel(&upload_id).await {
        warn!("Rejecting upload channel {}: {}", upload_id, e);
        let _ = channel.close().await;
        return;
    }

    let last_progress = Arc::new(Mutex::new(std::time::Instant::now()));
    let mut last_reported = 0i64;
    let mut finalized = false;
    let mut finished = false;
    let mut progress_tick = tokio::time::interval(std::time::Duration::from_millis(200));
    progress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress_tick.tick().await;
    loop {
        tokio::select! {
            event = channel.poll() => {
                let Some(event) = event else { break };
                match event {
                    DataChannelEvent::OnMessage(msg) if !finished => {
                        if write_storage_chunk(&channel, &upload_id, &last_progress, &mut last_reported, msg).await {
                            finished = true;
                        }
                    }
                    DataChannelEvent::OnError => {
                        warn!("DC error on label={} id={}", upload_id, channel.id());
                        finalize_storage_upload(&upload_id).await;
                        finalized = true;
                        break;
                    }
                    DataChannelEvent::OnClose | DataChannelEvent::OnClosing => {
                        finalize_storage_upload(&upload_id).await;
                        finalized = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ = progress_tick.tick(), if !finished => {
                report_storage_progress_if_advanced(&channel, &upload_id, &last_progress, &mut last_reported).await;
            }
        }
    }
    if !finalized {
        finalize_storage_upload(&upload_id).await;
    }
}

async fn write_storage_chunk(
    channel: &Arc<dyn DataChannel>,
    upload_id: &str,
    last_progress: &Mutex<std::time::Instant>,
    last_reported: &mut i64,
    msg: RTCDataChannelMessage,
) -> bool {
    let (size, already) = match append_upload_data(upload_id, &msg.data).await {
        Ok(progress) => progress,
        Err(e) => {
            warn!("failed to write upload chunk {}: {}", upload_id, e);
            return false;
        }
    };

    let mut should_send = false;
    {
        let mut t = last_progress.lock();
        if t.elapsed() >= std::time::Duration::from_millis(200) {
            *t = std::time::Instant::now();
            should_send = true;
        }
    }

    if already >= size {
        let progress = serde_json::json!({
            "Size": size,
            "AlreadyUploadedBytes": already,
        });
        if let Err(e) = channel.send_text(&progress.to_string()).await {
            warn!("failed to send final upload progress {}: {}", upload_id, e);
        } else {
            *last_reported = already;
        }
        return true;
    }

    if should_send {
        let progress = serde_json::json!({
            "Size": size,
            "AlreadyUploadedBytes": already,
        });
        if let Err(e) = channel.send_text(&progress.to_string()).await {
            warn!("failed to send upload progress {}: {}", upload_id, e);
        } else {
            *last_reported = already;
        }
    }
    false
}

async fn report_storage_progress_if_advanced(
    channel: &Arc<dyn DataChannel>,
    upload_id: &str,
    last_progress: &Mutex<std::time::Instant>,
    last_reported: &mut i64,
) {
    let Ok((size, already)) = get_upload_progress(upload_id).await else {
        return;
    };
    if already <= *last_reported {
        return;
    }
    let progress = serde_json::json!({
        "Size": size,
        "AlreadyUploadedBytes": already,
    });
    if let Err(e) = channel.send_text(&progress.to_string()).await {
        warn!("failed to send upload progress {}: {}", upload_id, e);
        return;
    }
    *last_reported = already;
    *last_progress.lock() = std::time::Instant::now();
}

async fn finalize_storage_upload(upload_id: &str) {
    match get_upload_progress(upload_id).await {
        Ok((size, already)) => {
            if already >= size {
                info!("Upload {} completed (on close): {}/{}", upload_id, already, size);
            } else {
                warn!("Upload {} channel closed early: {}/{}", upload_id, already, size);
            }
        }
        Err(e) => {
            debug!("Upload {} close: progress not available: {}", upload_id, e);
        }
    }
    if let Err(e) = complete_upload(upload_id).await {
        debug!("complete_upload on close ({}): {}", upload_id, e);
    }
}

pub async fn handle_ft_upload_channel(channel: Arc<dyn DataChannel>) {
    let label = channel.label().await.unwrap_or_default();
    info!("Upload data channel '{}' established", label);

    if !label.starts_with("ft_upload_") {
        warn!("Invalid upload channel label: {}", label);
        return;
    }

    let upload_id = label;
    info!("Starting file upload with ID: {}", upload_id);
    if let Err(e) = claim_ft_upload_channel(&upload_id).await {
        warn!("Rejecting upload channel {}: {}", upload_id, e);
        let _ = channel.close().await;
        return;
    }

    let last_progress = Arc::new(Mutex::new(std::time::Instant::now()));
    let mut last_reported = 0i64;
    let mut finalized = false;
    let mut finished = false;
    let mut progress_tick = tokio::time::interval(std::time::Duration::from_millis(200));
    progress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress_tick.tick().await;
    loop {
        tokio::select! {
            event = channel.poll() => {
                let Some(event) = event else { break };
                match event {
                    DataChannelEvent::OnMessage(msg) if !finished => {
                        if write_ft_chunk(&channel, &upload_id, &last_progress, &mut last_reported, msg).await {
                            finished = true;
                        }
                    }
                    DataChannelEvent::OnError => {
                        warn!("DC error on label={} id={}", upload_id, channel.id());
                        finalize_ft_upload(&upload_id).await;
                        finalized = true;
                        break;
                    }
                    DataChannelEvent::OnClose | DataChannelEvent::OnClosing => {
                        finalize_ft_upload(&upload_id).await;
                        finalized = true;
                        break;
                    }
                    _ => {}
                }
            }
            _ = progress_tick.tick(), if !finished => {
                report_ft_progress_if_advanced(&channel, &upload_id, &last_progress, &mut last_reported).await;
            }
        }
    }
    if !finalized {
        finalize_ft_upload(&upload_id).await;
    }
}

async fn write_ft_chunk(
    channel: &Arc<dyn DataChannel>,
    upload_id: &str,
    last_progress: &Mutex<std::time::Instant>,
    last_reported: &mut i64,
    msg: RTCDataChannelMessage,
) -> bool {
    let (size, already) = match append_ft_upload_data(upload_id, &msg.data).await {
        Ok(progress) => progress,
        Err(e) => {
            warn!("failed to write upload chunk {}: {}", upload_id, e);
            return false;
        }
    };

    let mut should_send = false;
    {
        let mut t = last_progress.lock();
        if t.elapsed() >= std::time::Duration::from_millis(200) {
            *t = std::time::Instant::now();
            should_send = true;
        }
    }

    if already >= size {
        let progress = serde_json::json!({
            "Size": size,
            "AlreadyUploadedBytes": already,
        });
        if let Err(e) = channel.send_text(&progress.to_string()).await {
            warn!("failed to send final upload progress {}: {}", upload_id, e);
        } else {
            *last_reported = already;
        }
        return true;
    }

    if should_send {
        let progress = serde_json::json!({
            "Size": size,
            "AlreadyUploadedBytes": already,
        });
        if let Err(e) = channel.send_text(&progress.to_string()).await {
            warn!("failed to send upload progress {}: {}", upload_id, e);
        } else {
            *last_reported = already;
        }
    }
    false
}

async fn report_ft_progress_if_advanced(
    channel: &Arc<dyn DataChannel>,
    upload_id: &str,
    last_progress: &Mutex<std::time::Instant>,
    last_reported: &mut i64,
) {
    let Ok((size, already)) = get_ft_upload_progress(upload_id).await else {
        return;
    };
    if already <= *last_reported {
        return;
    }
    let progress = serde_json::json!({
        "Size": size,
        "AlreadyUploadedBytes": already,
    });
    if let Err(e) = channel.send_text(&progress.to_string()).await {
        warn!("failed to send upload progress {}: {}", upload_id, e);
        return;
    }
    *last_reported = already;
    *last_progress.lock() = std::time::Instant::now();
}

async fn finalize_ft_upload(upload_id: &str) {
    match get_ft_upload_progress(upload_id).await {
        Ok((size, already)) => {
            if already >= size {
                info!("Upload {} completed (on close): {}/{}", upload_id, already, size);
            } else {
                warn!("Upload {} channel closed early: {}/{}", upload_id, already, size);
            }
        }
        Err(e) => {
            debug!("Upload {} close: progress not available: {}", upload_id, e);
        }
    }
    if let Err(e) = complete_ft_upload(upload_id).await {
        debug!("complete_upload on close ({}): {}", upload_id, e);
    }
}

/// Data channel management utilities
pub struct DataChannelManager {
    upload_prefix: String,
    ft_upload_prefix: String,
    ip_forward_prefix: String,
}

impl DataChannelManager {
    pub fn new() -> Self {
        Self {
            upload_prefix: "upload_".to_string(),
            ft_upload_prefix: "ft_upload_".to_string(),
            ip_forward_prefix: crate::ip_forward::IP_FORWARD_PREFIX.to_string(),
        }
    }

    /// Route data channel based on its label
    pub async fn route_data_channel(&self, channel: Arc<dyn DataChannel>, session_id: String) {
        let label = channel.label().await.unwrap_or_default();

        match label.as_str() {
            "rpc" => {
                info!("Setting up RPC data channel for session: {}", &session_id);
                self.setup_rpc_channel(channel, session_id).await;
            }

            "input" => {
                info!("Setting up input data channel for session: {}", &session_id);
                self.setup_input_channel(channel, session_id).await;
            }

            "input_unstable" => {
                info!("Setting up input unstable data channel for session: {}", &session_id);
                self.setup_input_channel(channel, session_id).await;
            }

            "disk" => {
                info!("Setting up disk data channel for session: {}", &session_id);
                self.setup_disk_channel(channel, session_id).await;
            }

            "terminal" => {
                info!("Setting up WebTerminal data channel for session: {}", &session_id);
                handle_terminal_channel(channel, session_id).await;
            }

            "serial" => {
                info!("Setting up serial data channel");
                handle_serial_channel(channel).await;
            }

            _ if label.starts_with(&self.upload_prefix) => {
                info!("Setting up upload data channel");
                tokio::spawn(handle_upload_channel(channel));
            }

            _ if label.starts_with(&self.ft_upload_prefix) => {
                tokio::spawn(handle_ft_upload_channel(channel));
            }

            _ if label.starts_with(&self.ip_forward_prefix) => {
                // Must not await here: blocking on_data_channel prevents on_open from
                // running, so wait_dc_open would deadlock until timeout.
                info!("Setting up IpForward data channel");
                tokio::spawn(handle_ip_forward_channel(channel, session_id));
            }

            _ => {
                warn!("Unknown data channel type: {}", label);
            }
        }
    }

    async fn setup_rpc_channel(&self, channel: Arc<dyn DataChannel>, session_id: String) {
        // Store the RPC channel globally for this session
        // crate::webrtc::store_rpc_channel(&session.id, channel.clone()).await;
        get_global_app_state()
            .update_session_rpc_channel(session_id.as_str(), channel.clone())
            .await;

        // let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let channel_clone = channel.clone();
        let session_id_for_msg = session_id.clone();
        let on_message = Box::new(move |msg: RTCDataChannelMessage| {
            // let tx = tx.clone();
            let channel = channel_clone.clone();
            let session_id = session_id_for_msg.clone();
            tokio::spawn(async move {
                // if let Err(e) = tx.send(msg).await {
                //     error!("failed to send message to RPC channel: {}", e);
                // }
                tokio::spawn(PROCESSOR.handle_message(msg, channel, session_id));
            });
        });

        // let channel_clone = channel.clone();
        // tokio::spawn(async move {
        //     let channel = channel_clone.clone();
        //     while let Some(msg) = rx.recv().await {

        //     }
        // });

        // When the RPC data channel is opened, trigger state updates so
        // events are sent only after the channel is ready and stored
        let on_open = Box::new(move || {
            tokio::spawn(async move {
                info!("RPC channel opened - triggering state updates");
                // crate::webrtc::trigger_ota_state_update().await;
                crate::webrtc::trigger_video_state_update().await;
                crate::webrtc::trigger_usb_state_update().await;
                crate::webrtc::trigger_keyboard_led_state_update().await;

                // Also emit session count update now that the channel is open
                crate::webrtc::on_active_sessions_changed().await;

                // crate::jsonrpc::broadcast_virtual_cm_state(crate::module::VirtualCMState { camera: true, microphone: true }).await;
            });
        });
        spawn_dc_events(
            channel,
            DcCallbacks {
                on_open: Some(on_open),
                on_message: Some(on_message),
                ..Default::default()
            },
        );
    }

    async fn setup_input_channel(&self, channel: Arc<dyn DataChannel>, session_id: String) {
        spawn_dc_events(
            channel,
            DcCallbacks {
                on_message: Some(Box::new(move |msg: RTCDataChannelMessage| {
                    let session_id = session_id.clone();
                    tokio::spawn(async move {
                        PROCESSOR.handle_input_message(msg, session_id).await;
                    });
                })),
                ..Default::default()
            },
        );
    }

    async fn setup_disk_channel(&self, channel: Arc<dyn DataChannel>, session_id: String) {
        // On open: install sender and bridge
        let ch_for_open = channel.clone();
        let on_open = Box::new(move || {
            let ch = ch_for_open.clone();
            tokio::spawn(async move {
                let rt = tokio::runtime::Handle::current();
                remote_mount::webrtc_disk_set_sender(Arc::new(move |text: &str| {
                    let ch = ch.clone();
                    let text = text.to_string();
                    rt.spawn(async move {
                        let _ = ch.send_text(&text).await.map_err(|e| anyhow!(e.to_string()));
                    });
                    Ok(())
                }));
                remote_mount::install_webrtc_disk_bridge();
                info!("Disk data channel opened and bridge installed");
            });
        });

        // Forward inbound data to remote_mount
        let on_message = Box::new(move |msg: RTCDataChannelMessage| {
            let session_id = session_id.clone();
            tokio::spawn(async move {
                debug!("disk msg {} bytes (session={})", msg.data.len(), session_id);
                remote_mount::webrtc_disk_on_message(&msg.data);
            });
        });

        // On close: clear sender and handler
        let on_close = Box::new(move || {
            tokio::spawn(async move {
                info!("Disk data channel closed");
                remote_mount::webrtc_disk_clear_sender();
                set_webrtc_read_handler(None);
            });
        });
        spawn_dc_events(
            channel,
            DcCallbacks {
                on_open: Some(on_open),
                on_message: Some(on_message),
                on_close: Some(on_close),
                ..Default::default()
            },
        );
    }
}

impl Default for DataChannelManager {
    fn default() -> Self {
        Self::new()
    }
}
