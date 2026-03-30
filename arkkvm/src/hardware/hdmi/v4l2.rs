//! V4L2 (Video4Linux2) Interface Bindings
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::os::raw::{c_int, c_ulong, c_void};
use std::os::unix::io::RawFd;
use anyhow::{Result, anyhow};
use libc::ioctl;

/// timeval structure (matches kernel expected layout)
/// struct timeval {
///     time_t          tv_sec;         /* seconds */
///     long            tv_usec;        /* microseconds */
/// };
/// On 32-bit architectures, time_t and long are both 32-bit (4 bytes), so timeval is 8 bytes
/// On 64-bit architectures, time_t and long are both 64-bit (8 bytes), so timeval is 16 bytes
/// Based on VIDIOC_QUERYBUF value (0xc0445609, size=68), kernel expects 8-byte timeval
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct timeval {
    /// Seconds (32-bit, matches 32-bit architecture kernel expectation)
    pub tv_sec: i32,
    /// Microseconds (32-bit, matches 32-bit architecture kernel expectation)
    pub tv_usec: i32,
}

use super::INPUT_BUFFER_COUNT;
use crate::hardware::mpi;
use mpi::{MB_BLK, MB_POOL, RK_TRUE};

// V4L2 constants
/// Buffer type: Multi-planar Video Capture - for multi-planar format video capture buffers
pub const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
/// Memory type: DMA Buffer (Direct Memory Access) - uses DMA direct memory access for zero-copy efficient transfer
pub const V4L2_MEMORY_DMABUF: u32 = 4;
/// Pixel format: YUYV (YUV 4:2:2) - a common YUV pixel format, each pixel represented by 2 bytes
pub const V4L2_PIX_FMT_YUYV: u32 = 0x56595559; // 'YUYV'
/// Field type: Any Field - accepts any field type (progressive or interlaced)
pub const V4L2_FIELD_ANY: u32 = 0;


/// Event type: All Events - subscribe to all event types
pub const V4L2_EVENT_ALL: u32 = 0;
/// Event type: Vertical Sync (VSync) - triggered when video frame vertical sync signal occurs
pub const V4L2_EVENT_VSYNC: u32 = 1;
/// Event type: End of Stream - triggered when video stream ends
pub const V4L2_EVENT_EOS: u32 = 2;
/// Event type: Control Change - triggered when video control parameters change
pub const V4L2_EVENT_CTRL: u32 = 3;
/// Event type: Frame Sync - triggered when frame sync signal occurs
pub const V4L2_EVENT_FRAME_SYNC: u32 = 4;
/// Event type: Source Change - triggered when video signal source changes (e.g., HDMI connect/disconnect, resolution change, etc.)
pub const V4L2_EVENT_SOURCE_CHANGE: u32 = 5;
/// Event type: Motion Detection - triggered when motion is detected
pub const V4L2_EVENT_MOTION_DET: u32 = 6;
/// Event type: Private Event Start - starting number for private event types, used for extending custom events
pub const V4L2_EVENT_PRIVATE_START: u32 = 0x08000000;

/// V4L2 event source change data structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct v4l2_event_src_change {
    /// Change type bitmask (Changes) - indicates the type of change that occurred
    pub changes: u32,
    /// Reserved field
    pub reserved: [u32; 7],
}

