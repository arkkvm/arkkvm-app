use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TokioJoinHandle;
use tracing::{info, warn};

use super::hid_io::{prepare_hid_write_file, set_std_file_non_blocking, write_all_with_timeout, HID_WRITE_TIMEOUT};

use crate::config::{
    HID_KB_REL_INSTANCE, HID_KB_REL_PROTOCOL, HID_KB_REL_REPORT_LENGTH, HID_KB_REL_SUBCLASS,
};
use crate::events::{ChangeRequest, ChangeResponse, UsbChangeHandler};
use crate::proto::v1::KeyboardState;

const IO_CHANNEL_CAP: usize = 32;
const LED_CMD_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum HidKbWriteCmd {
    Stop,
    Open(PathBuf),
    KeyboardReport([u8; 9]),
    RelMouseReport([u8; 5]),
}

#[derive(Debug)]
enum HidKbLedCmd {
    Stop,
    Open(PathBuf),
}

pub struct HidKbRelController {
    handle: Arc<HidKbRelHandle>,
}

impl HidKbRelController {
    pub fn new(handle: Arc<HidKbRelHandle>) -> Self {
        Self { handle }
    }

    pub async fn keyboard_report(&self, mods: u8, keys: &[u8]) {
        let mut d = [0u8; 9];
        d[0] = 1;
        d[1] = mods;
        d[3..3 + keys.len().min(6)].copy_from_slice(keys);
        self.handle.submit_write(HidKbWriteCmd::KeyboardReport(d));
    }

    pub async fn rel_mouse_report(&self, dx: i8, dy: i8, btns: u8) {
        self.handle.submit_write(HidKbWriteCmd::RelMouseReport([
            2, btns, dx as u8, dy as u8, 0,
        ]));
    }
}

pub struct HidKbRelHandle {
    name: String,
    enabled: AtomicBool,
    udc_updating: AtomicBool,
    warned_disabled: AtomicBool,
    warned_updating: AtomicBool,
    device_path: tokio::sync::Mutex<Option<PathBuf>>,
    write_tx: SyncSender<HidKbWriteCmd>,
    led_tx: SyncSender<HidKbLedCmd>,
    _write_handle: JoinHandle<()>,
    _led_handle: JoinHandle<()>,
    _led_publisher: TokioJoinHandle<()>,
}

impl HidKbRelHandle {
    pub fn new(enabled: bool) -> Arc<Self> {
        let (write_tx, write_rx) = std_mpsc::sync_channel(IO_CHANNEL_CAP);
        let (led_tx, led_rx) = std_mpsc::sync_channel(IO_CHANNEL_CAP);
        let (led_event_tx, mut led_event_rx) = mpsc::channel(IO_CHANNEL_CAP);
        let led_publisher = tokio::spawn(async move {
            while let Some(state) = led_event_rx.recv().await {
                if let Err(e) = crate::control::zenoh::send_keyboard_led_event(state).await {
                    warn!("HidKbRel: failed to publish keyboard LED event: {}", e);
                }
            }
        });
        let led_event_tx_for_thread = led_event_tx;
        let write_handle = thread::spawn(move || hid_kb_write_loop(write_rx));
        let led_handle =
            thread::spawn(move || hid_kb_led_loop(led_rx, led_event_tx_for_thread));
        Arc::new(Self {
            name: HID_KB_REL_INSTANCE.to_owned(),
            enabled: AtomicBool::new(enabled),
            udc_updating: AtomicBool::new(false),
            warned_disabled: AtomicBool::new(false),
            warned_updating: AtomicBool::new(false),
            device_path: tokio::sync::Mutex::new(None),
            write_tx,
            led_tx,
            _write_handle: write_handle,
            _led_handle: led_handle,
            _led_publisher: led_publisher,
        })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if enabled {
            self.warned_disabled.store(false, Ordering::Release);
        } else {
            self.stop_io_threads();
        }
    }

    fn submit_write(&self, cmd: HidKbWriteCmd) {
        if !self.enabled.load(Ordering::Acquire) {
            if !self.warned_disabled.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidKbRel: device disabled, dropping reports");
            }
            return;
        }
        if self.udc_updating.load(Ordering::Acquire) {
            if !self.warned_updating.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidKbRel: gadget updating, dropping reports");
            }
            return;
        }
        let _ = self.write_tx.send(cmd);
    }

    async fn sync_device_path(&self) {
        let mut cache = self.device_path.lock().await;
        if !self.enabled.load(Ordering::Acquire) {
            *cache = None;
            return;
        }
        match super::resolve_hid_device_path(
            HID_KB_REL_PROTOCOL,
            HID_KB_REL_SUBCLASS,
            HID_KB_REL_REPORT_LENGTH,
        )
        .await
        {
            Ok(path) => *cache = Some(path),
            Err(e) => {
                warn!(device = %self.name, error = ?e, "HidKbRel: failed to resolve device path");
                *cache = None;
            }
        }
    }

    async fn open_device_if_ready(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let path = self.device_path.lock().await.clone();
        if let Some(path) = path {
            let _ = self.write_tx.send(HidKbWriteCmd::Open(path.clone()));
            let _ = self.led_tx.send(HidKbLedCmd::Open(path));
        }
    }

    fn stop_io_threads(&self) {
        let _ = self.write_tx.send(HidKbWriteCmd::Stop);
        let _ = self.led_tx.send(HidKbLedCmd::Stop);
    }

    fn close_device(&self) {
        self.stop_io_threads();
    }
}

