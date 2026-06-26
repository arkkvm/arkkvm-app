use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use tracing::{info, warn};

use super::hid_io::{prepare_hid_write_file, write_all_with_timeout, HID_WRITE_TIMEOUT};

use crate::config::{HID_ABS_INSTANCE, HID_ABS_PROTOCOL, HID_ABS_REPORT_LENGTH, HID_ABS_SUBCLASS};
use crate::events::{ChangeRequest, ChangeResponse, UsbChangeHandler};

const IO_CHANNEL_CAP: usize = 32;

#[derive(Debug)]
enum HidAbsCmd {
    Stop,
    Open(PathBuf),
    AbsMouseReport([u8; 6]),
    MouseWheelReport([u8; 2]),
}

pub struct HidAbsController {
    handle: Arc<HidAbsHandle>,
}

impl HidAbsController {
    pub fn new(handle: Arc<HidAbsHandle>) -> Self {
        Self { handle }
    }

    pub async fn abs_mouse_report(&self, x: i32, y: i32, btns: u8) {
        let mut d = [0u8; 6];
        d[0] = 1;
        d[1] = btns;
        d[2] = (x & 0xff) as u8;
        d[3] = ((x >> 8) & 0xff) as u8;
        d[4] = (y & 0xff) as u8;
        d[5] = ((y >> 8) & 0xff) as u8;
        self.handle.submit(HidAbsCmd::AbsMouseReport(d));
    }

    pub async fn abs_mouse_wheel(&self, y: i8) {
        if y == 0 {
            return;
        }
        self.handle.submit(HidAbsCmd::MouseWheelReport([2, y as u8]));
    }
}

pub struct HidAbsHandle {
    name: String,
    enabled: AtomicBool,
    udc_updating: AtomicBool,
    warned_disabled: AtomicBool,
    warned_updating: AtomicBool,
    device_path: tokio::sync::Mutex<Option<PathBuf>>,
    cmd_tx: SyncSender<HidAbsCmd>,
    _io_handle: JoinHandle<()>,
}

impl HidAbsHandle {
    pub fn new(enabled: bool) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(IO_CHANNEL_CAP);
        let io_handle = thread::spawn(move || hid_abs_io_loop(cmd_rx));
        Arc::new(Self {
            name: HID_ABS_INSTANCE.to_owned(),
            enabled: AtomicBool::new(enabled),
            udc_updating: AtomicBool::new(false),
            warned_disabled: AtomicBool::new(false),
            warned_updating: AtomicBool::new(false),
            device_path: tokio::sync::Mutex::new(None),
            cmd_tx,
            _io_handle: io_handle,
        })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if enabled {
            self.warned_disabled.store(false, Ordering::Release);
        } else {
            let _ = self.cmd_tx.send(HidAbsCmd::Stop);
        }
    }

    fn submit(&self, cmd: HidAbsCmd) {
        if !self.enabled.load(Ordering::Acquire) {
            if !self.warned_disabled.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidAbs: device disabled, dropping reports");
            }
            return;
        }
        if self.udc_updating.load(Ordering::Acquire) {
            if !self.warned_updating.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidAbs: gadget updating, dropping reports");
            }
            return;
        }
        let _ = self.cmd_tx.send(cmd);
    }

    async fn sync_device_path(&self) {
        let mut cache = self.device_path.lock().await;
        if !self.enabled.load(Ordering::Acquire) {
            *cache = None;
            return;
        }
        match super::resolve_hid_device_path(
            HID_ABS_PROTOCOL,
            HID_ABS_SUBCLASS,
            HID_ABS_REPORT_LENGTH,
        )
        .await
        {
            Ok(path) => *cache = Some(path),
            Err(e) => {
                warn!(device = %self.name, error = ?e, "HidAbs: failed to resolve device path");
                *cache = None;
            }
        }
    }

    async fn open_device_if_ready(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            warn!(device = %self.name, "HidAbs: device disabled, skipping open");
            return;
        }
        let path = self.device_path.lock().await.clone();
        if let Some(path) = path {
            let _ = self.cmd_tx.send(HidAbsCmd::Open(path));
        }
    }

    fn close_device(&self) {
        let _ = self.cmd_tx.send(HidAbsCmd::Stop);
    }
}

fn hid_abs_io_loop(cmd_rx: mpsc::Receiver<HidAbsCmd>) {
    let mut file: Option<std::fs::File> = None;
    let mut warned_open = false;
    let mut warned_write = false;

    info!("HidAbs IO thread started");

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
            HidAbsCmd::Stop => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
            }
            HidAbsCmd::Open(path) => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
                match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => {
                        prepare_hid_write_file(&f);
                        file = Some(f);
                        info!(path = %path.display(), "HidAbs: device opened");
                    }
                    Err(e) => {
                        if !warned_open {
                            warned_open = true;
                            warn!(path = %path.display(), error = %e, "HidAbs: failed to open device");
                        }
                    }
                }
            }
            HidAbsCmd::AbsMouseReport(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = write_all_with_timeout(f, &d, HID_WRITE_TIMEOUT) {
                        if !warned_write {
                            warned_write = true;
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!(len = d.len(), "HidAbs: abs mouse report write timed out");
                            } else {
                                warn!(
                                    len = d.len(),
                                    error = %e,
                                    kind = ?e.kind(),
                                    "HidAbs: abs mouse report write failed"
                                );
                            }
                        }
                    }
                }
            }
            HidAbsCmd::MouseWheelReport(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = write_all_with_timeout(f, &d, HID_WRITE_TIMEOUT) {
                        if !warned_write {
                            warned_write = true;
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!(len = d.len(), "HidAbs: mouse wheel report write timed out");
                            } else {
                                warn!(
                                    len = d.len(),
                                    error = %e,
                                    kind = ?e.kind(),
                                    "HidAbs: mouse wheel report write failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    info!("HidAbs IO thread stopped");
}

#[async_trait]
impl UsbChangeHandler for HidAbsHandle {
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
