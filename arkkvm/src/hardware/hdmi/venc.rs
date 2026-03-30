//! Rockchip MPP Video Encoder Interface
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::alloc::{Layout, alloc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use anyhow::{Result, anyhow};
use bytes::Bytes;
use parking_lot::RwLock;
use tracing::{error, info, warn};

use crate::hardware::mpi;

// Re-export required types and constants from mpi module
pub use crate::hardware::mpi::{
    COMPRESS_MODE_NONE, H264E_PROFILE_HIGH, MB_BLK,
    MIRROR_NONE, RK_ERR_VENC_BUF_EMPTY, RK_ERR_VENC_TIME_OUT, RK_FMT_YUV422_YUYV, RK_SUCCESS,
    RK_VIDEO_ID_AVC, VENC_CHANNEL, VENC_CHN_ATTR_S, VENC_PACK_S, VENC_RC_MODE_H264VBR,
    VENC_RECV_PIC_PARAM_S, VENC_STREAM_S, VIDEO_FRAME_INFO_S,
};
// Rockchip MPP VENC error code definitions (from rk_mpi_venc.h)
// Note: Error code format is RK_DEF_ERR(RK_ID_VENC, RK_ERR_LEVEL_ERROR, RK_ERR_XXX)
// Common error codes:
// - RK_ERR_VENC_BUF_EMPTY: Buffer is empty (normal case, indicates no encoded data available)
// - RK_ERR_VENC_SYS_NOTREADY: System not ready
// - RK_ERR_VENC_BUSY: System busy
// - RK_ERR_VENC_UNEXIST: Channel does not exist
// - 0xa004800e: Channel not ready or encoder busy (temporary error, may occur during initialization)
// For other error codes, refer to Rockchip MPP SDK documentation

/// Get a human-readable description of a VENC error code
fn venc_error_description(error_code: i32) -> &'static str {
    match error_code as u32 {
        x if x == RK_ERR_VENC_BUF_EMPTY as u32 => {
            "Buffer is empty (normal, no encoded data available)"
        }
        0xa004800e => {
            "Channel not ready or encoder busy (temporary, may occur during initialization)"
        }
        0x80000001 => "System not ready",
        0x80000002 => "System busy",
        0x80000003 => "Channel does not exist",
        _ => "Unknown VENC error",
    }
}

/// Alignment function
fn align(x: u32, a: u32) -> u32 {
    (x + a - 1) & !(a - 1)
}

fn align_2(x: u32) -> u32 {
    align(x, 2)
}

/// Populate encoder attributes
fn populate_venc_attr(
    attr: &mut VENC_CHN_ATTR_S,
    bitrate: u32,
    max_bitrate: u32,
    width: u32,
    height: u32,
    fps: f64
) {
    attr.stRcAttr.enRcMode = VENC_RC_MODE_H264VBR;
    // Note: u32BitRate and u32MaxBitRate units are kbps (Rockchip MPP API)
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32BitRate = bitrate;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32MaxBitRate = max_bitrate;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32Gop = fps.round() as u32;

    let fps_int = fps.round() as u32;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32SrcFrameRateNum = fps_int;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32SrcFrameRateDen = 1;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.fr32DstFrameRateNum = fps_int;
    attr.stRcAttr.__bindgen_anon_1.stH264Vbr.fr32DstFrameRateDen = 1;

    attr.stVencAttr.enType = RK_VIDEO_ID_AVC;
    attr.stVencAttr.enPixelFormat = RK_FMT_YUV422_YUYV;
    attr.stVencAttr.u32Profile = H264E_PROFILE_HIGH;
    attr.stVencAttr.u32PicWidth = width;
    attr.stVencAttr.u32PicHeight = height;
    attr.stVencAttr.u32VirWidth = align_2(width);
    attr.stVencAttr.u32VirHeight = align_2(height);
    attr.stVencAttr.u32StreamBufCnt = 3;
    attr.stVencAttr.u32BufSize = width * height * 3 / 2;
    attr.stVencAttr.enMirror = MIRROR_NONE;
}

