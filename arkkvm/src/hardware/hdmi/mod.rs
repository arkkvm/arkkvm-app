//! HDMI Video Capture Module
//!
//! This module implements video capture from HDMI input and encoding to H.264.
//! Uses V4L2 for video capture and Rockchip MPP for hardware encoding.

use atomic_float::AtomicF64;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result};
use libc::{FD_ISSET, FD_SET, FD_ZERO, fd_set, select, timeval};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::hardware::mpi;
use crate::video::arkkvm_on_video_state_changed;

pub mod edid;
mod hdmirx;
pub mod rk628_hpd;
mod v4l2;
mod venc;

use mpi::MB_POOL;
use rk628_hpd::{HdmiConnectionStatus, Rk628HpdDetector};

// Constants
const VIDEO_DEV: &str = "/dev/video0";
const SUB_DEV: &str = "/dev/v4l-subdev2";
const INPUT_BUFFER_COUNT: usize = 2;
const BASE_BITRATE_HIGH: i32 = 2000; // 1500 kbps = 1.5 Mbps
const BASE_BITRATE_LOW: i32 = 512;   // 600 kbps = 0.5 Mbps
const MIN_BITRATE: i32 = 200;        // 200 kbps = 0.2 Mbps
const REF_WIDTH: u32 = 1920;
const REF_HEIGHT: u32 = 1080;
const MAX_RESOLUTION: u32 = REF_WIDTH * REF_HEIGHT;
const RESOLUTION_1920_1200: u32 = 1920 * 1200;

/// HDMI Video Capture
pub struct HdmiCapture {
    should_exit: Arc<AtomicBool>,
    streaming_flag: Arc<AtomicBool>,
    detected_fps: Arc<AtomicF64>, // FPS from HDMI hardware detection
    detected_hw_width: Arc<AtomicU32>, // HDMI IN Resolution
    detected_hw_height: Arc<AtomicU32>, // HDMI IN Resolution
    detected_changed: Arc<AtomicBool>, // True if HDMI IN resolution or FPS changed
    quality_factor: Arc<RwLock<f32>>,
    hw_detector_thread: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    streaming_thread: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    venc_read_thread: Arc<parking_lot::RwLock<Option<std::thread::JoinHandle<()>>>>,
    venc_running: Arc<AtomicBool>,
    mem_pool: Arc<RwLock<Option<MB_POOL>>>,
    hpd_detector: Arc<RwLock<Option<Rk628HpdDetector>>>,
    hpd_thread: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    sub_dev: Arc<RwLock<Option<File>>>,
}

impl HdmiCapture {
    /// Create a new HDMI capture instance
    pub fn new() -> Self {
        Self {
            should_exit: Arc::new(AtomicBool::new(false)),
            streaming_flag: Arc::new(AtomicBool::new(false)),
            detected_fps: Arc::new(AtomicF64::new(0.0)),
            detected_hw_width: Arc::new(AtomicU32::new(0)),
            detected_hw_height: Arc::new(AtomicU32::new(0)),
            detected_changed: Arc::new(AtomicBool::new(false)),
            quality_factor: Arc::new(RwLock::new(1.0)),
            hw_detector_thread: Arc::new(RwLock::new(None)),
            streaming_thread: Arc::new(RwLock::new(None)),
            venc_read_thread: Arc::new(parking_lot::RwLock::new(None)),
            venc_running: Arc::new(AtomicBool::new(false)),
            mem_pool: Arc::new(RwLock::new(None)),
            hpd_detector: Arc::new(RwLock::new(None)),
            hpd_thread: Arc::new(RwLock::new(None)),
            sub_dev: Arc::new(RwLock::new(None)),
        }
    }

    /// Initialize the video system
    pub async fn init(&self) -> Result<()> {
        // Initialize Rockchip MPP system (use the global manager to avoid multiple initializations)
        mpi::init().context("Failed to initialize Rockchip MPP system")?;

        // Open sub-device for FPS detection
        let sub_dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(SUB_DEV)
            .with_context(|| format!("Failed to open {}", SUB_DEV))?;
        info!("Opened sub device {} for FPS detection", SUB_DEV);
        *self.sub_dev.write().await = Some(sub_dev);

        Ok(())
    }

