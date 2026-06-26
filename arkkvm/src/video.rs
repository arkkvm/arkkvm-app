use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use bytes::Bytes;
use once_cell::sync::Lazy;
use prometheus::Encoder;
use serde_json as serde_json_crate;
use tokio::sync::{RwLock, mpsc};
// use tokio::time::Instant;
use tracing::{debug, info, warn, error};
use webrtc::media::Sample;
use webrtc::track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample};
use anyhow::anyhow;

use crate::config::get_config_manager;
use crate::jsonrpc::PROCESSOR;
use crate::hardware::hdmi::HdmiCapture;

/// Video input state
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VideoInputState {
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>, // no_signal, no_lock, out_of_range
    pub width: i32,
    pub height: i32,
    pub offset_x: f32,
    pub offset_y: f32,
    #[serde(rename = "fps")]
    pub frame_per_second: f64,
}

/// Last video state
static LAST_VIDEO_STATE: RwLock<VideoInputState> = RwLock::const_new(VideoInputState {
    ready: false,
    error: None,
    width: 0,
    height: 0,
    offset_x: 0.0,
    offset_y: 0.0,
    frame_per_second: 0.0,
});

// ----- Native video FFI bridge (authoritative path) -----

static VIDEO_SINK: tokio::sync::RwLock<Option<Arc<TrackLocalStaticSample>>> =
    tokio::sync::RwLock::const_new(None);
type FramePacket = (Bytes, u64, f64);
static VIDEO_FRAME_TX: tokio::sync::OnceCell<mpsc::Sender<FramePacket>> =
    tokio::sync::OnceCell::const_new();

// FFI pipeline starter (for direct frame ingress)
static VIDEO_PIPELINE_STARTED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

static VIDEO_STATE_TX: OnceLock<mpsc::UnboundedSender<VideoInputState>> = OnceLock::new();

// Prometheus drop counter
static VIDEO_DROP_COUNTER: Lazy<prometheus::IntCounter> = Lazy::new(|| {
    prometheus::register_int_counter!(
        "arkkvm_video_frame_drops_total",
        "Total number of dropped video frames in ArkKVM pipeline"
    )
    .expect("register arkkvm_video_frame_drops_total")
});

pub async fn init_video_state_updater() -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<VideoInputState>();
    VIDEO_STATE_TX.set(tx).map_err(|_| anyhow::anyhow!("video state tx already set"))?;

    tokio::spawn(async move {
        while let Some(st) = rx.recv().await {
            *LAST_VIDEO_STATE.write().await = st.clone();
            info!(
                "Video state updated: {}x{} @ {:.2}fps, ready={}, error: {:?}",
                st.width, st.height, st.frame_per_second, st.ready, st.error
            );
            trigger_video_state_update_rpc().await;
        }
    });

    Ok(())
}

pub async fn ensure_video_pipeline_started() -> anyhow::Result<()> {
    VIDEO_PIPELINE_STARTED
        .get_or_try_init(|| async {
            let (tx, rx) = mpsc::channel::<FramePacket>(32);
            let _ = VIDEO_FRAME_TX.set(tx);
            
            tokio::spawn(video_frame_writer(rx));

            Ok(())
        })
        .await
        .map(|_| ())
}

pub async fn attach_webrtc_sink(track: Arc<TrackLocalStaticSample>) {
    *VIDEO_SINK.write().await = Some(track);
    info!("video webrtc sink attached");
}

pub async fn detach_webrtc_sink() {
    *VIDEO_SINK.write().await = None;
    info!("video webrtc sink detached");
}

pub async fn equal_webrtc_sink(track: Arc<TrackLocalStaticSample>) -> bool {
    if let Some(sink) = VIDEO_SINK.read().await.as_ref() {
        sink.id() == track.id()
    }
    else {
        false
    }
}

