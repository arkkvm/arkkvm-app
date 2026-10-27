use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use once_cell::sync::Lazy;
use parking_lot::RwLock as ParkingRwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn, error};
use rtc::media::Sample;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

use crate::config::get_config_manager;
use crate::session::SessionRole;
use crate::zenoh_bus;

struct SinkEntry {
    session_id: String,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
}

static AUDIO_SINK: Lazy<tokio::sync::RwLock<HashMap<SessionRole, SinkEntry>>> =
    Lazy::new(|| tokio::sync::RwLock::new(HashMap::new()));

lazy_static::lazy_static! {
    pub static ref AUDIO_RUNNING: AtomicBool = AtomicBool::new(false);
    static ref AUDIO_HANDLE: Arc<ParkingRwLock<Option<JoinHandle<()>>>> = Arc::new(ParkingRwLock::new(None));
}

pub async fn attach_webrtc_sink(
    role: SessionRole,
    session_id: String,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
) {
    AUDIO_SINK.write().await.insert(
        role,
        SinkEntry {
            session_id: session_id.clone(),
            track,
            ssrc,
            payload_type,
        },
    );
    info!("audio webrtc sink attached for role {} session {}", role, session_id);
}

pub async fn detach_if_session(role: SessionRole, session_id: &str) {
    let mut sinks = AUDIO_SINK.write().await;
    let should_remove = sinks
        .get(&role)
        .is_some_and(|entry| entry.session_id == session_id);
    if should_remove {
        sinks.remove(&role);
        info!(
            "audio webrtc sink detached for role {} session {}",
            role, session_id
        );
    }
}

pub async fn detach_webrtc_sink(role: SessionRole) {
    if AUDIO_SINK.write().await.remove(&role).is_some() {
        info!("audio webrtc sink detached for role {}", role);
    }
}

pub async fn detach_all_webrtc_sinks() {
    AUDIO_SINK.write().await.clear();
    info!("all audio webrtc sinks detached");
}

pub fn init_native_audio() -> anyhow::Result<()> {
    // do nothing
    Ok(())
}

pub async fn shutdown_native_audio() {
    info!("Shutting down audio engine");
    stop_native_audio().await;

    detach_all_webrtc_sinks().await;
    info!("Audio Engine shutdown complete");
}

// Bridge zenoh audio to WebRTC
// 1. Hardware service captures audio and publishes on zenoh
//    RKMPI: "hdmirx/audio/pcm"
//    OPUS "hdmirx/audio/opus"
// 2. Subscribe to zenoh topic "hdmirx/audio/opus"
pub async fn start_native_audio() -> anyhow::Result<()> {
    let config = get_config_manager();
    if !config.get_emulation_audio_playback().await {
        warn!("Audio playback is disabled");
        return Err(anyhow!("Audio playback is disabled"));
    }

    if AUDIO_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }

    let session = zenoh_bus::get_session();

    let subscriber = session
        .declare_subscriber("hdmirx/audio/opus")
        .await
        .expect("Failed to declare subscriber");

    let handle = tokio::spawn(async move {
        const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(20);
        const AUDIO_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

        info!("Starting audio stream");

        loop {
            let reply = tokio::select! {
                biased;
                _ = wait_until_audio_stopped() => break,
                result = subscriber.recv_async() => result,
            };
            let reply = match reply {
                Ok(data) => data,
                Err(e) => {
                    error!("Failed to receive audio frame: {:?}", e);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };

            let entries: Vec<(SessionRole, String, Arc<TrackLocalStaticSample>, u32, u8)> = {
                let sinks = AUDIO_SINK.read().await;
                if sinks.is_empty() {
                    continue;
                }
                sinks
                    .iter()
                    .map(|(role, entry)| {
                        (
                            *role,
                            entry.session_id.clone(),
                            entry.track.clone(),
                            entry.ssrc,
                            entry.payload_type,
                        )
                    })
                    .collect()
            };

            let payload = reply.payload();
            let payload_bytes = payload.to_bytes();

            // Parse packet: [8 bytes timestamp][audio data]
            let audio_data = if payload_bytes.len() >= 8 {
                match payload_bytes[0..8].try_into() {
                    Ok(timestamp_bytes) => {
                        let timestamp_us = u64::from_le_bytes(timestamp_bytes);
                        let audio_data = &payload_bytes[8..];

                        if audio_data.is_empty() {
                            warn!("Received audio frame with timestamp but no audio data");
                            continue;
                        }

                        debug!("Received audio frame with capture timestamp: {}", timestamp_us);

                        audio_data
                    }
                    Err(_) => {
                        warn!("Failed to extract timestamp bytes, treating as old format");
                        continue;
                    }
                }
            } else {
                debug!("Received audio frame in old format (no timestamp), len: {}", payload_bytes.len());
                continue;
            };

            let sample_data = Bytes::copy_from_slice(audio_data);
            for (role, session_id, track, ssrc, payload_type) in entries {
                let sample = Sample {
                    data: sample_data.clone(),
                    duration: AUDIO_FRAME_DURATION,
                    ..Default::default()
                };
                let write_ok = match tokio::time::timeout(
                    AUDIO_WRITE_TIMEOUT,
                    track.write_sample(ssrc, payload_type, &sample, &[]),
                )
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        warn!("error writing audio sample for role {}: {}", role, e);
                        false
                    }
                    Err(_) => {
                        warn!(
                            "timed out writing audio sample for role {} after {:?}",
                            role, AUDIO_WRITE_TIMEOUT
                        );
                        false
                    }
                };
                if !write_ok {
                    detach_if_session(role, &session_id).await;
                }
            }
        }
        info!("Audio track piping finished");
    });
    *AUDIO_HANDLE.write() = Some(handle);

    Ok(())
}

pub async fn stop_native_audio() {
    AUDIO_RUNNING.store(false, Ordering::Release);
    let handle = AUDIO_HANDLE.write().take();
    if let Some(handle) = handle {
        match tokio::time::timeout(Duration::from_secs(2), handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Audio task join failed: {}", e),
            Err(_) => warn!("Timed out waiting audio task to stop"),
        }
    }
    info!("Audio engine stopped")
}

async fn wait_until_audio_stopped() {
    while AUDIO_RUNNING.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn get_webrtc_clock_rate() -> u32 {
    // Get detected audio sample rate from audio service (already stored globally)
    // WebRTC Opus codec supports: 8000, 12000, 16000, 24000, 48000 Hz
    // If detected rate is 44100, we'll use 48000 as Opus doesn't support 44100
    let detected_rate = crate::services::audio::get_detected_sample_rate().unwrap_or(48000);
    let webrtc_clock_rate = match detected_rate {
        8000 | 12000 | 16000 | 24000 | 48000 => detected_rate,
        44100 => {
            warn!("get_webrtc_clock_rate Detected 44100 Hz but WebRTC Opus doesn't support it, using 48000 Hz");
            48000
        }
        _ => {
            warn!("get_webrtc_clock_rate Unsupported sample rate: {} Hz, using 48000 Hz for WebRTC", detected_rate);
            48000
        }
    };
    info!("get_webrtc_clock_rate Using audio sample rate: {} Hz (detected: {} Hz) for WebRTC track", 
          webrtc_clock_rate, detected_rate);
    webrtc_clock_rate
}