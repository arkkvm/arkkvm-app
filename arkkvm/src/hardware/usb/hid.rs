//! USB HID keyboard/mouse control.
//!
//! - Keyboard output report writer and LED state reader (`/dev/hidg0`)
//! - Relative mouse report writer (`/dev/hidg0` - Report ID 2)
//! - Absolute mouse report writer (`/dev/hidg1` - Report ID 1 and 2)
//! - Keyboard LED state notifications via callback
//!
//! Safety:
//! - All file IO is best-effort and error-propagating
//! - Background readers are cancellable to avoid thread leaks
//!
//! Architecture:
//! - HID write path: async callers send via async_channel (send().await); a dedicated std thread
//!   runs write_task with recv_blocking() and synchronous file I/O.
//! - Keyboard LED reader remains a tokio task.

use std::future::Future;
use std::io::{ErrorKind, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn, error};

pub const HID_PATH: &str = "/dev/hidg0";
pub const ABS_MOUSE_PATH: &str = "/dev/hidg1";

/// Keyboard LED bit masks (USB HID spec)
pub const KEYBOARD_LED_MASK_NUM_LOCK: u8 = 1 << 0;
pub const KEYBOARD_LED_MASK_CAPS_LOCK: u8 = 1 << 1;
pub const KEYBOARD_LED_MASK_SCROLL_LOCK: u8 = 1 << 2;
pub const KEYBOARD_LED_MASK_COMPOSE: u8 = 1 << 3;
pub const KEYBOARD_LED_MASK_KANA: u8 = 1 << 4;
pub const KEYBOARD_LED_VALID_MASKS: u8 = KEYBOARD_LED_MASK_NUM_LOCK
    | KEYBOARD_LED_MASK_CAPS_LOCK
    | KEYBOARD_LED_MASK_SCROLL_LOCK
    | KEYBOARD_LED_MASK_COMPOSE
    | KEYBOARD_LED_MASK_KANA;

/// HID report IDs
const KEYBOARD_REPORT_ID: u8 = 1;
const ABS_MOUSE_REPORT_ID: u8 = 1; // Absolute mouse (matches Report ID 1 in mouse.rs)
const ABS_MOUSE_WHEEL_REPORT_ID: u8 = 2; // Mouse wheel (matches Report ID 2 in mouse.rs)
const REL_MOUSE_REPORT_ID: u8 = 2; // Relative mouse (Report ID 2 in hid.usb0)

/// Bounded channel capacity for HID write requests
const WRITE_CHANNEL_CAP: usize = 32;

/// Async callback: takes KeyboardState, returns a future. Call site uses `.await`.
// type KeyboardStateCallback = Box<
//     dyn Fn(KeyboardState) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
// >;

/// Keyboard LED state
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct KeyboardState {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
    pub compose: bool,
    pub kana: bool,
}

fn parse_keyboard_state(byte: u8) -> KeyboardState {
    KeyboardState {
        num_lock: byte & KEYBOARD_LED_MASK_NUM_LOCK != 0,
        caps_lock: byte & KEYBOARD_LED_MASK_CAPS_LOCK != 0,
        scroll_lock: byte & KEYBOARD_LED_MASK_SCROLL_LOCK != 0,
        compose: byte & KEYBOARD_LED_MASK_COMPOSE != 0,
        kana: byte & KEYBOARD_LED_MASK_KANA != 0,
    }
}

/// Set file descriptor to non-blocking mode (in-place; used for absolute mouse in write thread).
fn set_std_file_non_blocking(file: &std::fs::File) -> anyhow::Result<()> {
    let fd = file.as_fd();
    let flags = fcntl_getfl(fd).map_err(|e| anyhow::anyhow!("fcntl_getfl failed: {}", e))?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)
        .map_err(|e| anyhow::anyhow!("fcntl_setfl failed: {}", e))?;
    Ok(())
}