async fn video_frame_writer(mut rx: mpsc::Receiver<FramePacket>) {
    const VIDEO_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
    const VIDEO_WRITE_FAIL_DETACH_THRESHOLD: u32 = 3;
    let mut consecutive_write_failures: u32 = 0;

    // let mut last_ts: Option<u64> = None;
    // let mut last_time: Option<Instant> = None;
    while let Some((data, pts_us, fps)) = rx.recv().await {
        // let duration = match last_ts {
        //     Some(prev) => Duration::from_micros(pts_us.saturating_sub(prev)),
        //     None => Duration::from_millis((1000.0 / fps) as u64),
        // };

        // let duration = if let Some(last_time) = last_time {
        //     last_time.elapsed()
        // }
        // else {
        //     Duration::from_millis(0)
        // };
        // last_time = Some(Instant::now());

        let mut should_detach_sink = false;
        if let Some(track) = VIDEO_SINK.read().await.clone() {
            // data is already Bytes, no conversion needed
            let capture_time = UNIX_EPOCH + Duration::from_micros(pts_us);
            let sample = Sample { data, duration: Duration::from_millis((1000.0 / fps) as u64), timestamp: capture_time, ..Default::default() };
            match tokio::time::timeout(VIDEO_WRITE_TIMEOUT, track.write_sample(&sample)).await {
                Ok(Ok(())) => {
                    consecutive_write_failures = 0;
                }
                Ok(Err(e)) => {
                    consecutive_write_failures = consecutive_write_failures.saturating_add(1);
                    warn!("error writing video sample: {}", e);
                    VIDEO_DROP_COUNTER.inc();
                }
                Err(_) => {
                    consecutive_write_failures = consecutive_write_failures.saturating_add(1);
                    warn!(
                        "timed out writing video sample after {:?}",
                        VIDEO_WRITE_TIMEOUT
                    );
                    VIDEO_DROP_COUNTER.inc();
                }
            }

            if consecutive_write_failures > VIDEO_WRITE_FAIL_DETACH_THRESHOLD {
                warn!(
                    "detaching video sink after {} consecutive write failures",
                    consecutive_write_failures
                );
                should_detach_sink = true;
                consecutive_write_failures = 0;
            }
        }

        if should_detach_sink {
            detach_webrtc_sink().await;
        }

        // last_ts = Some(pts_us);
    }
    error!("video frame writer stopped");
}

