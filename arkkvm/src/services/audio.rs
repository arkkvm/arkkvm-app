use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{mem, slice, sync::OnceLock};

use atomic_float::AtomicF32;
use opusic_c::{Application, Bitrate, Channels, Encoder, SampleRate};
use tokio::runtime::Handle;
use tracing::{debug, error, info, warn};
use triple_buffer::triple_buffer;
use zenoh::bytes::ZBytes;

use crate::audio::AUDIO_RUNNING;
use crate::config::get_config_manager;
use crate::hardware::hdmi::rk628_hpd;
use crate::hardware::mpi;
use crate::zenoh_bus;

const DEFAULT_AUDIO_BITRATE: u32 = 128000;

/// Frame shared between capture (producer) and encode (consumer) via triple buffer.
/// Capture writes in-place and publish; encode reads latest by ref. Zero-copy, drop when behind.
#[derive(Clone)]
struct AudioFrame {
    data: Vec<u8>,
    capture_timestamp_us: u64,
}

lazy_static::lazy_static! {
    static ref DETECTED_SAMPLE_RATE: OnceLock<u32> = OnceLock::new();
    static ref AUDIO_QUALITY: AtomicF32 = AtomicF32::new(1.0);
    static ref ENCODER_CHANGED: AtomicBool = AtomicBool::new(false);
}

/// Get the detected audio sample rate from RK628
/// Returns the sample rate in Hz, or None if not yet detected
pub fn get_detected_sample_rate() -> Option<u32> {
    DETECTED_SAMPLE_RATE.get().copied()
}

/// Convert u32 sample rate to Opus SampleRate enum
fn u32_to_sample_rate(rate: &u32) -> SampleRate {
    match rate {
        8000 => SampleRate::Hz8000,
        12000 => SampleRate::Hz12000,
        16000 => SampleRate::Hz16000,
        24000 => SampleRate::Hz24000,
        48000 => SampleRate::Hz48000,
        _ => {
            warn!(
                "u32_to_sample_rate, Unsupported sample rate: {} Hz, falling back to 48000 Hz",
                rate
            );
            SampleRate::Hz48000
        }
    }
}

/// Convert u32 sample rate to Rockchip MPI sample rate enum
/// Returns the enum value as u32, defaults to 48000 if unsupported
///
/// Supported rates: 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 96000
/// The enum values directly correspond to the sample rate values
fn u32_to_mpi_sample_rate(rate: u32) -> u32 {
    match rate {
        8000 | 12000 | 16000 | 24000 | 48000 => rate,
        11025 | 22050 | 32000 | 44100 | 64000 | 96000 => {
            warn!(
                "u32_to_mpi_sample_rate, The opus can not support this sample rate: {} Hz, falling back to 48000 Hz",
                rate
            );
            48000
        }
        _ => {
            warn!(
                "u32_to_mpi_sample_rate, Unsupported MPI sample rate: {} Hz, falling back to 48000 Hz",
                rate
            );
            48000
        }
    }
}

/// Calculate samples per frame for a given sample rate
///
/// Frame duration is typically 20ms for audio processing
/// Returns samples per channel for the frame
fn calculate_samples_per_frame(sample_rate: u32, frame_duration_ms: u32) -> u32 {
    ((sample_rate * frame_duration_ms) / 1000) * 2
}

pub async fn init(quality: f32) -> anyhow::Result<()> {
    service_main(quality);
    Ok(())
}