fn parse_keyboard_state(byte: u8) -> KeyboardState {
    KeyboardState {
        num_lock: byte & 0x01 != 0,
        caps_lock: byte & 0x02 != 0,
        scroll_lock: byte & 0x04 != 0,
        compose: byte & 0x08 != 0,
        kana: byte & 0x10 != 0,
    }
}

fn hid_kb_write_loop(cmd_rx: std_mpsc::Receiver<HidKbWriteCmd>) {
    let mut file: Option<std::fs::File> = None;
    let mut warned_open = false;
    let mut warned_write = false;

    info!("HidKbRel write thread started");

    loop {
        let cmd = if file.is_some() {
            match cmd_rx.recv_timeout(HID_WRITE_TIMEOUT) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };

        let Some(cmd) = cmd else {
            continue;
        };

        match cmd {
            HidKbWriteCmd::Stop => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
            }
            HidKbWriteCmd::Open(path) => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
                match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => {
                        prepare_hid_write_file(&f);
                        file = Some(f);
                        info!(path = %path.display(), "HidKbRel: write device opened");
                    }
                    Err(e) => {
                        if !warned_open {
                            warned_open = true;
                            warn!(path = %path.display(), error = %e, "HidKbRel: failed to open write device");
                        }
                    }
                }
            }
            HidKbWriteCmd::KeyboardReport(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = write_all_with_timeout(f, &d, HID_WRITE_TIMEOUT) {
                        if !warned_write {
                            warned_write = true;
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!(len = d.len(), "HidKbRel: keyboard report write timed out");
                            } else {
                                warn!(
                                    len = d.len(),
                                    error = %e,
                                    kind = ?e.kind(),
                                    "HidKbRel: keyboard report write failed"
                                );
                            }
                        }
                    }
                }
            }
            HidKbWriteCmd::RelMouseReport(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = write_all_with_timeout(f, &d, HID_WRITE_TIMEOUT) {
                        if !warned_write {
                            warned_write = true;
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!(len = d.len(), "HidKbRel: rel mouse report write timed out");
                            } else {
                                warn!(
                                    len = d.len(),
                                    error = %e,
                                    kind = ?e.kind(),
                                    "HidKbRel: rel mouse report write failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    info!("HidKbRel write thread stopped");
}

fn hid_kb_led_loop(cmd_rx: std_mpsc::Receiver<HidKbLedCmd>, led_pub_tx: mpsc::Sender<KeyboardState>) {
    let mut file: Option<std::fs::File> = None;
    let mut last_led: Option<KeyboardState> = None;
    let mut led_buf = [0u8; 8];
    let mut warned_open = false;
    let mut warned_read = false;

    info!("HidKbRel LED thread started");

    loop {
        let cmd = if file.is_some() {
            match cmd_rx.recv_timeout(LED_CMD_POLL) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match cmd_rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };

        if let Some(cmd) = cmd {
            match cmd {
                HidKbLedCmd::Stop => {
                    drop(file.take());
                    last_led = None;
                    warned_open = false;
                    warned_read = false;
                }
                HidKbLedCmd::Open(path) => {
                    drop(file.take());
                    last_led = None;
                    warned_open = false;
                    warned_read = false;
                    match std::fs::OpenOptions::new().read(true).open(&path) {
                        Ok(f) => {
                            let _ = set_std_file_non_blocking(&f);
                            file = Some(f);
                            info!(path = %path.display(), "HidKbRel: LED device opened");
                        }
                        Err(e) => {
                            if !warned_open {
                                warned_open = true;
                                warn!(path = %path.display(), error = %e, "HidKbRel: failed to open LED device");
                            }
                        }
                    }
                }
            }
        }

        if let Some(f) = file.as_mut() {
            loop {
                match f.read(&mut led_buf) {
                    Ok(n) if n >= 2 => {
                        warned_read = false;
                        let state = parse_keyboard_state(led_buf[1]);
                        if last_led.replace(state) != Some(state) {
                            let _ = led_pub_tx.blocking_send(state);
                        }
                    }
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        drop(file.take());
                        last_led = None;
                        if !warned_read {
                            warned_read = true;
                            warn!(error = %e, kind = ?e.kind(), "HidKbRel: LED read failed");
                        }
                        break;
                    }
                }
            }
        }
    }

    info!("HidKbRel LED thread stopped");
}

#[async_trait]
impl UsbChangeHandler for HidKbRelHandle {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    async fn on_change_request(&self, req: &ChangeRequest) -> ChangeResponse {
        match req {
            ChangeRequest::RequestChange => {
                self.udc_updating.store(true, Ordering::Release);
                ChangeResponse::Proceed
            }
            ChangeRequest::PrepareChange => {
                self.close_device();
                *self.device_path.lock().await = None;
                ChangeResponse::Proceed
            }
            ChangeRequest::ChangeCompleted | ChangeRequest::ChangeCanceled => {
                self.sync_device_path().await;
                self.open_device_if_ready().await;
                self.udc_updating.store(false, Ordering::Release);
                self.warned_updating.store(false, Ordering::Release);
                ChangeResponse::Proceed
            }
        }
    }
}
