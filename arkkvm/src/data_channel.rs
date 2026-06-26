use std::sync::Arc;

use anyhow::anyhow;
use parking_lot::Mutex;
use tracing::{debug, error, info, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;

use crate::hardware::usb::storage::{
    append_ft_upload_data, append_upload_data, complete_ft_upload, complete_upload, get_ft_upload_progress, get_upload_progress, set_webrtc_read_handler
};
use crate::jsonrpc::PROCESSOR;
use crate::remote_mount;
use crate::terminal::setup_terminal_channel;
use crate::web::get_global_app_state;

/// Handle incoming RPC messages through WebRTC data channel
// pub async fn handle_rpc_message(msg: DataChannelMessage, session: &Session) {
//     debug!("Received RPC message: {} bytes", msg.data.len());

//     // Create JSON-RPC processor with default registry
//     let registry = Arc::new(create_default_registry());
//     let processor = JsonRpcProcessor::new(registry);

//     // Process the JSON-RPC message
//     processor.handle_message(msg, session).await;
// }

/// Handle terminal data channel - implements the full terminal functionality
pub async fn handle_terminal_channel(channel: Arc<RTCDataChannel>) {
    let label = channel.label().to_string();
    let channel_id = channel.id();
    info!("Terminal data channel '{}' (ID: {:?}) established", label, channel_id);

    // Create terminal handler which automatically sets up all event handlers
    match setup_terminal_channel(channel).await {
        Ok(_handler) => {
            // Handler is now managing the entire terminal lifecycle
            info!("Terminal handler successfully initialized");
        }
        Err(e) => {
            warn!("Failed to setup terminal channel: {}", e);
        }
    }
}

/// Handle serial data channel
pub async fn handle_serial_channel(channel: Arc<RTCDataChannel>) {
    let label = channel.label().to_string();
    info!("Serial data channel '{}' established", label);

    // Set up message handler for serial data
    channel.on_message(Box::new(move |msg: DataChannelMessage| {
        Box::pin(async move {
            handle_serial_message(msg).await;
        })
    }));

    // Set up channel state handlers
    channel.on_open(Box::new(move || {
        Box::pin(async move {
            info!("Serial channel opened");
        })
    }));

    channel.on_close(Box::new(move || {
        Box::pin(async move {
            info!("Serial channel closed");
        })
    }));
}

/// Handle serial messages
async fn handle_serial_message(msg: DataChannelMessage) {
    debug!("Received serial data: {} bytes", msg.data.len());

    // TODO: Forward serial data to/from actual serial port
    // This would include:
    // - Serial port communication
    // - Baud rate configuration
    // - Hardware flow control
    // - Data format handling
}

/// Handle upload data channels (dynamic channels with upload_ prefix)
pub async fn handle_upload_channel(channel: Arc<RTCDataChannel>) {
    let label = channel.label().to_string();
    info!("Upload data channel '{}' established", label);

    if !label.starts_with("upload_") {
        warn!("Invalid upload channel label: {}", label);
        return;
    }

    let upload_id = label.clone();
    info!("Starting file upload with ID: {}", upload_id);

    // Throttle progress to ~200ms using non-poisoning mutex
    let last_progress = Arc::new(Mutex::new(std::time::Instant::now()));

    // Message handler: write chunk + throttled progress feedback
    let ch_for_msg = channel.clone();
    let upload_id_for_msg = upload_id.clone();
    let last_for_msg = last_progress.clone();
    channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let ch = ch_for_msg.clone();
        let upload_id = upload_id_for_msg.clone();
        let last = last_for_msg.clone();
        Box::pin(async move {
            // Write chunk
            if let Err(e) = append_upload_data(&upload_id, &msg.data).await {
                warn!("failed to write upload chunk {}: {}", upload_id, e);
                return;
            }

            // Throttle progress send to ~200ms
            let mut should_send = false;
            {
                let mut t = last.lock();
                if t.elapsed() >= std::time::Duration::from_millis(200) {
                    *t = std::time::Instant::now();
                    should_send = true;
                }
            }

            // Read current progress
            match get_upload_progress(&upload_id).await {
                Ok((size, already)) => {
                    // Force-send final progress when done, even if throttled
                    if already >= size {
                        let progress = serde_json::json!({
                            "Size": size,
                            "AlreadyUploadedBytes": already,
                        });
                        if let Err(e) = ch.send_text(progress.to_string()).await {
                            warn!("failed to send final upload progress {}: {}", upload_id, e);
                        }
                        if let Err(e) = ch.close().await {
                            warn!("failed to close upload channel {}: {}", upload_id, e);
                        }
                        return;
                    }

                    if should_send {
                        let progress = serde_json::json!({
                            "Size": size,
                            "AlreadyUploadedBytes": already,
                        });
                        if let Err(e) = ch.send_text(progress.to_string()).await {
                            warn!("failed to send upload progress {}: {}", upload_id, e);
                        }
                    }
                }
                Err(e) => {
                    // Not fatal; just log
                    warn!("failed to get upload progress {}: {}", upload_id, e);
                }
            }
        })
    }));

    // Finalize on close (rename .incomplete -> final if complete)
    let upload_id_for_close = upload_id.clone();
    channel.on_close(Box::new(move || {
        let upload_id = upload_id_for_close.clone();
        Box::pin(async move {
            match get_upload_progress(&upload_id).await {
                Ok((size, already)) => {
                    if already >= size {
                        info!("Upload {} completed (on close): {}/{}", upload_id, already, size);
                    } else {
                        warn!("Upload {} channel closed early: {}/{}", upload_id, already, size);
                    }
                }
                Err(e) => {
                    // Not found likely means already finalized earlier
                    debug!("Upload {} close: progress not available: {}", upload_id, e);
                }
            }
            // Try to finalize if still pending; ignore 'not found'
            if let Err(e) = complete_upload(&upload_id).await {
                debug!("complete_upload on close ({}): {}", upload_id, e);
            }
        })
    }));
}