    /// Destroy memory pool (synchronous version for blocking threads)
    fn destroy_memory_pool_blocking(mem_pool: &Arc<RwLock<Option<MB_POOL>>>) {
        if let Some(pool) = mem_pool.blocking_write().take() {
            mpi::mb_destroy_pool(pool);
            info!("Destroyed memory pool");
        }
    }

    /// Create memory pool for given resolution (synchronous version for blocking threads)
    /// Note: This function does NOT destroy existing pool. Call destroy_memory_pool_blocking first if needed.
    fn create_memory_pool_blocking(
        mem_pool: &Arc<RwLock<Option<MB_POOL>>>,
        width: u32,
        height: u32,
    ) -> Result<()> {
        // Check if pool already exists
        {
            let pool_guard = mem_pool.blocking_read();
            if pool_guard.is_some() {
                anyhow::bail!(
                    "Memory pool already exists. Destroy it first before creating a new one."
                );
            }
        }

        let mut pool_cfg: mpi::MB_POOL_CONFIG_S = unsafe { std::mem::zeroed() };
        // Calculate buffer size based on actual resolution (YUYV format: 2 bytes per pixel)
        pool_cfg.u64MBSize = (width * height * 2) as u64;
        pool_cfg.u32MBCnt = INPUT_BUFFER_COUNT as u32;
        pool_cfg.enAllocType = mpi::MB_ALLOC_TYPE_DMA as i32;
        pool_cfg.bPreAlloc = mpi::RK_FALSE;

        match mpi::mb_create_pool(&mut pool_cfg) {
            Ok(pool) => {
                info!(
                    "Created memory pool for resolution {}x{} (buffer size: {} bytes)",
                    width, height, pool_cfg.u64MBSize
                );
                *mem_pool.blocking_write() = Some(pool);
                Ok(())
            }
            Err(e) => {
                anyhow::bail!("Failed to create memory pool for {}x{}: {}", width, height, e);
            }
        }
    }

    /// Destroy memory pool (async version)
    // async fn destroy_memory_pool_async(&self) {
    //     if let Some(pool) = self.mem_pool.write().await.take() {
    //         mpi::mb_destroy_pool(pool);
    //         info!("Destroyed memory pool");
    //     }
    // }