/// Internal write request message (fixed-size arrays to avoid heap allocation on hot path)
#[derive(Debug)]
enum WriteRequest {
    /// Keyboard report; write to /dev/hidg0
    KeyboardReport([u8; 9]),
    /// Relative mouse report; write to /dev/hidg0
    RelativeMouseReport([u8; 5]),
    /// Absolute mouse position report; write to /dev/hidg1
    AbsoluteMouseReport([u8; 6]),
    /// Mouse wheel report; write to /dev/hidg1
    MouseWheelReport([u8; 2]),
}

/// HID device manager
///
/// Files:
/// - `/dev/hidg0`: Keyboard + Relative Mouse (Report ID 1 and 2)
/// - `/dev/hidg1`: Absolute Mouse (Report ID 1 and 2)
///
/// Architecture:
/// - Write path: async_channel; senders use send().await, a std thread runs write_task with
///   recv_blocking() and synchronous file I/O.
/// - Keyboard LED reader remains a tokio task.
pub struct Hid {
    hid_path: String,       // /dev/hidg0 - Keyboard + Relative Mouse
    abs_mouse_path: String, // /dev/hidg1 - Absolute Mouse
    write_tx: Option<async_channel::Sender<WriteRequest>>,
    write_handle: Option<thread::JoinHandle<()>>,

    hid_reader_cancel: Arc<AtomicBool>,
    hid_reader_handle: Arc<RwLock<Option<JoinHandle<()>>>>,

    // on_keyboard_state_change: Arc<RwLock<Option<KeyboardStateCallback>>>,

    last_user_input: Arc<RwLock<Instant>>,
}

impl Default for Hid {
    fn default() -> Self {
        Self::new(HID_PATH.to_owned(), ABS_MOUSE_PATH.to_owned())
    }
}

impl Drop for Hid {
    fn drop(&mut self) {
        info!("Dropping Hid, cleaning up resources");

        self.hid_reader_cancel.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.hid_reader_handle.try_write() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }

        let write_tx = self.write_tx.take();
        drop(write_tx);
        if let Some(handle) = self.write_handle.take() {
            let _ = handle.join();
        }

        info!("Hid resources cleaned up");
    }
}

impl Hid {
    /// Create a new HID manager with explicit device paths.
    /// Can be called from any context; write task runs in a dedicated std thread.
    pub fn new(hid_path: String, abs_mouse_path: String) -> Self {
        let (write_tx, write_rx) = async_channel::bounded(WRITE_CHANNEL_CAP);
        let hid_path_clone = hid_path.clone();
        let abs_mouse_path_clone = abs_mouse_path.clone();
        let write_handle = thread::spawn(move || {
            Self::write_task(hid_path_clone, abs_mouse_path_clone, write_rx);
        });

        Self {
            hid_path,
            abs_mouse_path,
            write_tx: Some(write_tx),
            write_handle: Some(write_handle),
            hid_reader_cancel: Arc::new(AtomicBool::new(false)),
            hid_reader_handle: Arc::new(RwLock::new(None)),
            // on_keyboard_state_change: Arc::new(RwLock::new(None)),
            last_user_input: Arc::new(RwLock::new(Instant::now())),
        }
    }