pub async fn handle_ft_upload_channel(channel: Arc<RTCDataChannel>) {
    let label = channel.label().to_string();
    info!("Upload data channel '{}' established", label);

    if !label.starts_with("ft_upload_") {
        warn!("Invalid upload channel label: {}", label);
        return;
    }

    let upload_id = label.clone();
    info!("Starting file upload with ID: {}", upload_id);

    // Throttle progress to ~200ms using non-poisoning mutex
    let last_progress = Arc::new(Mutex::new(std::time::Instant::now()));

    // Message handler: write chunk + throttled progress feedback
    let ch_for_msg = channel.clone();
    let upload_id_for_msg = upload_id.clone();
    let last_for_msg = last_progress.clone();

    channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let ch = ch_for_msg.clone();
        let upload_id = upload_id_for_msg.clone();
        let last = last_for_msg.clone();
        Box::pin(async move {
            // Write chunk
            if let Err(e) = append_ft_upload_data(&upload_id, &msg.data).await {
                warn!("failed to write upload chunk {}: {}", upload_id, e);
                return;
            }

            // Throttle progress send to ~200ms
            let mut should_send = false;
            {
                let mut t = last.lock();
                if t.elapsed() >= std::time::Duration::from_millis(200) {
                    *t = std::time::Instant::now();
                    should_send = true;
                }
            }

            // Read current progress
            match get_ft_upload_progress(&upload_id).await {
                Ok((size, already)) => {
                    // Force-send final progress when done, even if throttled
                    if already >= size {
                        let progress = serde_json::json!({
                            "Size": size,
                            "AlreadyUploadedBytes": already,
                        });
                        if let Err(e) = ch.send_text(progress.to_string()).await {
                            warn!("failed to send final upload progress {}: {}", upload_id, e);
                        }
                        if let Err(e) = ch.close().await {
                            warn!("failed to close upload channel {}: {}", upload_id, e);
                        }
                        return;
                    }

                    if should_send {
                        let progress = serde_json::json!({
                            "Size": size,
                            "AlreadyUploadedBytes": already,
                        });
                        if let Err(e) = ch.send_text(progress.to_string()).await {
                            warn!("failed to send upload progress {}: {}", upload_id, e);
                        }
                    }
                }
                Err(e) => {
                    // Not fatal; just log
                    warn!("failed to get upload progress {}: {}", upload_id, e);
                }
            }
        })
    }));

    // Finalize on close (rename .incomplete -> final if complete)
    let upload_id_for_close = upload_id.clone();
    channel.on_close(Box::new(move || {
        let upload_id = upload_id_for_close.clone();
        Box::pin(async move {
            match get_ft_upload_progress(&upload_id).await {
                Ok((size, already)) => {
                    if already >= size {
                        info!("Upload {} completed (on close): {}/{}", upload_id, already, size);
                    } else {
                        warn!("Upload {} channel closed early: {}/{}", upload_id, already, size);
                    }
                }
                Err(e) => {
                    // Not found likely means already finalized earlier
                    debug!("Upload {} close: progress not available: {}", upload_id, e);
                }
            }
            // Try to finalize if still pending; ignore 'not found'
            if let Err(e) = complete_ft_upload(&upload_id).await {
                debug!("complete_upload on close ({}): {}", upload_id, e);
            }
        })
    }));
}