/// Dynamically update bitrate at runtime (Option A: GetChnAttr -> modify RC fields -> SetChnAttr)
/// Note: This function only modifies RC-related fields to avoid affecting other encoding parameters.
// pub fn update_venc_bitrate(bitrate: i32, max_bitrate: i32) -> Result<()> {
//     if bitrate <= 0 || max_bitrate <= 0 {
//         return Err(anyhow!("Invalid bitrate/max_bitrate: {}, {}", bitrate, max_bitrate));
//     }

//     let mut attr = mpi::venc_get_chn_attr(VENC_CHANNEL)
//         .map_err(|e| anyhow!("Failed to get encoder channel attr: {}", e))?;

//     // Only update when the current RC mode is H264VBR to avoid writing the wrong union fields in other modes
//     if attr.stRcAttr.enRcMode != VENC_RC_MODE_H264VBR {
//         return Err(anyhow!(
//             "Unsupported rc mode for dynamic bitrate update: {} (expected H264VBR={})",
//             attr.stRcAttr.enRcMode,
//             VENC_RC_MODE_H264VBR
//         ));
//     }

//     attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32BitRate = bitrate as u32;
//     attr.stRcAttr.__bindgen_anon_1.stH264Vbr.u32MaxBitRate = max_bitrate as u32;

//     mpi::venc_set_chn_attr(VENC_CHANNEL, &attr)
//         .map_err(|e| anyhow!("Failed to set encoder channel attr: {}", e))?;

//     info!(
//         "Updated VENC bitrate: bitrate={} max_bitrate={}",
//         bitrate, max_bitrate
//     );
//     Ok(())
// }

/// Start encoder
pub fn start_venc(
    bitrate: i32,
    max_bitrate: i32,
    width: u32,
    height: u32,
    fps: f64,
    venc_running: Arc<AtomicBool>,
    venc_read_thread: Arc<RwLock<Option<std::thread::JoinHandle<()>>>>,
) -> Result<()> {
    let mut attr: VENC_CHN_ATTR_S = unsafe { std::mem::zeroed() };
    populate_venc_attr(&mut attr, bitrate as u32, max_bitrate as u32, width, height, fps);

    mpi::venc_create_chn(VENC_CHANNEL, &attr)
        .map_err(|e| anyhow!("Failed to create encoder channel: {}", e))?;

    let recv_param = VENC_RECV_PIC_PARAM_S { s32RecvPicNum: -1 };

    if let Err(e) = mpi::venc_start_recv_frame(VENC_CHANNEL, &recv_param) {
        mpi::venc_destroy_chn(VENC_CHANNEL);
        return Err(anyhow!("Failed to start receiving frames: {}", e));
    }

    venc_running.store(true, Ordering::Release);

    // Start encoder read thread
    let venc_running_clone = venc_running.clone();
    let handle = std::thread::spawn(move || {
        venc_read_stream(venc_running_clone, fps);
    });

    *venc_read_thread.write() = Some(handle);

    Ok(())
}

/// Stop encoder
pub fn stop_venc(
    venc_running: Arc<AtomicBool>,
    venc_read_thread: Arc<RwLock<Option<std::thread::JoinHandle<()>>>>,
) {
    venc_running.store(false, Ordering::Release);

    mpi::venc_stop_recv_frame(VENC_CHANNEL);

    if let Some(handle) = venc_read_thread.write().take() {
        // Wait for thread to complete, but this is in a blocking context, so use blocking_read
        // Actually handle is a JoinHandle, we need to await in async context
        // But since this is in a blocking function, we need to use tokio::runtime::Handle::current().block_on
        // if let Ok(rt) = tokio::runtime::Handle::try_current() {
        //     let _ = rt.block_on(handle);
        // } else {
        //     // If no runtime exists, create a temporary one
        //     let rt = tokio::runtime::Runtime::new().unwrap();
        //     let _ = rt.block_on(handle);
        // }
        if let Err(e) = handle.join() {
            error!("Failed to join video encoder read thread: {:?}", e);
        }
    }

    mpi::venc_destroy_chn(VENC_CHANNEL);
}