    /// Runs in a dedicated std thread; blocks on recv_blocking() and uses synchronous file I/O.
    fn write_task(
        hid_path: String,
        abs_mouse_path: String,
        write_rx: async_channel::Receiver<WriteRequest>,
    ) {
        loop {
            let mut hid_file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&hid_path)
            {
                Ok(file) => file,
                Err(e) => {
                    warn!("Failed to open HID file: {}", e);
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            let mut abs_mouse_file = match std::fs::OpenOptions::new()
                .write(true)
                .open(&abs_mouse_path)
            {
                Ok(file) => {
                    if let Err(e) = set_std_file_non_blocking(&file) {
                        warn!("Failed to set absolute mouse file to non-blocking: {}", e);
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    file
                }
                Err(e) => {
                    warn!("Failed to open absolute mouse file: {}", e);
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            info!("Write task started, files opened");
            let mut is_close = false;
            loop {
                match write_rx.recv_blocking() {
                    Ok(request) => {
                        match request {
                            WriteRequest::KeyboardReport(data) => {
                                if let Err(e) = hid_file.write_all(&data) {
                                    error!("Failed to write keyboard report: {:?}", e);
                                    break;
                                }
                            }
                            WriteRequest::RelativeMouseReport(data) => {
                                if let Err(e) = hid_file.write_all(&data) {
                                    error!("Failed to write relative mouse report: {:?}", e);
                                    break;
                                }
                            }
                            WriteRequest::AbsoluteMouseReport(data) => {
                                if let Err(e) = abs_mouse_file.write_all(&data) {
                                    if e.kind() == ErrorKind::WouldBlock {
                                        warn!("absolute mouse file would block");
                                    } else {
                                        error!("Failed to write absolute mouse report: {:?}", e);
                                        break;
                                    }
                                } else {
                                    thread::sleep(Duration::from_millis(1));
                                }
                            }
                            WriteRequest::MouseWheelReport(data) => {
                                if let Err(e) = abs_mouse_file.write_all(&data) {
                                    if e.kind() == ErrorKind::WouldBlock {
                                        warn!("absolute mouse file would block");
                                    } else {
                                        error!("Failed to write mouse wheel report: {:?}", e);
                                        break;
                                    }
                                } else {
                                    thread::sleep(Duration::from_millis(1));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Write task exiting, channel closed: {:?}", e);
                        is_close = true;
                        break;
                    }
                }
            }

            drop(hid_file);
            drop(abs_mouse_file);

            if is_close {
                break;
            }

            thread::sleep(Duration::from_millis(100));
        }
        info!("Write task exited");
    }

    async fn send_write_request(&self, request: WriteRequest) -> anyhow::Result<()> {
        let Some(tx) = self.write_tx.as_ref() else {
            return Err(anyhow::anyhow!("Write channel not available"));
        };
        tx.send(request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send HID write request: {}", e))?;
        Ok(())
    }

    /// Set async callback for keyboard LED state changes.
    /// The callback is invoked in the keyboard LED reader task; it may use `.await`.
    // pub async fn set_on_keyboard_state_change<F, Fut>(&self, f: F)
    // where
    //     F: Fn(KeyboardState) -> Fut + Send + Sync + 'static,
    //     Fut: Future<Output = ()> + Send + 'static,
    // {
    //     let mut guard = self.on_keyboard_state_change.write().await;
    //     let f = Arc::new(f);
    //     let f_ref = Arc::clone(&f);
    //     *guard = Some(Box::new(move |state: KeyboardState| {
    //         let f = Arc::clone(&f_ref);
    //         Box::pin(async move { f(state).await })
    //     }));
    // }

    /// Atomically checks "already running" and stores the new handle under write lock to avoid TOCTOU.
    pub async fn start_keyboard_led_monitor(&self) -> anyhow::Result<()> {
        let mut guard = self.hid_reader_handle.write().await;
        if guard.is_some() {
            return Ok(());
        }

        self.hid_reader_cancel.store(false, Ordering::Relaxed);

        loop {
            if Path::new(&self.hid_path).exists() {
                break;
            }

            if self.hid_reader_cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            warn!("HID file not found, waiting for 500ms");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let reader_cancel = Arc::clone(&self.hid_reader_cancel);
        let hid_path_clone = self.hid_path.clone();
        // let on_change = self.on_keyboard_state_change.clone();
        let handle = tokio::spawn(async move {
            let mut file_for_read = match tokio::fs::File::open(&hid_path_clone).await {
                Ok(file) => file,
                Err(e) => {
                    warn!("Failed to open HID file for reading: {}", e);
                    return;
                }
            };

            let mut last_state: Option<KeyboardState> = None;
            let mut buf = [0u8; 8];
            loop {
                tokio::select! {
                    _ = sleep(Duration::from_millis(200)) => {
                        if reader_cancel.load(Ordering::Relaxed) {
                            info!("keyboard reader cancelled");
                            break;
                        }
                    }
                    result = file_for_read.read(&mut buf) => {
                        match result {
                            Ok(0) => continue,
                            Ok(n) => {
                                debug!("read from keyboard hid: n={}", n);
                                if n < 2 {
                                    continue;
                                }
                                let b = buf[1];
                                if b & !KEYBOARD_LED_VALID_MASKS != 0 {
                                    debug!("ignored invalid LED bits: {:02x}", b);
                                    continue;
                                }
                                let new_state = parse_keyboard_state(b);
                                let changed = last_state.as_ref() != Some(&new_state);
                                if changed {
                                    crate::jsonrpc::broadcast_keyboard_led_state(new_state).await;
                                    last_state = Some(new_state);
                                }
                            }
                            Err(e) => {
                                warn!("keyboard reader error: {}", e);
                                sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });

        *guard = Some(handle);
        Ok(())
    }

    /// Send keyboard report: modifier + up to 6 keys (padded with zeros)
    pub async fn keyboard_report(&self, modifier: u8, keys: &[u8]) -> anyhow::Result<()> {
        // self.start_keyboard_led_monitor()?;

        let mut report = [0u8; 9];
        report[0] = KEYBOARD_REPORT_ID;
        report[1] = modifier;
        report[2] = 0; // reserved
        let n = keys.len().min(6);
        report[3..3 + n].copy_from_slice(&keys[..n]);
        self.send_write_request(WriteRequest::KeyboardReport(report)).await?;
        self.reset_user_input_time().await;
        Ok(())
    }

    /// Absolute mouse report for `/dev/hidg1` (Report ID 1)
    pub async fn abs_mouse_report(
        &self,
        x: i32,
        y: i32,
        buttons: u8,
        by_user: bool,
    ) -> anyhow::Result<()> {
        let mut data = [0u8; 6];
        data[0] = ABS_MOUSE_REPORT_ID; // Report ID 1 (matches mouse.rs)
        data[1] = buttons;
        data[2] = (x as u16 & 0x00FF) as u8;
        data[3] = ((x as u16 >> 8) & 0x00FF) as u8;
        data[4] = (y as u16 & 0x00FF) as u8;
        data[5] = ((y as u16 >> 8) & 0x00FF) as u8;
        self.send_write_request(WriteRequest::AbsoluteMouseReport(data)).await?;
        if by_user {
            self.reset_user_input_time().await;
        }
        Ok(())
    }

    /// Absolute mouse wheel report for `/dev/hidg1` (Report ID 2)
    pub async fn abs_mouse_wheel_report(&self, wheel_y: i8) -> anyhow::Result<()> {
        if wheel_y == 0 {
            return Ok(());
        }
        let data = [ABS_MOUSE_WHEEL_REPORT_ID, wheel_y as u8]; // Report ID 2 (matches mouse.rs)
        self.send_write_request(WriteRequest::MouseWheelReport(data)).await?;
        self.reset_user_input_time().await;
        Ok(())
    }

    /// Relative mouse report for `/dev/hidg0` (Report ID 2)
    pub async fn rel_mouse_report(
        &self,
        dx: i8,
        dy: i8,
        buttons: u8,
        by_user: bool,
    ) -> anyhow::Result<()> {
        let data = [REL_MOUSE_REPORT_ID, buttons, dx as u8, dy as u8, 0u8];
        self.send_write_request(WriteRequest::RelativeMouseReport(data)).await?;
        if by_user {
            self.reset_user_input_time().await;
        }
        Ok(())
    }

    /// Get last user input timestamp offset in seconds
    pub async fn get_last_user_input_time_offset(&self) -> u64 {
        self.last_user_input.read().await.elapsed().as_secs()
    }

    async fn reset_user_input_time(&self) {
        let mut guard = self.last_user_input.write().await;
        if guard.elapsed().as_secs() > 1 {
            *guard = Instant::now();
        }
    }
}
