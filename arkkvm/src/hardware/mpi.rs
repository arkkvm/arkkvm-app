//! Rockchip MPP (Media Process Platform) global manager
//!
//! Provides a process-wide MPI singleton so the system is initialized only once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use anyhow::{Context, Result};
use tracing::info;

// Re-export types from rockchip_mpi_sys for convenience
pub use rockchip_mpi_sys::{
    MB_BLK, MB_POOL, MB_POOL_CONFIG_S, VENC_CHN_ATTR_S, VENC_PACK_S, 
    VENC_RECV_PIC_PARAM_S, VENC_STREAM_S, VIDEO_FRAME_INFO_S,
    // Audio input types
    AIO_ATTR_S, AI_CHN_PARAM_S, AUDIO_FRAME_S,
    // Audio constants/enums - re-export all from rockchip_mpi_sys
};

// Re-export audio-related constants and enums that might be used
// These are typically defined in rockchip_mpi_sys
pub use rockchip_mpi_sys::{
    rkAUDIO_BIT_WIDTH_E_AUDIO_BIT_WIDTH_16,
    rkAUDIO_SAMPLE_RATE_E_AUDIO_SAMPLE_RATE_48000,
    rkAIO_SOUND_MODE_E_AUDIO_SOUND_MODE_STEREO,
    rkAUDIO_LOOPBACK_MODE_E_AUDIO_LOOPBACK_NONE,
};

// Re-export constants
pub const MB_INVALID_POOLID: MB_POOL = u32::MAX as MB_POOL;
pub const RK_SUCCESS: i32 = 0;
pub const RK_FALSE: u32 = 0;
pub const RK_TRUE: u32 = 1;
pub const RK_ERR_VENC_BUF_EMPTY: i32 = 0x40408003;
pub const RK_ERR_VENC_TIME_OUT: i32 = 0xa004800eu32 as i32;

// Rockchip MPP constant definitions
pub const MB_ALLOC_TYPE_DMA: u32 = 0;
pub const VENC_RC_MODE_H264VBR: u32 = 2;
pub const RK_VIDEO_ID_AVC: u32 = 8;
pub const RK_FMT_YUV422_YUYV: u32 = 9;
pub const H264E_PROFILE_HIGH: u32 = 100;
pub const MIRROR_NONE: u32 = 0;
pub const COMPRESS_MODE_NONE: u32 = 0;

/// Global MPI manager
///
/// Singleton ensuring MPI is initialized only once
pub struct MpiManager {
    initialized: AtomicBool,
}

impl MpiManager {
    /// Return the global MPI manager instance
    pub fn instance() -> &'static MpiManager {
        static INSTANCE: OnceLock<MpiManager> = OnceLock::new();
        INSTANCE.get_or_init(|| MpiManager {
            initialized: AtomicBool::new(false),
        })
    }

    /// Initialize the MPI system
    ///
    /// No-op if already initialized
    pub fn init(&self) -> Result<()> {
        // compare_exchange for thread safety
        if self.initialized.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            unsafe {
                let ret = rockchip_mpi_sys::RK_MPI_SYS_Init();
                if ret != 0 {
                    // init failed; reset flag
                    self.initialized.store(false, Ordering::Release);
                    anyhow::bail!("Failed to initialize Rockchip MPP system: {}", ret);
                }
            }
            info!("Rockchip MPP system initialized");
        } else {
            // already initialized
            info!("Rockchip MPP system already initialized, skipping");
        }
        Ok(())
    }

    /// Whether MPI has been initialized
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Shut down MPI
    ///
    /// Call only after all MPI users have been torn down
    pub fn exit(&self) {
        if self.initialized.compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            unsafe {
                rockchip_mpi_sys::RK_MPI_SYS_Exit();
            }
            info!("Rockchip MPP system exited");
        }
    }
}

/// Initialize MPI (convenience wrapper)
pub fn init() -> Result<()> {
    MpiManager::instance().init()
}

/// Exit MPI (convenience wrapper)
pub fn exit() {
    MpiManager::instance().exit()
}