/// Data channel management utilities
pub struct DataChannelManager {
    upload_prefix: String,
    ft_upload_prefix: String,
}

impl DataChannelManager {
    pub fn new() -> Self {
        Self { upload_prefix: "upload_".to_string(), ft_upload_prefix: "ft_upload_".to_string()}
    }

    /// Route data channel based on its label
    pub async fn route_data_channel(&self, channel: Arc<RTCDataChannel>, session_id: String) {
        let label = channel.label().to_string();

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
                info!("Setting up terminal data channel");
                handle_terminal_channel(channel).await;
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
            
            _ => {
                warn!("Unknown data channel type: {}", label);
            }
        }
    }

    async fn setup_rpc_channel(&self, channel: Arc<RTCDataChannel>, session_id: String) {
        // Store the RPC channel globally for this session
        // crate::webrtc::store_rpc_channel(&session.id, channel.clone()).await;
        get_global_app_state().update_session_rpc_channel(session_id.as_str(), channel.clone()).await;

        // let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let channel_clone = channel.clone();
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            // let tx = tx.clone();
            let channel = channel_clone.clone();
            Box::pin(async move {
                // if let Err(e) = tx.send(msg).await {
                //     error!("failed to send message to RPC channel: {}", e);
                // }
                tokio::spawn(PROCESSOR.handle_message(msg, channel.clone()));
            })
        }));

        // let channel_clone = channel.clone();
        // tokio::spawn(async move {
        //     let channel = channel_clone.clone();
        //     while let Some(msg) = rx.recv().await {
                
        //     }
        // });

        // When the RPC data channel is opened, trigger state updates so
        // events are sent only after the channel is ready and stored
        channel.on_open(Box::new(move || {
            Box::pin(async move {
                info!("RPC channel opened - triggering state updates");
                // crate::webrtc::trigger_ota_state_update().await;
                crate::webrtc::trigger_video_state_update().await;
                crate::webrtc::trigger_usb_state_update().await;
                crate::webrtc::trigger_keyboard_led_state_update().await;

                // Also emit session count update now that the channel is open
                crate::webrtc::on_active_sessions_changed().await;

                // crate::jsonrpc::broadcast_virtual_cm_state(crate::module::VirtualCMState { camera: true, microphone: true }).await;
            })
        }));
    }

    async fn setup_input_channel(&self, channel: Arc<RTCDataChannel>, _session_id: String) {
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            Box::pin(async move {
                PROCESSOR.handle_input_message(msg).await;
            })
        }));
    }

    async fn setup_disk_channel(&self, channel: Arc<RTCDataChannel>, session_id: String) {

        // On open: install sender and bridge
        let ch_for_open = channel.clone();
        channel.on_open(Box::new(move || {
            let ch = ch_for_open.clone();
            Box::pin(async move {
                let rt = tokio::runtime::Handle::current();
                remote_mount::webrtc_disk_set_sender(Arc::new(move |text: &str| {
                    let ch = ch.clone();
                    let text = text.to_string();
                    rt.spawn(async move {
                        let _ = ch.send_text(text).await.map_err(|e| anyhow!(e.to_string()));
                    });
                    Ok(())
                }));
                remote_mount::install_webrtc_disk_bridge();
                info!("Disk data channel opened and bridge installed");
            })
        }));

        // Forward inbound data to remote_mount
        channel.on_message(Box::new(move |msg: DataChannelMessage| {
            let session_id = session_id.clone();
            Box::pin(async move {
                debug!("disk msg {} bytes (session={})", msg.data.len(), session_id);
                remote_mount::webrtc_disk_on_message(&msg.data);
            })
        }));

        // On close: clear sender and handler
        channel.on_close(Box::new(move || {
            Box::pin(async move {
                info!("Disk data channel closed");
                remote_mount::webrtc_disk_clear_sender();
                set_webrtc_read_handler(None);
            })
        }));
    }
}

impl Default for DataChannelManager {
    fn default() -> Self {
        Self::new()
    }
}
