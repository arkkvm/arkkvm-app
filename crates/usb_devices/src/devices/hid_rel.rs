use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use tracing::{info, warn};

use super::hid_io::{prepare_hid_write_file, write_all_with_timeout, HID_WRITE_TIMEOUT};

use crate::config::{HID_REL_INSTANCE, HID_REL_PROTOCOL, HID_REL_REPORT_LENGTH, HID_REL_SUBCLASS};
use crate::events::{ChangeRequest, ChangeResponse, UsbChangeHandler};

const IO_CHANNEL_CAP: usize = 32;

#[derive(Debug)]
enum HidRelCmd {
    Stop,
    Open(PathBuf),
    RelMouseReport([u8; 5]),
}

pub struct HidRelController {
    handle: Arc<HidRelHandle>,
}

impl HidRelController {
    pub fn new(handle: Arc<HidRelHandle>) -> Self {
        Self { handle }
    }

    pub async fn rel_mouse_report(&self, dx: i8, dy: i8, btns: u8) {
        self.handle.submit(HidRelCmd::RelMouseReport([
            1, btns, dx as u8, dy as u8, 0,
        ]));
    }
}

pub struct HidRelHandle {
    name: String,
    enabled: AtomicBool,
    udc_updating: AtomicBool,
    warned_disabled: AtomicBool,
    warned_updating: AtomicBool,
    device_path: tokio::sync::Mutex<Option<PathBuf>>,
    cmd_tx: SyncSender<HidRelCmd>,
    _io_handle: JoinHandle<()>,
}

impl HidRelHandle {
    pub fn new(enabled: bool) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(IO_CHANNEL_CAP);
        let io_handle = thread::spawn(move || hid_rel_io_loop(cmd_rx));
        Arc::new(Self {
            name: HID_REL_INSTANCE.to_owned(),
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
            let _ = self.cmd_tx.send(HidRelCmd::Stop);
        }
    }

    fn submit(&self, cmd: HidRelCmd) {
        if !self.enabled.load(Ordering::Acquire) {
            if !self.warned_disabled.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidRel: device disabled, dropping reports");
            }
            return;
        }
        if self.udc_updating.load(Ordering::Acquire) {
            if !self.warned_updating.swap(true, Ordering::AcqRel) {
                warn!(device = %self.name, "HidRel: gadget updating, dropping reports");
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
            HID_REL_PROTOCOL,
            HID_REL_SUBCLASS,
            HID_REL_REPORT_LENGTH,
        )
        .await
        {
            Ok(path) => *cache = Some(path),
            Err(e) => {
                warn!(device = %self.name, error = ?e, "HidRel: failed to resolve device path");
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
            let _ = self.cmd_tx.send(HidRelCmd::Open(path));
        }
    }

    fn close_device(&self) {
        let _ = self.cmd_tx.send(HidRelCmd::Stop);
    }
}

fn hid_rel_io_loop(cmd_rx: mpsc::Receiver<HidRelCmd>) {
    let mut file: Option<std::fs::File> = None;
    let mut warned_open = false;
    let mut warned_write = false;

    info!("HidRel IO thread started");

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
            HidRelCmd::Stop => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
            }
            HidRelCmd::Open(path) => {
                drop(file.take());
                warned_open = false;
                warned_write = false;
                match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => {
                        prepare_hid_write_file(&f);
                        file = Some(f);
                        info!(path = %path.display(), "HidRel: device opened");
                    }
                    Err(e) => {
                        if !warned_open {
                            warned_open = true;
                            warn!(path = %path.display(), error = %e, "HidRel: failed to open device");
                        }
                    }
                }
            }
            HidRelCmd::RelMouseReport(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = write_all_with_timeout(f, &d, HID_WRITE_TIMEOUT) {
                        if !warned_write {
                            warned_write = true;
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!(len = d.len(), "HidRel: rel mouse report write timed out");
                            } else {
                                warn!(
                                    len = d.len(),
                                    error = %e,
                                    kind = ?e.kind(),
                                    "HidRel: rel mouse report write failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    info!("HidRel IO thread stopped");
}

#[async_trait]
impl UsbChangeHandler for HidRelHandle {
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