/// Whether MPI is initialized (convenience wrapper)
pub fn is_initialized() -> bool {
    MpiManager::instance().is_initialized()
}

// ========== Memory pool wrappers ==========

/// Create a memory pool
pub fn mb_create_pool(config: &mut MB_POOL_CONFIG_S) -> Result<MB_POOL> {
    // ensure MPI is initialized
    MpiManager::instance().init()
        .context("MPI system must be initialized before creating memory pool")?;

    unsafe {
        let pool = rockchip_mpi_sys::RK_MPI_MB_CreatePool(config);
        if pool == MB_INVALID_POOLID {
            anyhow::bail!("Failed to create memory pool");
        }
        Ok(pool)
    }
}

/// Destroy a memory pool
pub fn mb_destroy_pool(pool: MB_POOL) {
    unsafe {
        rockchip_mpi_sys::RK_MPI_MB_DestroyPool(pool);
    }
}

/// Get a memory block from a pool
pub fn mb_get_mb(pool: MB_POOL, size: u64, blocking: u32) -> Result<MB_BLK> {
    unsafe {
        let mb_blk = rockchip_mpi_sys::RK_MPI_MB_GetMB(pool, size, blocking);
        if mb_blk.is_null() {
            anyhow::bail!("Failed to get MB block from pool");
        }
        Ok(mb_blk)
    }
}

/// Release a memory block
pub fn mb_release_mb(mb_blk: MB_BLK) {
    unsafe {
        rockchip_mpi_sys::RK_MPI_MB_ReleaseMB(mb_blk);
    }
}

/// Convert a memory block handle to a file descriptor
pub fn mb_handle2fd(mb_blk: MB_BLK) -> Result<i32> {
    unsafe {
        let fd = rockchip_mpi_sys::RK_MPI_MB_Handle2Fd(mb_blk);
        if fd < 0 {
            anyhow::bail!("Failed to convert MB handle to file descriptor: {}", fd);
        }
        Ok(fd)
    }
}

/// Convert a memory block handle to a virtual address
pub fn mb_handle2viraddr(mb_blk: MB_BLK) -> *mut std::ffi::c_void {
    unsafe {
        rockchip_mpi_sys::RK_MPI_MB_Handle2VirAddr(mb_blk)
    }
}

// ========== VENC encoder wrappers ==========

/// VENC channel id
pub const VENC_CHANNEL: i32 = 0;

/// Create a VENC encoder channel
pub fn venc_create_chn(channel: i32, attr: &VENC_CHN_ATTR_S) -> Result<()> {
    // ensure MPI is initialized
    MpiManager::instance().init()
        .context("MPI system must be initialized before creating VENC channel")?;

    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_VENC_CreateChn(channel, attr);
        if ret < 0 {
            anyhow::bail!("Failed to create encoder channel: {}", ret);
        }
        Ok(())
    }
}

/// Destroy a VENC encoder channel
pub fn venc_destroy_chn(channel: i32) {
    unsafe {
        let _ = rockchip_mpi_sys::RK_MPI_VENC_DestroyChn(channel);
    }
}

/// Start receiving frames
pub fn venc_start_recv_frame(channel: i32, param: &VENC_RECV_PIC_PARAM_S) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_VENC_StartRecvFrame(channel, param);
        if ret < 0 {
            anyhow::bail!("Failed to start receiving frames: {}", ret);
        }
        Ok(())
    }
}

/// Stop receiving frames
pub fn venc_stop_recv_frame(channel: i32) {
    unsafe {
        rockchip_mpi_sys::RK_MPI_VENC_StopRecvFrame(channel);
    }
}

/// Get encoded stream
pub fn venc_get_stream(channel: i32, stream: &mut VENC_STREAM_S, timeout_ms: i32) -> i32 {
    unsafe {
        rockchip_mpi_sys::RK_MPI_VENC_GetStream(channel, stream, timeout_ms)
    }
}

/// Release encoded stream
pub fn venc_release_stream(channel: i32, stream: &mut VENC_STREAM_S) -> i32 {
    unsafe {
        rockchip_mpi_sys::RK_MPI_VENC_ReleaseStream(channel, stream)
    }
}

