use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use opusic_c::{Channels, Decoder, SampleRate};
use parking_lot::Mutex;
use tracing::{info, warn};
use zenoh::sample::Sample;

use crate::config::get_config_manager;
use crate::hardware::usb::mic::AudioOutput;
use crate::zenoh_bus;

const UNINIT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const UNINIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

lazy_static::lazy_static! {
    static ref RUNNING: AtomicBool = AtomicBool::new(false);
    static ref NEED_STOP: AtomicBool = AtomicBool::new(false);
}

static AUDIO_OUTPUT: OnceLock<Mutex<Option<Arc<AudioOutput>>>> = OnceLock::new();

fn audio_output_slot() -> &'static Mutex<Option<Arc<AudioOutput>>> {
    AUDIO_OUTPUT.get_or_init(|| Mutex::new(None))
}

/// Start the virtual mic pipeline. Callers that apply a new USB device state
/// (e.g. `reboot_usb_manager`) must use this directly — config is persisted only
/// after the full apply succeeds, so reading saved config here would be stale.
pub async fn init() -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    service_main().await
}

/// Startup only: honor persisted `microphone_emulation`.
pub async fn init_from_saved_config() -> anyhow::Result<()> {
    let config = get_config_manager();
    if !config.get_emulation_microphone().await {
        return Ok(());
    }
    init().await
}

pub async fn uninit() -> anyhow::Result<()> {
    NEED_STOP.store(true, Ordering::Release);

    if let Some(output) = audio_output_slot().lock().take() {
        output.shutdown();
    }

    let deadline = tokio::time::Instant::now() + UNINIT_WAIT_TIMEOUT;
    while RUNNING.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            warn!("virtual mic uninit timed out waiting for pipeline to stop");
            break;
        }
        tokio::time::sleep(UNINIT_POLL_INTERVAL).await;
    }

    Ok(())
}

async fn service_main() -> anyhow::Result<()> {
    info!("Virtual Mic Service Starting");
    if RUNNING.load(Ordering::Acquire) {
        if audio_output_slot().lock().is_some() {
            return Ok(());
        }
        anyhow::bail!("virtual mic service is still shutting down");
    }

    RUNNING.store(true, Ordering::Release);
    NEED_STOP.store(false, Ordering::Release);

    let output = Arc::new(AudioOutput::new());
    *audio_output_slot().lock() = Some(Arc::clone(&output));

    let (tx, rx) = std::sync::mpsc::channel();

    tokio::spawn(async move {
        let session = zenoh_bus::get_session();

        let subscriber = session
            .declare_subscriber("webrtc/audio/mic")
            .await
            .expect("Failed to declare subscriber");

        info!("virtual miv pipline has started");
        loop {
            if NEED_STOP.load(Ordering::Acquire) {
                info!("Virtual Mic Service Stopping");
                break;
            }

            let sample = match subscriber.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(data)) => data,
                Ok(None) => continue,
                Err(e) => {
                    warn!("Failed to receive sample: {:?}", e);
                    break;
                }
            };

            if let Err(e) = tx.send(sample) {
                warn!("Failed to send webrtc virtual mic message: {:?}", e);
                break;
            }
        }
        info!("virtual miv pipline has stoped");
    });

    let decode_output = Arc::clone(&output);
    tokio::task::spawn_blocking(move || run_virtual_mic(rx, decode_output));

    Ok(())
}

fn run_virtual_mic(rx: std::sync::mpsc::Receiver<Sample>, device: Arc<AudioOutput>) {
    let sample_rate = SampleRate::Hz48000;
    let channels = Channels::Stereo;

    let mut opus_decoder = match Decoder::new(channels, sample_rate) {
        Ok(decoder) => decoder,
        Err(err) => {
            eprintln!("Failed to create Opus decoder: {:?}", err);
            RUNNING.store(false, Ordering::Release);
            *audio_output_slot().lock() = None;
            return;
        }
    };
    info!("Opus decoder created");

    let mut frame_cache = [0u16; 1920];
    let mut bytes_buffer = vec![0u8; 3840];
    loop {
        match rx.recv() {
            Ok(reply) => {
                let payload = reply.payload();
                let slices = payload.slices();
                for slice in slices {
                    let _size = match opus_decoder.decode_to_slice(slice, &mut frame_cache, false) {
                        Ok(size) => size,
                        Err(e) => {
                            warn!("Failed to decode Opus frame: {:?}", e);
                            continue;
                        }
                    };
                    u16_slice_to_le_bytes_in(&frame_cache, &mut bytes_buffer);
                    device.send_data(&bytes_buffer);
                }
            }

            Err(e) => {
                warn!("Virtual Mic Error receiving packet: {:?}", e);
                break;
            }
        }
    }
    info!("Virtual Mic thread exiting");
    RUNNING.store(false, Ordering::Release);
    *audio_output_slot().lock() = None;
}

#[inline]
fn u16_slice_to_le_bytes_in(data: &[u16], dst: &mut [u8]) {
    let byte_len = data.len() * 2;
    let out = &mut dst[..byte_len];
    out.copy_from_slice(unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, byte_len)
    });
}