/// Capture audio task: continuously captures into triple buffer (in-place write + publish).
/// Encode task reads latest from triple buffer; when behind, old frames are dropped (no blocking).
fn capture_audio_task(
    bitrate: Option<u32>,
    tokio_handle: Handle,
) -> anyhow::Result<()> {
    loop {
        if !AUDIO_RUNNING.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        AudioMPI::init()?;

        let initial = AudioFrame {
            data: Vec::with_capacity(4000),
            capture_timestamp_us: 0,
        };
        let (mut buf_input, buf_output) = triple_buffer(&initial);
        let encode_handle = tokio_handle.spawn(encode_audio_task(buf_output, bitrate));

        info!("Starting audio capture task (triple buffer)");
        loop {
            if !AUDIO_RUNNING.load(Ordering::Acquire) {
                break;
            }

            {
                let input = buf_input.input_buffer();
                input.data.clear();
                if let Err(e) = AudioMPI::get_audio_frame(|frame_data| {
                    input.data.extend_from_slice(frame_data);
                }) {
                    warn!("Failed to get audio frame: {:?}", e);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                input.capture_timestamp_us = get_timestamp_us();
            }
            buf_input.publish();
        }

        encode_handle.abort();
        std::thread::sleep(std::time::Duration::from_millis(100));
        AudioMPI::un_init()?;
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }
    info!("Audio capture task finished");
    Ok(())
}

/// Encode audio task: reads latest frame from triple buffer (by ref), encodes, publishes to zenoh.
/// When behind capture, only latest frame is encoded (drop old frames, no blocking).
/// Owns Output (no Arc/Mutex): single consumer, moved in at spawn.
async fn encode_audio_task(
    mut buf_output: triple_buffer::Output<AudioFrame>,
    bitrate: Option<u32>,
) {
    let session = zenoh_bus::get_session();
    let sample_rate_hz = DETECTED_SAMPLE_RATE.get().unwrap_or(&48000);
    let sample_rate = u32_to_sample_rate(sample_rate_hz);
    let channels = Channels::Stereo;
    let application = Application::Audio;

    info!("Using detected sample rate: {:?} Hz for Opus encoder", &sample_rate);

    let mut opus_encoder = Encoder::new(channels, sample_rate, application)
        .expect("Failed to create opus encoder");
    // opus_encoder.set_inband_fec(InbandFec::Mode1).expect("Failed to set inband fec");
    opus_encoder.set_vbr(true).expect("Failed to set vbr");
    opus_encoder.set_vbr_constraint(true).expect("Failed to set vbr constraint");
    if let Some(bitrate) = bitrate {
        opus_encoder.set_bitrate(Bitrate::Value(bitrate)).expect("Failed to set bitrate");
    } else {
        opus_encoder.set_bitrate(Bitrate::Auto).expect("Failed to set bitrate");
    }

    let mut opus_buf = vec![0u8; 4000];
    let mut audio_u16s = Vec::<u16>::with_capacity(2000);
    let mut packet_buf = Vec::<u8>::with_capacity(4096);
    /// 20ms period (50 fps). Advance next_tick only after consuming a frame so we stay aligned with upstream.
    const FRAME_PERIOD: Duration = Duration::from_millis(20);
    const POLL_INTERVAL: Duration = Duration::from_millis(1);
    let mut next_tick = tokio::time::Instant::now();

    info!("Starting audio encode task (triple buffer)");
    loop {
        if !AUDIO_RUNNING.load(Ordering::Acquire) {
            break;
        }

        if ENCODER_CHANGED.load(Ordering::Acquire) {
            let quality = AUDIO_QUALITY.load(Ordering::Acquire);
            let bitrate = get_audio_bitrate(quality);
            info!("Encoder bitrate changed, setting audio quality to {}, bitrate to {} kbps", quality, bitrate / 1000);
            opus_encoder.set_bitrate(Bitrate::Value(bitrate)).expect("Failed to set bitrate");
            ENCODER_CHANGED.store(false, Ordering::Release);
        }

        tokio::time::sleep_until(next_tick).await;

        if !buf_output.update() {
            next_tick = tokio::time::Instant::now() + POLL_INTERVAL;
            continue;
        }

        let frame = buf_output.output_buffer();
        if frame.data.is_empty() {
            next_tick = tokio::time::Instant::now() + POLL_INTERVAL;
            continue;
        }
        next_tick += FRAME_PERIOD;

        let capture_timestamp_us = frame.capture_timestamp_us;
        convert_u8_to_u16_slice_reuse(frame.data.as_slice(), &mut audio_u16s);

        let opus_buf_len = match opus_encoder.encode_to_slice(&audio_u16s, &mut opus_buf) {
            Ok(len) => len,
            Err(e) => {
                warn!("Failed to encode audio frame: {:?}", e);
                next_tick = tokio::time::Instant::now() + POLL_INTERVAL;
                continue;
            }
        };

        packet_buf.clear();
        packet_buf.extend_from_slice(&capture_timestamp_us.to_le_bytes());
        packet_buf.extend_from_slice(&opus_buf[..opus_buf_len]);

        if let Err(e) = session.put("hdmirx/audio/opus", ZBytes::from(packet_buf.clone())).await {
            warn!("Failed to publish audio frame to zenoh: {:?}", e);
        }
    }
    info!("Audio encode task finished");
}

fn get_timestamp_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

fn service_main(quality: f32) {
    info!("Rockchip MPI Audio Service Started");
    AUDIO_QUALITY.store(quality, Ordering::Release);
    let bitrate = get_audio_bitrate(quality);
    let tokio_handle = Handle::current();
    std::thread::spawn(move || {
        if let Err(e) = capture_audio_task(Some(bitrate), tokio_handle) {
            error!("Audio capture task error: {:?}", e);
        }
    });
}

/// Update audio quality and bitrate
/// quality: 0.0 - 1.0
/// High: 1.0 (128 kbps)
/// Medium: 0.75 (96 kbps)
/// Low: 0.375 (48 kbps)
pub async fn update_audio_quality(quality: f32) -> anyhow::Result<()> {
    if quality <= 0.0 {
        return Err(anyhow::anyhow!("Quality must be greater than 0.0"));
    }

    let manager = get_config_manager();
    if manager.get_audio_quality().await == quality {
        return Ok(());
    }
    manager.set_audio_quality(quality).await?;

    AUDIO_QUALITY.store(quality, Ordering::Release);
    ENCODER_CHANGED.store(true, Ordering::Release);
    Ok(())
}

pub fn get_audio_quality() -> f32 {
    AUDIO_QUALITY.load(Ordering::Acquire)
}

fn get_audio_bitrate(quality: f32) -> u32 {
    (DEFAULT_AUDIO_BITRATE as f32 * quality) as u32
}

struct AudioMPI {}

pub type GetFrameCallback = fn(&[u8]) -> ();

impl AudioMPI {
    pub fn init() -> anyhow::Result<()> {
        // Initialize Rockchip MPP system (using global manager to avoid multiple initializations)
        mpi::init()
            .map_err(|e| anyhow::anyhow!("Failed to initialize Rockchip MPP system: {}", e))?;

        // Get actual sample rate from RK628 HDMI device
        let sample_rate = match rk628_hpd::get_rk628_audio_rate() {
            Some(rate) => {
                let mpi_rate = u32_to_mpi_sample_rate(rate);
                info!("Detected RK628 audio sample rate: {}Hz, primitive: {}Hz", mpi_rate, rate);
                // Store the detected sample rate globally
                DETECTED_SAMPLE_RATE.set(mpi_rate).ok();
                mpi_rate
            }
            None => {
                warn!("Failed to get RK628 audio rate, using default 48000 Hz");
                let default_rate = 48000;
                DETECTED_SAMPLE_RATE.set(default_rate).ok();
                default_rate
            }
        };

        let channels = 2;

        // Initialize audio input
        let mut ai_attr: mpi::AIO_ATTR_S = unsafe { mem::zeroed() };

        debug!("sizeof(AIO_ATTR_S): {}", mem::size_of::<mpi::AIO_ATTR_S>());
        ai_attr.soundCard.channels = channels;
        ai_attr.soundCard.sampleRate = sample_rate;
        ai_attr.soundCard.bitWidth = mpi::rkAUDIO_BIT_WIDTH_E_AUDIO_BIT_WIDTH_16;
        ai_attr.enBitwidth = mpi::rkAUDIO_BIT_WIDTH_E_AUDIO_BIT_WIDTH_16;
        // Use the detected sample rate directly as enum value
        // Note: Rockchip MPI may need specific enum values, but we'll try using the rate directly
        ai_attr.enSamplerate = sample_rate;
        ai_attr.enSoundmode = mpi::rkAIO_SOUND_MODE_E_AUDIO_SOUND_MODE_STEREO;
        // Calculate samples per frame based on sample rate (20ms frame duration)
        // For stereo: samples_per_channel * channels
        let samples_per_channel = calculate_samples_per_frame(sample_rate, 20);
        ai_attr.u32PtNumPerFrm = samples_per_channel * channels;
        ai_attr.u32FrmNum = 4;
        ai_attr.u32EXFlag = 0;
        ai_attr.u32ChnCnt = 2;

        let card_name = b"hw:0,0\0";
        ai_attr.u8CardName[..card_name.len()].copy_from_slice(card_name);

        info!(
            "Calculated samples per frame: {} ({} Hz, {} channels, 20ms)",
            ai_attr.u32PtNumPerFrm, sample_rate, channels
        );

        let dev_id = 0;
        let chn_id = 0;

        mpi::ai_set_pub_attr(dev_id, &ai_attr)
            .map_err(|e| anyhow::anyhow!("Failed to set audio input attributes: {}", e))?;

        mpi::ai_enable(dev_id)
            .map_err(|e| anyhow::anyhow!("Failed to enable audio input: {}", e))?;

        let chn_param: mpi::AI_CHN_PARAM_S = mpi::AI_CHN_PARAM_S {
            s32UsrFrmDepth: 4,
            enLoopbackMode: mpi::rkAUDIO_LOOPBACK_MODE_E_AUDIO_LOOPBACK_NONE,
        };

        if let Err(e) = mpi::ai_set_chn_param(dev_id, chn_id, &chn_param) {
            mpi::ai_disable(dev_id);
            anyhow::bail!("Failed to set audio input channel parameters: {}", e);
        }

        if let Err(e) = mpi::ai_enable_chn(dev_id, chn_id) {
            mpi::ai_disable(dev_id);
            anyhow::bail!("Failed to enable audio input channel: {}", e);
        }

        if let Err(e) = mpi::ai_set_volume(dev_id, 100) {
            mpi::ai_disable(dev_id);
            anyhow::bail!("Failed to set audio input volume: {}", e);
        }
        info!("Set audio input volume success");

        Ok(())
    }

    pub fn un_init() -> anyhow::Result<()> {
        let dev_id = 0;
        let chn_id = 0;

        // disable audio input channel
        mpi::ai_disable_chn(dev_id, chn_id);

        // disable audio input device
        mpi::ai_disable(dev_id);

        info!("AudioMPI uninitialized successfully");
        Ok(())
    }

    pub fn get_audio_frame<F>(mut callback: F) -> anyhow::Result<()>
    where
        F: FnMut(&[u8]) -> (),
    {
        let dev_id = 0;
        let chn_id = 0;

        let mut frame: mpi::AUDIO_FRAME_S = unsafe { mem::zeroed() };

        // Timeout for getting data:
        // -1: blocking mode
        // >=0: non-blocking mode (milliseconds)
        let timeout_ms = -1;
        let ret = mpi::ai_get_frame(dev_id, chn_id, &mut frame, timeout_ms);

        if ret != mpi::RK_SUCCESS {
            info!("Failed to get audio input frame, error: {:#010X}", ret as u32);
        }

        let frame_data = mpi::mb_handle2viraddr(frame.pMbBlk);

        if frame_data.is_null() {
            mpi::ai_release_frame(dev_id, chn_id, &frame);
            panic!("Failed to get audio input frame, error: data is null");
        }

        let frame_len = frame.u32Len;

        let frame_data =
            unsafe { slice::from_raw_parts(frame_data as *mut u8, frame_len as usize) };

        callback(frame_data);

        mpi::ai_release_frame(dev_id, chn_id, &frame);

        Ok(())
    }
}

fn convert_u8_to_u16_slice(bytes: &[u8]) -> Vec<u16> {
    assert!(bytes.len() % 2 == 0);
    bytes.chunks_exact(2).map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])).collect()
}

/// Convert u8 slice to u16 slice, reusing output buffer to avoid allocations
fn convert_u8_to_u16_slice_reuse(bytes: &[u8], output: &mut Vec<u16>) {
    assert!(bytes.len() % 2 == 0);
    let needed_len = bytes.len() / 2;

    // Ensure capacity, but don't shrink if buffer is larger
    if output.capacity() < needed_len {
        output.reserve(needed_len - output.capacity());
    }

    // Clear and set length
    output.clear();
    unsafe {
        output.set_len(needed_len);
    }

    // Fill the buffer
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        output[i] = u16::from_le_bytes([chunk[0], chunk[1]]);
    }
}