/// Send a frame to the encoder
pub fn venc_send_frame(channel: i32, frame: &VIDEO_FRAME_INFO_S, timeout_ms: i32) -> i32 {
    unsafe {
        rockchip_mpi_sys::RK_MPI_VENC_SendFrame(channel, frame, timeout_ms)
    }
}

/// Get VENC channel attributes (e.g. runtime bitrate updates)
pub fn venc_get_chn_attr(channel: i32) -> Result<VENC_CHN_ATTR_S> {
    unsafe {
        let mut attr: VENC_CHN_ATTR_S = std::mem::zeroed();
        let ret = rockchip_mpi_sys::RK_MPI_VENC_GetChnAttr(channel, &mut attr);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to get encoder channel attributes: {}", ret);
        }
        Ok(attr)
    }
}

/// Set VENC channel attributes (e.g. runtime bitrate updates)
pub fn venc_set_chn_attr(channel: i32, attr: &VENC_CHN_ATTR_S) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_VENC_SetChnAttr(channel, attr);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to set encoder channel attributes: {}", ret);
        }
        Ok(())
    }
}

// ========== Audio input (AI) wrappers ==========

/// Set audio input public attributes
pub fn ai_set_pub_attr(dev_id: i32, attr: &AIO_ATTR_S) -> Result<()> {
    // ensure MPI is initialized
    MpiManager::instance().init()
        .context("MPI system must be initialized before setting AI attributes")?;

    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_AI_SetPubAttr(dev_id, attr);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to set audio input attributes: {}", ret);
        }
        Ok(())
    }
}

/// Enable audio input device
pub fn ai_enable(dev_id: i32) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_AI_Enable(dev_id);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to enable audio input: {}", ret);
        }
        Ok(())
    }
}

/// Disable audio input device
pub fn ai_disable(dev_id: i32) {
    unsafe {
        let _ = rockchip_mpi_sys::RK_MPI_AI_Disable(dev_id);
    }
}

/// Set audio input channel parameters
pub fn ai_set_chn_param(dev_id: i32, chn_id: i32, param: &AI_CHN_PARAM_S) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_AI_SetChnParam(dev_id, chn_id, param);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to set audio input channel parameters: {}", ret);
        }
        Ok(())
    }
}

/// Enable audio input channel
pub fn ai_enable_chn(dev_id: i32, chn_id: i32) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_AI_EnableChn(dev_id, chn_id);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to enable audio input channel: {}", ret);
        }
        Ok(())
    }
}

/// Disable audio input channel
pub fn ai_disable_chn(dev_id: i32, chn_id: i32) {
    unsafe {
        let _ = rockchip_mpi_sys::RK_MPI_AI_DisableChn(dev_id, chn_id);
    }
}

/// Set audio input volume
pub fn ai_set_volume(dev_id: i32, volume: i32) -> Result<()> {
    unsafe {
        let ret = rockchip_mpi_sys::RK_MPI_AI_SetVolume(dev_id, volume);
        if ret != RK_SUCCESS {
            anyhow::bail!("Failed to set audio input volume: {}", ret);
        }
        Ok(())
    }
}

/// Get an audio frame
/// 
/// # Arguments
/// * `dev_id` - audio input device ID
/// * `chn_id` - audio input channel ID
/// * `frame` - audio frame struct
/// * `timeout_ms` - timeout in ms; -1 blocks
/// 
/// # Returns
/// Returns `RK_SUCCESS` on success, error code otherwise
pub fn ai_get_frame(dev_id: i32, chn_id: i32, frame: &mut AUDIO_FRAME_S, timeout_ms: i32) -> i32 {
    unsafe {
        rockchip_mpi_sys::RK_MPI_AI_GetFrame(dev_id, chn_id, frame, std::ptr::null_mut(), timeout_ms)
    }
}

/// Release an audio frame
pub fn ai_release_frame(dev_id: i32, chn_id: i32, frame: &AUDIO_FRAME_S) {
    unsafe {
        let _ = rockchip_mpi_sys::RK_MPI_AI_ReleaseFrame(dev_id, chn_id, frame, std::ptr::null());
    }
}