/// Encoder read stream thread
fn venc_read_stream(venc_running: Arc<AtomicBool>, fps: f64) {
    let pts = (1000.0 / fps).ceil() as i32;
    let pack_layout = Layout::new::<VENC_PACK_S>();
    let pack_ptr = unsafe { alloc(pack_layout) as *mut VENC_PACK_S };

    if pack_ptr.is_null() {
        error!("Failed to allocate VENC_PACK_S");
        return;
    }

    info!("Start encoder read stream with pts: {}", pts);
    let mut frame = VENC_STREAM_S { pstPack: pack_ptr, u32PackCount: 1, ..unsafe { std::mem::zeroed() } };
    while venc_running.load(Ordering::Acquire) {
        let ret = mpi::venc_get_stream(VENC_CHANNEL, &mut frame, pts);
        if ret == RK_SUCCESS {
            // Ensure pointer is valid
            if frame.pstPack.is_null() {
                error!("Invalid pack pointer from RK_MPI_VENC_GetStream");
                break;
            }

            let pack = unsafe { &*frame.pstPack };

            // Ensure mb_blk is valid
            if pack.pMbBlk.is_null() {
                warn!("Invalid mb_blk pointer in pack");
                let ret = mpi::venc_release_stream(VENC_CHANNEL, &mut frame);
                if ret != RK_SUCCESS {
                    warn!("Failed to release stream: 0x{:x}", ret);
                }
                continue;
            }

            let data = mpi::mb_handle2viraddr(pack.pMbBlk) as *const u8;

            // Ensure data pointer is valid
            if data.is_null() {
                warn!("Invalid data pointer from RK_MPI_MB_Handle2VirAddr");
                let ret = mpi::venc_release_stream(VENC_CHANNEL, &mut frame);
                if ret != RK_SUCCESS {
                    warn!("Failed to release stream: 0x{:x}", ret);
                }
                continue;
            }

            // Pass H.264 frame through FFI callback
            // Note: data pointer is valid before ReleaseStream
            let slice = unsafe { std::slice::from_raw_parts(data, pack.u32Len as usize) };
            // Note: Must copy because C pointer memory will be released after venc_release_stream
            crate::video::on_frame_received(Bytes::copy_from_slice(slice), pack.u64PTS, fps);

            let ret = mpi::venc_release_stream(VENC_CHANNEL, &mut frame);
            if ret != RK_SUCCESS {
                warn!("Failed to release stream: 0x{:x}", ret);
            }
        } else if ret != RK_ERR_VENC_BUF_EMPTY && ret != RK_ERR_VENC_TIME_OUT {
            warn!("Failed to get stream (encoder running): 0x{:x} - {} (may be temporary)", ret, venc_error_description(ret));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Free memory
    unsafe { std::alloc::dealloc(pack_ptr as *mut u8, pack_layout) };
    info!("Exiting video encoder stream reader");
}

/// Send frame to encoder
pub fn send_frame_to_venc(
    mb_blk: MB_BLK,
    width: u32,
    height: u32,
    frame_count: u32,
    timestamp_us: u64,
) -> Result<()> {
    let mut st_frame = VIDEO_FRAME_INFO_S { stVFrame: unsafe { std::mem::zeroed() } };
    st_frame.stVFrame.pMbBlk = mb_blk;
    st_frame.stVFrame.u32Width = width;
    st_frame.stVFrame.u32Height = height;
    st_frame.stVFrame.u32VirWidth = (width + 2 - 1) & !(2 - 1); // align_2
    st_frame.stVFrame.u32VirHeight = (height + 2 - 1) & !(2 - 1); // align_2
    st_frame.stVFrame.u32TimeRef = frame_count;
    st_frame.stVFrame.u64PTS = timestamp_us;
    st_frame.stVFrame.enPixelFormat = RK_FMT_YUV422_YUYV;
    st_frame.stVFrame.u32FrameFlag = 0;
    st_frame.stVFrame.enCompressMode = COMPRESS_MODE_NONE;

    let mut retried = false;
    loop {
        let ret = mpi::venc_send_frame(VENC_CHANNEL, &st_frame, 2000);
        if ret == RK_SUCCESS {
            break;
        }

        if retried {
            return Err(anyhow!("RK_MPI_VENC_SendFrame retry failed: 0x{:x}", ret));
        }

        retried = true;
        std::thread::sleep(std::time::Duration::from_micros(1000));
    }

    Ok(())
}