    /// Start HPD detection
    pub async fn start_hpd_detection(capture: Arc<HdmiCapture>) -> Result<()> {
        let mut hdp_detector_lock = capture.hpd_detector.write().await;
        if hdp_detector_lock.is_some() {
            warn!("HPD detection already started, skipping start");
            return Ok(());
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let detector = Rk628HpdDetector::new(tx);

        // Start HPD polling detection
        let hpd_handle = detector.start_polling(Duration::from_secs(1)).await?;
        *hdp_detector_lock = Some(detector);
        *capture.hpd_thread.write().await = Some(hpd_handle);

        let capture_clone = capture.clone();
        // Start HPD event handling task
        tokio::spawn(async move {
            info!("HPD event handling task started");
            while let Some(status) = rx.recv().await {
                match status {
                    HdmiConnectionStatus::Connected => {
                        info!("HDMI connected, starting HW detection and video streaming");
                        capture_clone.should_exit.store(false, Ordering::Release);
                        capture_clone.streaming_flag.store(true, Ordering::Release);

                        // Start HW detection thread
                        if let Err(e) = capture_clone.start_hw_detection().await {
                            error!("Failed to start HW detection: {}", e);
                        } else {
                            info!("HW detection thread started from HPD event");
                        }

                        // Start video streaming thread
                        if let Err(e) = capture_clone.start_streaming().await {
                            error!("Failed to start streaming: {}", e);
                        } else {
                            info!("Video streaming thread started from HPD event");
                        }
                    }
                    HdmiConnectionStatus::Disconnected => {
                        warn!("HDMI disconnected, stopping FPS detection and video stream");
                        capture_clone.should_exit.store(true, Ordering::Release);
                        // Stop FPS detection thread
                        capture_clone.wait_to_exit_hw_detection().await;
                        // Stop video stream
                        capture_clone.stop_streaming().await;

                        // Report error
                        report_video_format(false, Some("hdmi_disconnected"), 0, 0, 0.0, 0.0, 0.0);
                        info!("FPS detection and video streaming stopped");
                    }
                    HdmiConnectionStatus::NoSignal => {
                        warn!("HDMI plugged in but no signal detected, stopping FPS detection and video stream");
                        capture_clone.should_exit.store(true, Ordering::Release);
                        // Stop FPS detection thread
                        capture_clone.wait_to_exit_hw_detection().await;
                        // Stop video stream
                        capture_clone.stop_streaming().await;

                        // Report error
                        report_video_format(false, Some("hdmi_no_signal"), 0, 0, 0.0, 0.0, 0.0);
                        info!("FPS detection and video streaming stopped due to no signal");
                    }
                    HdmiConnectionStatus::Unknown => {
                        warn!("HDMI status unknown");
                    }
                }
            }
            info!("

             task stopped");
        });

        Ok(())
    }

    async fn stop_hpd_detection(&self) {
        // Stop HPD detection
        let detector = if let Some(detector) = self.hpd_detector.write().await.take() {
            detector.stop().await;
            Some(detector)
        }
        else {
            None
        };

        if let Some(handle) = self.hpd_thread.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        if let Some(detector) = detector {
            drop(detector);
        }
    }

    /// Start FPS detection thread (only for getting frame rate from HDMI hardware)
    async fn start_hw_detection(&self) -> Result<()> {
        // Check if FPS detection thread already exists
        if self.hw_detector_thread.read().await.is_some() {
            warn!("HW detection thread already exists, skipping start");
            return Ok(());
        }

        // Get sub_dev
        let sub_dev = {
            let sub_dev_guard = self.sub_dev.read().await;
            sub_dev_guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Sub device not initialized"))?
                .try_clone()
                .with_context(|| "Failed to clone sub device file descriptor")?
        };

        let should_exit = self.should_exit.clone();
        let detected_fps = self.detected_fps.clone();
        let detected_hw_width = self.detected_hw_width.clone();
        let detected_hw_height = self.detected_hw_height.clone();
        let detected_changed = self.detected_changed.clone();

        let handle = tokio::task::spawn_blocking(move || {
            run_hw_detection(
                should_exit,
                detected_fps,
                detected_hw_width,
                detected_hw_height,
                detected_changed,
                sub_dev,
            );
        });

        *self.hw_detector_thread.write().await = Some(handle);
        info!("HW detection thread started");
        Ok(())
    }

    /// Stop HW detection thread
    async fn wait_to_exit_hw_detection(&self) {
        if let Some(handle) = self.hw_detector_thread.write().await.take() {
            info!("Stopping HW detection thread");
            let _ = handle.await;
            info!("HW detection thread stopped");
        }
    }

    pub async fn restart_streaming(&self) -> Result<()> {
        self.should_exit.store(true, Ordering::Release);
        self.wait_to_exit_hw_detection().await;
        self.stop_streaming().await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        self.start_hw_detection().await?;
        self.start_streaming().await?;
        Ok(())
    }

    /// Start video streaming
    pub async fn start_streaming(&self) -> Result<()> {
        // Check if video streaming thread already exists
        if self.streaming_thread.read().await.is_some() {
            warn!("Video streaming thread already exists, skipping start");
            return Ok(());
        }

        self.should_exit.store(false, Ordering::Release);
        self.streaming_flag.store(true, Ordering::Release);

        let should_exit = self.should_exit.clone();
        let streaming_flag = self.streaming_flag.clone();
        let detected_fps = self.detected_fps.clone();
        let detected_hw_width = self.detected_hw_width.clone();
        let detected_hw_height = self.detected_hw_height.clone();
        let detected_changed = self.detected_changed.clone();
        let quality_factor = self.quality_factor.clone();
        let mem_pool = self.mem_pool.clone();
        let venc_running = self.venc_running.clone();
        let venc_read_thread = self.venc_read_thread.clone();

        let handle = tokio::task::spawn_blocking(move || {
            run_video_stream(
                should_exit,
                streaming_flag,
                detected_fps,
                detected_hw_width,
                detected_hw_height,
                detected_changed,
                quality_factor,
                mem_pool,
                venc_running,
                venc_read_thread,
            )
        });

        *self.streaming_thread.write().await = Some(handle);
        Ok(())
    }

    /// Stop video streaming
    pub async fn stop_streaming(&self) {
        self.streaming_flag.store(false, Ordering::Release);

        if let Some(handle) = self.streaming_thread.write().await.take() {
            info!("Stopping video streaming thread");
            if let Err(e) = handle.await {
                error!("Failed to stop video streaming thread: {}", e);
            }
        }
    }

    /// Set video quality
    pub async fn set_quality(&self, quality: f32) -> Result<()> {
        info!("HdmiCapture Setting video quality to {}", quality);
        {
            let mut quality_guard = self.quality_factor.write().await;
            *quality_guard = quality.clamp(0.0, 1.0);
        }

        if self.venc_running.load(Ordering::Acquire) {
            // self.restart_streaming().await?
            self.detected_changed.store(true, Ordering::Release);
        }
        Ok(())
    }

    pub async fn get_quality(&self) -> f32 {
        self.quality_factor.read().await.clone()
    }

    /// Shutdown video system
    pub async fn shutdown(&self) {
        self.should_exit.store(true, Ordering::Release);

        // Stop HPD detection
        self.stop_hpd_detection().await;

        // Stop FPS detection thread
        self.wait_to_exit_hw_detection().await;

        // Stop video streaming thread
        self.stop_streaming().await;

        info!("Video system shutdown complete");
    }
}

/// Calculate frame rate from v4l2_dv_timings
fn calculate_fps(dv_timings: &v4l2::v4l2_dv_timings) -> f64 {
    let bt = dv_timings.bt();
    let pixelclock = bt.pixelclock as f64;
    let total_width = (bt.width + bt.hfrontporch + bt.hsync + bt.hbackporch) as f64;
    let total_height = (bt.height + bt.vfrontporch + bt.vsync + bt.vbackporch) as f64;

    if total_width > 0.0 && total_height > 0.0 {
        pixelclock / (total_width * total_height)
    } else {
        0.0
    }
}

/// Run HW detection
fn run_hw_detection(
    should_exit: Arc<AtomicBool>,
    detected_fps: Arc<AtomicF64>,
    detected_hw_width: Arc<AtomicU32>,
    detected_hw_height: Arc<AtomicU32>,
    detected_changed: Arc<AtomicBool>,
    sub_dev: File,
) {
    let subdev_fd = sub_dev.as_raw_fd();
    let mut last_fps = 0.0;
    let mut last_hw_w = 0;
    let mut last_hw_h = 0;
    while !should_exit.load(Ordering::Acquire) {
        // Query DV timings to get frame rate and HDMI IN resolution
        match v4l2::query_dv_timings(subdev_fd) {
            Ok(dv_timings) => {
                let bt = dv_timings.bt();
                let hw_w = bt.width;
                let hw_h = bt.height;
                let fps = calculate_fps(&dv_timings);

                // Update detected HDMI IN resolution (for offset computation)
                if hw_w > 0
                    && hw_h > 0
                    && fps > 0.0
                    && (hw_w != last_hw_w || hw_h != last_hw_h || (fps - last_fps).abs() > 1.0)
                {
                    info!("HDMI IN resolution changed: {}x{}@{}fps, old: {}x{}@{}fps", hw_w, hw_h, fps, last_hw_w, last_hw_h, last_fps);
                    last_hw_w = hw_w;
                    last_hw_h = hw_h;
                    last_fps = fps;

                    detected_hw_width.store(hw_w, Ordering::Release);
                    detected_hw_height.store(hw_h, Ordering::Release);
                    detected_fps.store(fps, Ordering::Release);

                    detected_changed.store(true, Ordering::Release);
                }
            }

            Err(e) => {
                error!("HDMI HW detection failed: {}, resetting", e);
                // Failed to query timings, set FPS to 0
                if last_fps > 0.0 {
                    last_fps = 0.0;
                    last_hw_h = 0;
                    last_hw_w = 0;

                    detected_fps.store(0.0, Ordering::Release);
                    detected_hw_width.store(0, Ordering::Release);
                    detected_hw_height.store(0, Ordering::Release);
                    detected_changed.store(true, Ordering::Release);
                }
            }
        }
        // Check every 1000ms
        std::thread::sleep(Duration::from_secs(1));
    }

    detected_fps.store(0.0, Ordering::Release);
    detected_hw_width.store(0, Ordering::Release);
    detected_hw_height.store(0, Ordering::Release);
    info!("FPS detection thread finished");
}

/// Run video stream
fn run_video_stream(
    should_exit: Arc<AtomicBool>,
    streaming_flag: Arc<AtomicBool>,
    detected_fps: Arc<AtomicF64>,
    detected_hw_width: Arc<AtomicU32>,
    detected_hw_height: Arc<AtomicU32>,
    detected_changed: Arc<AtomicBool>,
    quality_factor: Arc<RwLock<f32>>,
    mem_pool: Arc<RwLock<Option<MB_POOL>>>,
    venc_running: Arc<AtomicBool>,
    venc_read_thread: Arc<parking_lot::RwLock<Option<std::thread::JoinHandle<()>>>>,
) {
    info!("try to start video stream");
    let mut last_width = 0u32;
    let mut last_height = 0u32;
    let mut last_offset_x = 0.0;
    let mut last_offset_y = 0.0;
    let mut retry = false;
    while streaming_flag.load(Ordering::Acquire) && !should_exit.load(Ordering::Acquire) {
        if !detected_changed.load(Ordering::Acquire) && !retry {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        detected_changed.store(false, Ordering::Release);
        retry = false;

        let hw_w = detected_hw_width.load(Ordering::Acquire);
        let hw_h = detected_hw_height.load(Ordering::Acquire);
        let fps = detected_fps.load(Ordering::Acquire);

        if hw_w == 0 || hw_h == 0 || fps == 0.0 {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        let (width, height) = {
            let resolution = hw_w * hw_h;
            if resolution > MAX_RESOLUTION {
                if resolution == RESOLUTION_1920_1200 {
                    (hw_w, hw_h)
                } else {
                    if hw_w > hw_h {
                        (REF_WIDTH, REF_HEIGHT)
                    } else {
                        (REF_HEIGHT, REF_WIDTH)
                    }
                }
            } else {
                (hw_w, hw_h)
            }
        };
        info!("HW Video format: {}x{}", width, height);

        // Open video device
        let video_dev = match std::fs::OpenOptions::new().read(true).write(true).open(VIDEO_DEV) {
            Ok(dev) => dev,
            Err(e) => {
                error!("Failed to open {}: {}", VIDEO_DEV, e);
                std::thread::sleep(Duration::from_secs(1));
                retry = true;
                continue;
            }
        };

        let video_fd = video_dev.as_raw_fd();
        info!("Opened video device {}", VIDEO_DEV);

        if !try_video_format(video_fd, width, height) {
            std::thread::sleep(Duration::from_millis(100));
            if !try_video_format(video_fd, width, height) {
                drop(video_dev);
                warn!("Failed to set video format ({}x{}), retrying", width, height);
                retry = true;
                continue;
            }
        }
        info!("Video format set successfully ({}x{})", width, height);

        // Get actual CSI stream resolution (don't set it, let driver use default)
        let (width, height) = match v4l2::get_video_format(video_fd) {
            Ok((w, h)) => (w, h),
            Err(e) => {
                error!("Failed to get CSI stream format: {}", e);
                drop(video_dev);
                std::thread::sleep(Duration::from_millis(100));
                retry = true;
                continue;
            }
        };
        info!("CSI stream resolution: {}x{}", width, height);

        // Scale the input resolution proportionally to the target resolution and compute centered offsets
        let (offset_x, offset_y) = if (hw_w * hw_h) != (width * height) {
            let target_w = width as f64;
            let target_h = height as f64;
            let src_w = hw_w as f64;
            let src_h = hw_h as f64;

            // Proportional scaling: pick the largest scale factor that does not exceed the target resolution
            let scale = (target_w / src_w).min(target_h / src_h);
            let scaled_w = (src_w * scale).round() as u32;
            let scaled_h = (src_h * scale).round() as u32;

            // Center offset (top-left coordinates)
            let offset_x = (width - scaled_w) as f32 / 2.0;
            let offset_y = (height - scaled_h) as f32 / 2.0;

            (offset_x, offset_y)
        } else {
            (0.0, 0.0)
        };

        if let Err(e) = HdmiCapture::create_memory_pool_blocking(&mem_pool, width, height) {
            error!("Failed to create memory pool: {}", e);
            drop(video_dev);
            retry = true;
            continue;
        }

        // Start encoder if not already running or resolution changed
        let quality = quality_factor.blocking_read();
        let bitrate = calculate_bitrate(*quality, width, height);
        drop(quality);

        // Request buffers
        let buffers = match v4l2::request_buffers(video_fd, &mem_pool) {
            Ok(bufs) => bufs,
            Err(e) => {
                error!("Failed to request buffers: {}", e);
                HdmiCapture::destroy_memory_pool_blocking(&mem_pool);
                drop(video_dev);
                retry = true;
                continue;
            }
        };
        info!("Buffers requested successfully");

        // Start encoder first (before starting V4L2 stream)
        if let Err(e) = venc::start_venc(
            bitrate,
            (bitrate as f32 * 2.0).round() as i32,
            width,
            height,
            fps,
            venc_running.clone(),
            venc_read_thread.clone(),
        ) {
            error!("Failed to start video encoder: {}", e);
            // Release buffers
            for buffer in &buffers {
                if let Some(mb_blk) = buffer.mb_blk {
                    mpi::mb_release_mb(mb_blk);
                }
            }
            HdmiCapture::destroy_memory_pool_blocking(&mem_pool);
            drop(video_dev);
            retry = true;
            continue;
        }
        info!("Video encoder started successfully");

        // Start stream after encoder is ready
        if let Err(e) = v4l2::stream_on(video_fd, v4l2::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE as u32) {
            error!("Failed to start streaming: {}", e);
            venc::stop_venc(venc_running.clone(), venc_read_thread.clone());
            // Release buffers
            for buffer in &buffers {
                if let Some(mb_blk) = buffer.mb_blk {
                    mpi::mb_release_mb(mb_blk);
                }
            }
            HdmiCapture::destroy_memory_pool_blocking(&mem_pool);
            drop(video_dev);
            retry = true;
            continue;
        }
        info!("Video streaming started ({}x{})", width, height);

        // Check if resolution changed (after initial setup)
        if last_width != width
            || last_height != height
            || last_offset_x != offset_x
            || last_offset_y != offset_y
        {
            info!(
                "Resolution changed: {}x{} (offset: {}x{}) -> {}x{} (offset: {}x{})",
                last_width,
                last_height,
                last_offset_x,
                last_offset_y,
                width,
                height,
                offset_x,
                offset_y
            );
            // Get FPS from HDMI hardware detection
            let fps = detected_fps.load(Ordering::Acquire);
            let fps = if fps > 0.0 { fps } else { 60.0 }; // Fallback to 60 if not detected yet

            // Report new resolution with FPS from HDMI hardware
            report_video_format(true, None, width, height, offset_x, offset_y, fps);

            // Update tracking variables
            last_width = width;
            last_height = height;
            last_offset_x = offset_x;
            last_offset_y = offset_y;
        }

        // Main loop: capture and encode frames
        let mut frame_count = 0u32;
        while streaming_flag.load(Ordering::Acquire) && !should_exit.load(Ordering::Acquire) {
            // Periodically check if resolution changed
            if detected_changed.load(Ordering::Acquire) {
                info!("Resolution changed, restarting video stream");
                break;
            }

            // Use select to wait for data ready
            unsafe {
                let mut read_fds: fd_set = std::mem::zeroed();
                FD_ZERO(&mut read_fds);
                FD_SET(video_fd, &mut read_fds);

                let mut timeout: timeval = timeval { tv_sec: 1, tv_usec: 0 };

                let ret = select(
                    video_fd + 1,
                    &mut read_fds,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut timeout,
                );

                if ret == 0 {
                    warn!("Select timeout");
                    retry = true;
                    break;
                } else if ret < 0 {
                    let errno = std::io::Error::last_os_error();
                    if errno.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    error!("Select failed: {}", errno);
                    retry = true;
                    break;
                }

                // Check if file descriptor is ready
                if !FD_ISSET(video_fd, &read_fds) {
                    continue;
                }
            }

            // Dequeue buffer
            let (buf_index, _frame_size) = match v4l2::dequeue_buffer(video_fd) {
                Ok(result) => result,
                Err(e) => {
                    error!("Failed to dequeue buffer: {}", e);
                    retry = true;
                    break;
                }
            };

            // Send frame to encoder
            if let Some(mb_blk) = buffers.get(buf_index).and_then(|b| b.mb_blk) {
                let timestamp_us = get_timestamp_us();
                if let Err(e) =
                    venc::send_frame_to_venc(mb_blk, width, height, frame_count, timestamp_us)
                {
                    warn!("Failed to send frame to encoder: {}", e);
                    retry = true;
                    break;
                }

                if !venc_running.load(Ordering::Acquire) {
                    retry = true;
                    break;
                }
            }

            frame_count += 1;

            // Requeue buffer
            if let Err(e) = v4l2::queue_buffer(video_fd, buf_index, &buffers) {
                error!("Failed to requeue buffer: {}", e);
                retry = true;
                break;
            }
        }

        // Cleanup: all resource release happens here after loop exits
        // Stop encoder if running
        venc::stop_venc(venc_running.clone(), venc_read_thread.clone());
        info!("Encoder stopped");

        // Stop video stream
        if let Err(e) = v4l2::stream_off(video_fd, v4l2::V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE as u32)
        {
            warn!("Failed to stop video stream: {:?}", e);
        }
        info!("Video stream stopped");

        // Release buffers
        for buffer in &buffers {
            if let Some(mb_blk) = buffer.mb_blk {
                mpi::mb_release_mb(mb_blk);
            }
        }
        info!("Buffers released");

        // Destroy memory pool
        HdmiCapture::destroy_memory_pool_blocking(&mem_pool);
        info!("Memory pool destroyed");

        // video_dev will be dropped here automatically, closing the file descriptor
        drop(video_dev);
        info!("Video device closed");
    }
    info!("video stream thread finished");
}

fn try_video_format(video_fd: RawFd, width: u32, height: u32) -> bool {
    match v4l2::set_video_format(video_fd, width, height) {
        Ok((actual_width, actual_height)) => {
            let diff = actual_width != width || actual_height != height;
            if diff {
                warn!(
                    "Driver modified resolution: requested {}x{}, got {}x{}",
                    width, height, actual_width, actual_height
                );
            }
            !diff
        }
        Err(e) => {
            error!("Failed to set video format({}x{}): {}", width, height, e);
            false
        }
    }
}

/// Calculate bitrate
fn calculate_bitrate(quality_factor: f32, width: u32, height: u32) -> i32 {
    let scale_factor = (width * height) as f64 / (REF_WIDTH * REF_HEIGHT) as f64;
    let base_bitrate = BASE_BITRATE_LOW as f64
        + (BASE_BITRATE_HIGH - BASE_BITRATE_LOW) as f64 * quality_factor as f64;
    let bitrate = (base_bitrate * scale_factor) as i32;
    let bitrate = bitrate.max(MIN_BITRATE);
    info!(
        "Calculated bitrate: {} (quality: {}, width: {}, height: {})",
        bitrate, quality_factor, width, height
    );
    bitrate
}

/// Get timestamp (microseconds)
fn get_timestamp_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64
}

/// Report video format
fn report_video_format(
    ready: bool,
    error: Option<&str>,
    width: u32,
    height: u32,
    offset_x: f32,
    offset_y: f32,
    fps: f64,
) {
    let error_cstr = error.map(|s| CString::new(s).unwrap());
    let error_ptr = error_cstr.as_ref().map(|s| s.as_ptr()).unwrap_or(std::ptr::null());

    unsafe {
        arkkvm_on_video_state_changed(
            if ready { 1 } else { 0 },
            width as u16,
            height as u16,
            offset_x,
            offset_y,
            fps,
            error_ptr,
        );
    }
}