pub fn on_frame_received(data: &[u8], pts_us: u64, fps: f64) {
    if let Some(tx) = VIDEO_FRAME_TX.get() {
        if let Err(e) = tx.try_send((Bytes::copy_from_slice(data), pts_us, fps)) {
            // warn!("Failed to send video frame: {:?}", e);
            VIDEO_DROP_COUNTER.inc();
        }
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// This function is called from C code via FFI and is therefore `unsafe`.
/// The caller must ensure the following preconditions are met:
/// - `error` is either a null pointer or a valid NUL-terminated C string
///   that remains valid for the duration of this call.
/// - The values of `width`, `height`, and `fps` reflect the current detected
///   video format and are consistent with the produced H.264 stream.
/// - The function may be called from non-Rust threads; it must not be called
///   after the Rust runtime has been torn down.
///
/// Inside this function we immediately convert the C string (if present) into
/// an owned `String` and then forward an owned `VideoInputState` via an
/// unbounded channel, avoiding holding raw pointers across threads.
pub unsafe extern "C" fn arkkvm_on_video_state_changed(
    ready: i32,
    width: u16,
    height: u16,
    offset_x: f32,
    offset_y: f32,
    fps: f64,
    error: *const std::os::raw::c_char,
) {
    let error_str = if error.is_null() {
        None
    } else {
        let c_str = unsafe { std::ffi::CStr::from_ptr(error) };
        c_str.to_str().ok().map(|s| s.to_string())
    };

    let st = VideoInputState {
        ready: ready != 0,
        error: error_str,
        width: width as i32,
        height: height as i32,
        offset_x,
        offset_y,
        frame_per_second: fps,
    };
    info!("arkkvm_on_video_state_changed state: {:?}", &st);
    if let Some(tx) = VIDEO_STATE_TX.get() {
        let _ = tx.send(st);
    } else {
        debug!("video state updater not initialized");
    }
}

// Rust video pipeline (replaces C implementation)
static HDMI_CAPTURE: OnceLock<Arc<tokio::sync::RwLock<Option<Arc<HdmiCapture>>>>> = OnceLock::new();
static VIDEO_MONITORING_RUNNING: AtomicBool = AtomicBool::new(false);
static VIDEO_MONITORING_STATUS: AtomicBool = AtomicBool::new(false);

fn get_hdmi_capture() -> Arc<tokio::sync::RwLock<Option<Arc<HdmiCapture>>>> {
    HDMI_CAPTURE.get_or_init(|| Arc::new(tokio::sync::RwLock::new(None))).clone()
}

fn start_video_monitoring_task() {
    VIDEO_MONITORING_STATUS.store(true, Ordering::Relaxed);
    if VIDEO_MONITORING_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    VIDEO_MONITORING_RUNNING.store(true, Ordering::Relaxed);
    // Spawn video monitoring task
    tokio::spawn(async move {
        info!("Video monitoring task started");
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
        let mut last_metrics: Option<String> = None;
        loop {
            interval.tick().await;

            if !VIDEO_MONITORING_STATUS.load(Ordering::Relaxed) {
                break;
            }

            if let Ok(stats) = get_video_stats().await {
                debug!("Video stats: {:?}", stats);
            }

            let metrics = video_metrics_snapshot();
            if !metrics.trim().is_empty() {
                // Only log metrics when they change
                if last_metrics.as_ref().map(|m| m != &metrics).unwrap_or(true) {
                    warn!("Prometheus metrics:\n{}", metrics);
                    last_metrics = Some(metrics);
                }
            }
        }
        VIDEO_MONITORING_RUNNING.store(false, Ordering::Relaxed);
        info!("Video monitoring task stopped");
    });
}

pub async fn start_native_video() -> anyhow::Result<()> {
    ensure_video_pipeline_started().await?;
    
    let capture_arc = get_hdmi_capture();
    let mut capture_guard = capture_arc.write().await;
    
    if capture_guard.is_some() {
        warn!("Video capture already started");
        return Ok(());
    }
    
    let capture = Arc::new(HdmiCapture::new());
    capture.set_quality(get_config_manager().get_video_quality().await).await?;
    capture.init().await?;
    HdmiCapture::start_hpd_detection(capture.clone()).await?;
    *capture_guard = Some(capture);
    
    start_video_monitoring_task();

    Ok(())
}

pub async fn stop_native_video() {
    let capture_arc = get_hdmi_capture();
    let mut capture_guard = capture_arc.write().await;
    
    if let Some(capture) = capture_guard.as_ref() {
        capture.shutdown().await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        *capture_guard = None;
    }

    VIDEO_MONITORING_STATUS.store(false, Ordering::Relaxed);
    info!("Video pipeline shutdown complete");
}

pub async fn shutdown_video_pipeline() {
    stop_native_video().await;
    detach_webrtc_sink().await;
}

/// Update video quality dynamically
pub async fn update_video_quality(quality: f32) -> anyhow::Result<()> {
    if !(0.0..=1.0).contains(&quality) {
        return Err(anyhow!("Quality must be between 0.0 and 1.0"));
    }

    get_config_manager().set_video_quality(quality).await?;

    let capture_arc = get_hdmi_capture();
    let capture_guard = capture_arc.read().await;
    if let Some(capture) = capture_guard.as_ref() {
        capture.set_quality(quality).await?;
    }
    
    Ok(())
}

pub async fn get_video_quality() -> f32 {
    let capture_arc = get_hdmi_capture();
    let capture_guard = capture_arc.read().await;
    if let Some(capture) = capture_guard.as_ref() {
        capture.get_quality().await
    }
    else {
        get_config_manager().get_video_quality().await
    }
}

/// Get video pipeline statistics
pub async fn get_video_stats() -> anyhow::Result<VideoStats> {
    let state = get_video_state().await;
    let stats = VideoStats {
        ready: state.ready,
        error: state.error.clone(),
        width: state.width,
        height: state.height,
        fps: state.frame_per_second,
        has_sink: VIDEO_SINK.read().await.is_some(),
        pipeline_started: VIDEO_PIPELINE_STARTED.get().is_some(),
    };
    Ok(stats)
}

/// Video pipeline statistics
#[derive(Debug, Clone, serde::Serialize)]
pub struct VideoStats {
    pub ready: bool,
    pub error: Option<String>,
    pub width: i32,
    pub height: i32,
    pub fps: f64,
    pub has_sink: bool,
    pub pipeline_started: bool,
}

/// Get current video state
pub async fn get_video_state() -> VideoInputState {
    LAST_VIDEO_STATE.read().await.clone()
}

/// Update video state
// pub async fn handle_video_state_message(video_state: VideoInputState) {
//     *LAST_VIDEO_STATE.write().await = video_state.clone();
//     trigger_video_state_update_rpc().await;
//     // Update LVGL display to reflect new video state
//     let _ = crate::hardware::display::request_display_update(true).await;
// }

/// Public interface for video state update (called from webrtc.rs or native events)
pub async fn trigger_video_state_update_rpc() {
    let video_state = LAST_VIDEO_STATE.read().await.clone();
    info!("Triggering video state update: {:?}", video_state);

    if let Some(session) = crate::web::get_global_app_state().get_current_session().await {
        // let mut session = crate::session::Session::new(session_id.clone());
        // if let Some(rpc_channel) = crate::webrtc::get_rpc_channel(&session_id).await {
        //     // Optional: check channel open state like webrtc.rs does
        //     if rpc_channel.ready_state()
        //         != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
        //     {
        //         // Avoid spamming logs on normal race
        //         return;
        //     }
        //     session.rpc_channel = Some(rpc_channel);
        // } else {
        //     return;
        // }

        let params = match serde_json_crate::to_value(&video_state) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("Failed to serialize video state: {}", e);
                None
            }
        };
        if let Err(e) = PROCESSOR.send_event("videoInputState", params, session).await {
            warn!("Failed to send videoInputState event: {}", e);
        }
    }
}

pub fn video_metrics_snapshot() -> String {
    let metric_families = prometheus::gather();
    let mut buf = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        tracing::warn!("Failed to encode metrics: {}", e);
        return "# ERROR: Failed to encode metrics\n".to_string();
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Write control action
pub async fn write_ctrl_action(action: &str) -> anyhow::Result<()> {
    use crate::hardware::native::socket::call_ctrl_action;
    info!("Writing control action: {}", serde_json::json!({ "action": action }));
    let _ = call_ctrl_action(action, None).await?;
    Ok(())
}
