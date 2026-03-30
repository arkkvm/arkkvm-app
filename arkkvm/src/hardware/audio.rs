// audio.rs
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;
use std::time::Duration;
use std::thread;
use tracing::{debug, error, info};

#[repr(C)]
pub struct AudioInputConfig {
    card_name: *const c_char,
    sample_rate: c_int,
    channels: c_int,
    format: *const c_char,
}

#[repr(C)]
pub struct AudioEncodeConfig {
    encode_type: *const c_char,
    bitrate: c_int,
    sample_rate: c_int,
    channels: c_int,
}

#[derive(Debug)]
#[repr(C)]
pub enum AudioProcessorError {
    Success = 0,
    ErrorInit = -1,
    ErrorSystem = -2,
    ErrorMemory = -3,
    ErrorInvalidHandle = -4,
    ErrorCapture = -5,
    ErrorEncode = -6,
    ErrorTimeout = -7,
}

type AudioProcessorHandle = *mut c_void;


unsafe extern "C" {
    fn audio_processor_init(
        input_config: *const AudioInputConfig,
        encode_config: *const AudioEncodeConfig,
        handle: *mut AudioProcessorHandle,
    ) -> AudioProcessorError;

    fn audio_processor_capture_frame(
        handle: AudioProcessorHandle,
        data: *mut *mut i16,
        length: *mut c_uint,
    ) -> AudioProcessorError;

    fn audio_processor_encode_frame(
        handle: AudioProcessorHandle,
        encoded_data: *mut *mut u8,
        encoded_length: *mut c_uint,
    ) -> AudioProcessorError;

    fn audio_processor_release(handle: AudioProcessorHandle) -> AudioProcessorError;
}

pub struct AudioProcessor {
    handle: AudioProcessorHandle,
}

impl AudioProcessor {
    pub fn new(
        card_name: &str,
        sample_rate: i32,
        channels: i32,
        format: &str,
        encode_type: &str,
        encode_bitrate: i32,
    ) -> Result<Self, AudioProcessorError> {
        let input_config = AudioInputConfig {
            card_name: CString::new(card_name).unwrap().into_raw(),
            sample_rate,
            channels,
            format: CString::new(format).unwrap().into_raw(),
        };

        let encode_config = AudioEncodeConfig {
            encode_type: CString::new(encode_type).unwrap().into_raw(),
            bitrate: encode_bitrate,
            sample_rate,
            channels,
        };

        let mut handle: AudioProcessorHandle = ptr::null_mut();

        let result = unsafe {
            audio_processor_init(&input_config, &encode_config, &mut handle)
        };

        // Reclaim the CStrings to prevent memory leaks
        unsafe {
            let _ = CString::from_raw(input_config.card_name as *mut c_char);
            let _ = CString::from_raw(input_config.format as *mut c_char);
            let _ = CString::from_raw(encode_config.encode_type as *mut c_char);
        }

        match result {
            AudioProcessorError::Success => Ok(AudioProcessor { handle }),
            err => Err(err),
        }
    }

    pub fn capture_frame(&self) -> Result<Vec<i16>, AudioProcessorError> {
        let mut data_ptr: *mut i16 = ptr::null_mut();
        let mut length: c_uint = 0;

        let result = unsafe {
            audio_processor_capture_frame(self.handle, &mut data_ptr, &mut length)
        };

        match result {
            AudioProcessorError::Success => {
                let slice = unsafe { std::slice::from_raw_parts(data_ptr, length as usize) };
                Ok(slice.to_vec())
            }
            err => Err(err),
        }
    }

    pub fn encode_frame(&self) -> Result<Vec<u8>, AudioProcessorError> {
        let mut encoded_data_ptr: *mut u8 = ptr::null_mut();
        let mut encoded_length: c_uint = 0;

        let result = unsafe {
            audio_processor_encode_frame(self.handle, &mut encoded_data_ptr, &mut encoded_length)
        };

        match result {
            AudioProcessorError::Success => {
                let slice = unsafe { std::slice::from_raw_parts(encoded_data_ptr, encoded_length as usize) };
                Ok(slice.to_vec())
            }
            err => Err(err),
        }
    }
}

impl Drop for AudioProcessor {
    fn drop(&mut self) {
        unsafe {
            audio_processor_release(self.handle);
        }
    }
}

/// Get the audio sample rate detected by RK628
/// 
/// Reads the actual audio sample rate from RK628 hardware, returns default 48000 if unable to read
pub fn get_rk628_audio_rate() -> i32 {
    match crate::hardware::hdmi::rk628_hpd::get_rk628_audio_rate() {
        Some(rate) => rate as i32,
        None => {
            error!("Warning: Failed to get RK628 audio rate, using default 48000 Hz");
            48000
        }
    }
}

pub fn process_audio() -> Result<(), AudioProcessorError> {
    // Read the actual audio sample rate from RK628
    let sample_rate = get_rk628_audio_rate();
    
    let processor = AudioProcessor::new(
        "   ",
        sample_rate,
        2,
        "S16",
        "mp3",
        sample_rate,
    )?;

    info!("Audio processor initialized successfully");

    for i in 0..100 {
        let raw_data = processor.capture_frame()?;
        let encoded_data = processor.encode_frame()?;

        debug!(
            "Iteration {}: Captured {} bytes, Encoded to {} bytes",
            i,
            raw_data.len(),
            encoded_data.len()
        );

        // Simulate processing delay (10ms)
        thread::sleep(Duration::from_millis(10));
    }

    info!("Audio processing completed successfully");
    Ok(())
}