impl Default for v4l2_event_src_change {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// Source change type: Resolution Change - set when video resolution changes
pub const V4L2_EVENT_SRC_CH_RESOLUTION: u32 = 0x0001;

// V4L2 ioctl commands
/// Get video format (Get Format) - get current pixel format, resolution and other parameters of video capture device
/// _IOWR('V', 4, struct v4l2_format)
pub const VIDIOC_G_FMT: c_ulong = 0xc0cc5604;
/// Set video format (Set Format) - set pixel format, resolution and other parameters of video capture device
/// _IOWR('V', 5, struct v4l2_format)
pub const VIDIOC_S_FMT: c_ulong = 0xc0cc5605;
/// Request buffers (Request Buffers) - request allocation of specified number of video buffers
pub const VIDIOC_REQBUFS: c_ulong = 0xc0145608;
/// Query buffer (Query Buffer) - query buffer information (address, size, etc.) for specified index
pub const VIDIOC_QUERYBUF: c_ulong = 0xc0445609;
/// Queue buffer (Queue Buffer) - add buffer to driver's input queue, ready to receive data
pub const VIDIOC_QBUF: c_ulong = 0xc044560f;
/// Dequeue buffer (Dequeue Buffer) - remove buffer with filled data from driver's output queue
pub const VIDIOC_DQBUF: c_ulong = 0xc0445611;
/// Start stream (Stream On) - start video stream capture
pub const VIDIOC_STREAMON: c_ulong = 0x40045612;
/// Stop stream (Stream Off) - stop video stream capture
pub const VIDIOC_STREAMOFF: c_ulong = 0x40045613;
/// Query digital video timings (Query DV Timings) - query currently detected digital video timing information (resolution, frame rate, etc.)
/// _IOR('V', 99, struct v4l2_dv_timings)
/// According to videodev2.h: struct v4l2_dv_timings size is 132 bytes (4 + 128)
pub const VIDIOC_QUERY_DV_TIMINGS: c_ulong = 0x80845663;
/// Subscribe event (Subscribe Event) - subscribe to V4L2 events (e.g., source change events)
/// _IOW('V', 90, struct v4l2_event_subscription)
pub const VIDIOC_SUBSCRIBE_EVENT: c_ulong = 0x4020565a;
/// Dequeue event (Dequeue Event) - remove occurred events from event queue
/// _IOR('V', 89, struct v4l2_event)
pub const VIDIOC_DQEVENT: c_ulong = 0x80805659;

// V4L2 structure definitions
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_rect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_fract {
    pub numerator: u32,
    pub denominator: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct v4l2_bt_timings {
    /// Video width (pixels)
    pub width: u32,
    /// Video height (pixels)
    pub height: u32,
    /// Whether interlaced (0=progressive, 1=interlaced)
    pub interlaced: u32,
    /// Polarity flags
    pub polarities: u32,
    /// Pixel clock frequency (Hz)
    pub pixelclock: u64,
    /// Horizontal Front Porch
    pub hfrontporch: u32,
    /// Horizontal Sync
    pub hsync: u32,
    /// Horizontal Back Porch
    pub hbackporch: u32,
    /// Vertical Front Porch
    pub vfrontporch: u32,
    /// Vertical Sync
    pub vsync: u32,
    /// Vertical Back Porch
    pub vbackporch: u32,
    /// Interlaced Vertical Front Porch
    pub il_vfrontporch: u32,
    /// Interlaced Vertical Sync
    pub il_vsync: u32,
    /// Interlaced Vertical Back Porch
    pub il_vbackporch: u32,
    /// Video standard flags
    pub standards: u32,
    /// Flags
    pub flags: u32,
    /// Aspect ratio
    pub picture_aspect: v4l2_fract,
    /// CEA-861 VIC（Video Identification Code）
    pub cea861_vic: u8,
    /// HDMI VIC（Video Identification Code）
    pub hdmi_vic: u8,
    /// Reserved field
    pub reserved: [u8; 46],
}

impl Default for v4l2_bt_timings {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// V4L2 DV timings union (matches union in C header file)
#[repr(C, packed)]
pub union v4l2_dv_timings_union {
    /// BT timings data (Blanking Timings)
    pub bt: v4l2_bt_timings,
    /// Reserved field (used to ensure union size is 128 bytes)
    pub reserved: [u32; 32],
}

impl Default for v4l2_dv_timings_union {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_dv_timings_union {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_dv_timings_union {}

impl std::fmt::Debug for v4l2_dv_timings_union {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            f.debug_struct("v4l2_dv_timings_union")
                .field("bt", &self.bt)
                .finish()
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_dv_timings {
    /// Timing type (V4L2_DV_BT_656_1120, etc.)
    pub type_: u32,
    /// Union (BT timings data or reserved field)
    pub u: v4l2_dv_timings_union,
}

impl v4l2_dv_timings {
    /// Get BT timings data (safely access bt field in union)
    pub fn bt(&self) -> &v4l2_bt_timings {
        unsafe { &self.u.bt }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_event_subscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct v4l2_event {
    pub type_: u32,
    pub u: v4l2_event_data,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: timeval,
    pub id: u32,
    pub reserved: [u32; 8],
}

impl Default for v4l2_event {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub union v4l2_event_data {
    pub data: [u8; 64],
}

impl Default for v4l2_event_data {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_event_data {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_event_data {}

impl std::fmt::Debug for v4l2_event_data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4l2_event_data").field("data", &"[...]").finish()
    }
}

/// V4L2 multi-planar pixel format encoding union (matches anonymous union in C header file)
#[repr(C, packed)]
pub union v4l2_pix_format_mplane_enc {
    /// YCbCr encoding
    pub ycbcr_enc: u8,
    /// HSV encoding
    pub hsv_enc: u8,
}

impl Default for v4l2_pix_format_mplane_enc {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_pix_format_mplane_enc {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_pix_format_mplane_enc {}

impl std::fmt::Debug for v4l2_pix_format_mplane_enc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            f.debug_struct("v4l2_pix_format_mplane_enc")
                .field("ycbcr_enc", &self.ycbcr_enc)
                .finish()
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_pix_format_mplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [v4l2_plane_pix_format; 8],
    pub num_planes: u8,
    pub flags: u8,
    /// Encoding union (YCbCr or HSV encoding)
    pub enc: v4l2_pix_format_mplane_enc,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_plane_pix_format {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_format {
    pub type_: u32,
    pub fmt: v4l2_format_fmt,
}

#[repr(C)]
pub union v4l2_format_fmt {
    pub pix: v4l2_pix_format,
    pub pix_mp: v4l2_pix_format_mplane,
    pub raw_data: [u8; 200],
}

impl Default for v4l2_format_fmt {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_format_fmt {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_format_fmt {}

impl std::fmt::Debug for v4l2_format_fmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4l2_format_fmt").field("union", &"[...]").finish()
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_pix_format {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_requestbuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub reserved: [u32; 1],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: v4l2_plane_m,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

#[repr(C)]
pub union v4l2_plane_m {
    pub mem_offset: u32,
    pub userptr: u64,
    pub fd: c_int,
    pub reserved: u64,
}

impl Default for v4l2_plane_m {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_plane_m {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_plane_m {}

impl std::fmt::Debug for v4l2_plane_m {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4l2_plane_m").field("union", &"[...]").finish()
    }
}

/// V4L2 buffer request union (matches union in C header file)
#[repr(C)]
pub union v4l2_buffer_request {
    /// Request file descriptor (Request File Descriptor)
    pub request_fd: c_int,
    /// Reserved field
    pub reserved: u32,
}

impl Default for v4l2_buffer_request {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_buffer_request {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_buffer_request {}

impl std::fmt::Debug for v4l2_buffer_request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            f.debug_struct("v4l2_buffer_request")
                .field("reserved", &self.reserved)
                .finish()
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct v4l2_buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: timeval,
    pub timecode: v4l2_timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: v4l2_buffer_m,
    pub length: u32,
    pub reserved2: u32,
    /// Request union (Request Union) - contains request_fd or reserved
    pub request: v4l2_buffer_request,
}

impl Default for v4l2_buffer {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct v4l2_timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
pub union v4l2_buffer_m {
    pub offset: u32,
    /// On 32-bit architectures, unsigned long is 4 bytes, not 8 bytes
    /// Use usize to match target architecture's pointer size
    pub userptr: usize,
    pub planes: *mut v4l2_plane,
    pub fd: c_int,
}

impl Default for v4l2_buffer_m {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Clone for v4l2_buffer_m {
    fn clone(&self) -> Self {
        unsafe { std::ptr::read(self) }
    }
}

impl Copy for v4l2_buffer_m {}

impl std::fmt::Debug for v4l2_buffer_m {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4l2_buffer_m").field("union", &"[...]").finish()
    }
}

/// Video buffer
pub struct VideoBuffer {
    pub plane_buffer: v4l2_plane,
    pub mapped_addr: *mut c_void,
    pub length: usize,
    pub mb_blk: Option<MB_BLK>,
}

impl VideoBuffer {
    pub fn new() -> Self {
        Self {
            plane_buffer: v4l2_plane::default(),
            mapped_addr: std::ptr::null_mut(),
            length: 0,
            mb_blk: None,
        }
    }
}

/// V4L2 ioctl wrapper (internal use)
/// Note: On 32-bit architectures, c_ulong is u32 and can be used directly
/// On 64-bit architectures, c_ulong is u64, but ioctl's request parameter is still u32
fn v4l2_ioctl(fd: RawFd, request: c_ulong, arg: *mut c_void) -> c_int {
    // Ensure request value is within u32 range (ioctl's request parameter is always u32)
    unsafe { ioctl(fd, request as u32, arg) }
}

/// Query digital video timings (safe wrapper for VIDIOC_QUERY_DV_TIMINGS)
pub fn query_dv_timings(fd: RawFd) -> Result<v4l2_dv_timings> {
    let mut dv_timings = v4l2_dv_timings::default();
    if v4l2_ioctl(
        fd,
        VIDIOC_QUERY_DV_TIMINGS,
        &mut dv_timings as *mut _ as *mut c_void,
    ) < 0 {
        return Err(anyhow!(
            "Failed to query DV timings: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(dv_timings)
}

/// Start video stream（VIDIOC_STREAMON）
pub fn stream_on(fd: RawFd, buf_type: u32) -> Result<()> {
    let mut type_ = buf_type;
    if v4l2_ioctl(fd, VIDIOC_STREAMON, &mut type_ as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!(
            "Failed to start streaming: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Stop video stream（VIDIOC_STREAMOFF）
pub fn stream_off(fd: RawFd, buf_type: u32) -> Result<()> {
    let mut type_ = buf_type;
    if v4l2_ioctl(fd, VIDIOC_STREAMOFF, &mut type_ as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!(
            "Failed to stop streaming: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Get video format (query actual resolution used by driver)
pub fn get_video_format(fd: RawFd) -> Result<(u32, u32)> {
    let mut fmt = v4l2_format {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
        fmt: v4l2_format_fmt {
            pix_mp: v4l2_pix_format_mplane {
                ..Default::default()
            },
        },
    };

    if v4l2_ioctl(fd, VIDIOC_G_FMT, &mut fmt as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!("Failed to get format: {}", std::io::Error::last_os_error()));
    }

    // Safely access union field
    let (width, height) = unsafe { (fmt.fmt.pix_mp.width, fmt.fmt.pix_mp.height) };
    if width == 0 || height == 0 {
        return Err(anyhow!(
            "Invalid resolution: {}x{} (width and height must be > 0)",
            width, height
        ));
    }

    Ok((width, height))
}

/// Set video format
pub fn set_video_format(fd: RawFd, width: u32, height: u32) -> Result<(u32, u32)> {
    // Validate resolution validity: if width or height is 0, reject setting format
    // This prevents using invalid resolution when HDMI is disconnected, avoiding driver returning default resolution
    if width == 0 || height == 0 {
        return Err(anyhow!("Invalid resolution: {}x{} (width and height must be > 0)", width, height));
    }

    let mut fmt = v4l2_format {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
        fmt: v4l2_format_fmt {
            pix_mp: v4l2_pix_format_mplane {
                width,
                height,
                pixelformat: V4L2_PIX_FMT_YUYV,
                field: V4L2_FIELD_ANY,
                ..Default::default()
            },
        },
    };

    if v4l2_ioctl(fd, VIDIOC_S_FMT, &mut fmt as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!("Failed to set format: {}", std::io::Error::last_os_error()));
    }

    // Verify actual resolution returned by driver (VIDIOC_S_FMT may modify resolution)
    // If driver returns different resolution (e.g., default 800x600), log warning
    let (actual_width, actual_height) = unsafe { (fmt.fmt.pix_mp.width, fmt.fmt.pix_mp.height) };
    Ok((actual_width, actual_height))
}

/// Request buffers
pub fn request_buffers(
    fd: RawFd,
    mem_pool: &tokio::sync::RwLock<Option<MB_POOL>>,
) -> Result<Vec<VideoBuffer>> {
    let mut req = v4l2_requestbuffers {
        count: INPUT_BUFFER_COUNT as u32,
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
        memory: V4L2_MEMORY_DMABUF,
        ..Default::default()
    };

    if v4l2_ioctl(fd, VIDIOC_REQBUFS, &mut req as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!("Failed to request buffers: {}", std::io::Error::last_os_error()));
    }


    if req.count == 0 {
        return Err(anyhow!(
            "Driver returned 0 buffers (requested {}), cannot proceed",
            INPUT_BUFFER_COUNT
        ));
    }

    let pool = mem_pool.blocking_read();
    let pool_id = match *pool {
        Some(p) => p,
        None => return Err(anyhow!("Memory pool not initialized")),
    };
    drop(pool);

    // Use actual buffer count returned by driver, but not exceeding requested count
    let buffer_count = std::cmp::min(req.count, INPUT_BUFFER_COUNT as u32) as usize;

    // Pre-create buffer array (matches struct video_buffer buffers[INPUT_BUFFER_COUNT] in C code)
    // Use zeroed to ensure all fields are zeroed (matches memset(buffers, 0, sizeof(buffers)) in C code)
    let mut buffers: Vec<VideoBuffer> = (0..buffer_count)
        .map(|_| unsafe { std::mem::zeroed::<VideoBuffer>() })
        .collect();
    let mut allocated_mb_blks = Vec::new(); // For cleanup on error

    for i in 0..buffer_count {
        // Explicitly zero plane_buffer (ensure consistency with memset(&plane, 0, sizeof(plane)) in C code)
        // Using zeroed() is safer because write_bytes requires ensuring pointer is valid
        buffers[i].plane_buffer = unsafe { std::mem::zeroed() };

        // Use buffers[i].plane_buffer (matches &buffers[i].plane_buffer in C code)
        // Completely zero buf structure, then set necessary fields (matches init_v4l2_buffer in C code)
        let mut buf = unsafe { std::mem::zeroed::<v4l2_buffer>() };
        buf.index = i as u32;
        buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buf.memory = V4L2_MEMORY_DMABUF;
        buf.length = 1; // For multi-planar format, length is number of planes
        // Explicitly convert to *mut v4l2_plane (matches struct v4l2_plane * in C code)
        // Directly use address of buffers[i].plane_buffer
        buf.m.planes = &mut buffers[i].plane_buffer as *mut v4l2_plane;

        if v4l2_ioctl(fd, VIDIOC_QUERYBUF, &mut buf as *mut _ as *mut c_void) < 0 {
            let errno = std::io::Error::last_os_error();
            let errno_raw = errno.raw_os_error();
            // Clean up allocated MB blocks
            for mb_blk in allocated_mb_blks {
                mpi::mb_release_mb(mb_blk);
            }
            return Err(anyhow!(
                "Failed to query buffer {} (requested {} buffers, driver returned {}, fd={}, errno={:?}): {}",
                i,
                INPUT_BUFFER_COUNT,
                req.count,
                fd,
                errno_raw,
                errno
            ));
        }

        // Get length from plane pointed to by buf.m.planes (VIDIOC_QUERYBUF will fill this value)
        let length = unsafe {
            if !buf.m.planes.is_null() {
                (*buf.m.planes).length as usize
            } else {
                buffers[i].plane_buffer.length as usize
            }
        };

        // Get MB block from memory pool
        let mb_blk = match mpi::mb_get_mb(pool_id, length as u64, RK_TRUE as u32) {
            Ok(mb) => mb,
            Err(e) => {
                // Clean up allocated MB blocks
                for mb_blk in allocated_mb_blks {
                    mpi::mb_release_mb(mb_blk);
                }
                return Err(anyhow!("Failed to get MB block for buffer {}: {}", i, e));
            }
        };

        let buf_fd = match mpi::mb_handle2fd(mb_blk) {
            Ok(fd) => fd,
            Err(e) => {
                mpi::mb_release_mb(mb_blk);
                // Clean up allocated MB blocks
                for mb_blk in allocated_mb_blks {
                    mpi::mb_release_mb(mb_blk);
                }
                return Err(anyhow!("Failed to get file descriptor for buffer {}: {}", i, e));
            }
        };

        // Update buffers[i].plane_buffer.m.fd (after VIDIOC_QUERYBUF, plane data is in buffers[i].plane_buffer)
        buffers[i].plane_buffer.m.fd = buf_fd;
        let mapped_addr = mpi::mb_handle2viraddr(mb_blk);

        // Directly update buffers[i] fields (plane already updated via reference)
        buffers[i].mapped_addr = mapped_addr;
        buffers[i].length = length;
        buffers[i].mb_blk = Some(mb_blk);

        allocated_mb_blks.push(mb_blk);
    }

    // Queue all buffers
    for (i, buffer) in buffers.iter().enumerate() {
        let mut buf = unsafe { std::mem::zeroed::<v4l2_buffer>() };
        buf.index = i as u32;
        buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        buf.memory = V4L2_MEMORY_DMABUF;
        buf.length = 1;
        buf.m.planes = &buffer.plane_buffer as *const v4l2_plane as *mut v4l2_plane;

        if v4l2_ioctl(fd, VIDIOC_QBUF, &mut buf as *mut _ as *mut c_void) < 0 {
            // Clean up allocated MB blocks
            for buffer in &buffers {
                if let Some(mb_blk) = buffer.mb_blk {
                    mpi::mb_release_mb(mb_blk);
                }
            }
            return Err(anyhow!(
                "Failed to queue buffer {}: {}",
                i,
                std::io::Error::last_os_error()
            ));
        }
    }

    Ok(buffers)
}

/// Dequeue buffer
pub fn dequeue_buffer(fd: RawFd) -> Result<(usize, usize)> {
    let mut buf = unsafe { std::mem::zeroed::<v4l2_buffer>() };
    buf.index = 0;
    buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    buf.memory = V4L2_MEMORY_DMABUF;
    buf.length = 1;

    // Explicitly zero plane structure (matches memset(&plane, 0, sizeof(plane)) in C code)
    let mut plane = unsafe { std::mem::zeroed::<v4l2_plane>() };
    buf.m.planes = &mut plane as *mut v4l2_plane;

    if v4l2_ioctl(fd, VIDIOC_DQBUF, &mut buf as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!("Failed to dequeue buffer: {}", std::io::Error::last_os_error()));
    }

    Ok((buf.index as usize, plane.bytesused as usize))
}

/// Queue buffer
pub fn queue_buffer(fd: RawFd, index: usize, buffers: &[VideoBuffer]) -> Result<()> {
    let buffer = buffers.get(index).ok_or_else(|| anyhow!("Invalid buffer index"))?;

    let mut buf = unsafe { std::mem::zeroed::<v4l2_buffer>() };
    buf.index = index as u32;
    buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
    buf.memory = V4L2_MEMORY_DMABUF;
    buf.length = 1;

        // Use pointer to buffer.plane_buffer (matches &buffers[i].plane_buffer in C code)
        // Note: Although buffer is an immutable reference, kernel will only read plane_buffer.m.fd field, not modify Rust memory
    buf.m.planes = &buffer.plane_buffer as *const v4l2_plane as *mut v4l2_plane;

    if v4l2_ioctl(fd, VIDIOC_QBUF, &mut buf as *mut _ as *mut c_void) < 0 {
        return Err(anyhow!("Failed to requeue buffer: {}", std::io::Error::last_os_error()));
    }

    Ok(())
}
