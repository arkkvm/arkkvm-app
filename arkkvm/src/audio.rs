use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::anyhow;
use bytes::Bytes;
use parking_lot::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn, error};
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::config::get_config_manager;
use crate::zenoh_bus;

static AUDIO_SINK: tokio::sync::RwLock<Option<Arc<TrackLocalStaticSample>>> =
    tokio::sync::RwLock::const_new(None);

lazy_static::lazy_static! {
    pub static ref AUDIO_RUNNING: AtomicBool = AtomicBool::new(false);
    static ref AUDIO_HANDLE: Arc<RwLock<Option<JoinHandle<()>>>> = Arc::new(RwLock::new(None));
}

pub async fn attach_webrtc_sink(track: Arc<TrackLocalStaticSample>) {
    *AUDIO_SINK.write().await = Some(track);
}

pub async fn detach_webrtc_sink() {
    *AUDIO_SINK.write().await = None;
}

pub fn init_native_audio() -> anyhow::Result<()> {
    // do nothing
    Ok(())
}

pub async fn shutdown_native_audio() {
    info!("Shutting down audio engine");
    stop_native_audio().await;

    // Clear video sink
    detach_webrtc_sink().await;
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
        info!("Starting audio stream");

        loop {
            if !AUDIO_RUNNING.load(Ordering::Acquire) {
                break;
            }

            let reply = match subscriber.recv_timeout(Duration::from_millis(21)) {
                Ok(Some(data)) => data,
                Ok(None) => continue,
                Err(e) => {
                    error!("Failed to receive audio frame: {:?}", e);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                },
            };
            
            let Some(track) = AUDIO_SINK.read().await.clone() else {
                continue;
            };

            let payload = reply.payload();
            let payload_bytes = payload.to_bytes();
            
            // Parse packet: [8 bytes timestamp][audio data]
            // Support backward compatibility with old format (no timestamp)
            let (audio_data, capture_time) = if payload_bytes.len() >= 8 {
                // New format: extract timestamp and audio data
                match payload_bytes[0..8].try_into() {
                    Ok(timestamp_bytes) => {
                        let timestamp_us = u64::from_le_bytes(timestamp_bytes);
                        let audio_data = &payload_bytes[8..];
                        
                        if audio_data.is_empty() {
                            warn!("Received audio frame with timestamp but no audio data");
                            continue;
                        }
                        
                        // Log timestamp for debugging (optional, can be removed in production)
                        let capture_time = UNIX_EPOCH + Duration::from_micros(timestamp_us);
                        debug!("Received audio frame with capture timestamp: {:?}", &capture_time);
                        
                        (audio_data, capture_time)
                    }
                    Err(_) => {
                        warn!("Failed to extract timestamp bytes, treating as old format");
                        continue;
                    }
                }
            } else {
                // Old format: entire payload is audio data
                debug!("Received audio frame in old format (no timestamp), len: {}", payload_bytes.len());
                continue;
            };
            
            // TODO: Use timestamp_us for WebRTC synchronization when webrtc-rs supports it
            // For now, we just pass the audio data to write_sample
            
            let _ = track
                    .write_sample(&Sample {
                        data: Bytes::copy_from_slice(audio_data),
                        duration: Duration::from_millis(20),
                        timestamp: capture_time,
                        ..Default::default()
                    })
                    .await;
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