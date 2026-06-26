use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::config::UAC1_INSTANCE;
use crate::error::UsbError;
use crate::events::{ChangeRequest, ChangeResponse, UsbChangeHandler};

const ARKKVM_MIC_NAME: &str = "arkkvm_mic";
const ARKKVM_MIC_PATH: &str = "/oem/usr/bin/arkkvm_mic";
const RESTART_DELAY: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

enum UacCmd {
    Stop { ack: mpsc::Sender<()> },
    SyncState {
        device_enabled: bool,
        process_enabled: bool,
        ack: mpsc::Sender<()>,
    },
}

pub struct UacController {
    handle: Arc<UacHandle>,
}

impl UacController {
    pub fn new(handle: Arc<UacHandle>) -> Self {
        Self { handle }
    }

    pub fn set_process_enabled(&self, enabled: bool) {
        self.handle.set_process_enabled(enabled);
    }

    pub async fn sync_state(&self) -> Result<(), UsbError> {
        self.handle.sync_process_state().await
    }

    pub fn is_process_running(&self) -> bool {
        self.handle.is_process_running()
    }
}

pub struct UacHandle {
    name: String,
    enabled: AtomicBool,
    process_enabled: AtomicBool,
    udc_updating: AtomicBool,
    cmd_tx: mpsc::Sender<UacCmd>,
    daemon: Arc<MicDaemon>,
    _worker: JoinHandle<()>,
}

fn spawn_mic_process_state_publisher(state_rx: mpsc::Receiver<bool>) {
    tokio::spawn(async move {
        loop {
            while let Ok(running) = state_rx.try_recv() {
                if let Err(e) = crate::control::zenoh::send_mic_process_state_event(running).await {
                    warn!("failed to publish mic process state: {}", e);
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

impl UacHandle {
    pub fn new(device_enabled: bool, process_enabled: bool) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (state_tx, state_rx) = mpsc::channel();
        spawn_mic_process_state_publisher(state_rx);
        let daemon = Arc::new(MicDaemon::new(state_tx));
        let worker_daemon = Arc::clone(&daemon);
        let worker = thread::spawn(move || uac_worker_loop(cmd_rx, worker_daemon));

        Arc::new(Self {
            name: UAC1_INSTANCE.to_owned(),
            enabled: AtomicBool::new(device_enabled),
            process_enabled: AtomicBool::new(process_enabled),
            udc_updating: AtomicBool::new(false),
            cmd_tx,
            daemon,
            _worker: worker,
        })
    }

    pub fn is_process_running(&self) -> bool {
        self.daemon.is_process_running()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn set_process_enabled(&self, enabled: bool) {
        self.process_enabled.store(enabled, Ordering::Release);
    }

    pub async fn sync_process_state(&self) -> Result<(), UsbError> {
        let device_enabled = self.enabled.load(Ordering::Acquire);
        let process_enabled = self.process_enabled.load(Ordering::Acquire);
        let (ack_tx, ack_rx) = mpsc::channel();
        self.cmd_tx
            .send(UacCmd::SyncState {
                device_enabled,
                process_enabled,
                ack: ack_tx,
            })
            .map_err(|e| UsbError::GadgetError(format!("uac sync command failed: {e}")))?;

        tokio::task::spawn_blocking(move || ack_rx.recv_timeout(CMD_TIMEOUT))
            .await
            .map_err(|e| UsbError::GadgetError(format!("uac sync join failed: {e}")))?
            .map_err(|e| UsbError::GadgetError(format!("uac sync ack timeout: {e}")))?;
        Ok(())
    }

    async fn stop_process(&self) -> Result<(), UsbError> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.cmd_tx
            .send(UacCmd::Stop { ack: ack_tx })
            .map_err(|e| UsbError::GadgetError(format!("uac stop failed: {e}")))?;

        tokio::task::spawn_blocking(move || ack_rx.recv_timeout(CMD_TIMEOUT))
            .await
            .map_err(|e| UsbError::GadgetError(format!("uac stop join failed: {e}")))?
            .map_err(|e| UsbError::GadgetError(format!("uac stop ack timeout: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl UsbChangeHandler for UacHandle {
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
                if let Err(e) = self.stop_process().await {
                    warn!(error = ?e, "UacHandle PrepareChange stop failed");
                }
                ChangeResponse::Proceed
            }
            ChangeRequest::ChangeCompleted | ChangeRequest::ChangeCanceled => {
                if let Err(e) = self.sync_process_state().await {
                    warn!(error = ?e, "UacHandle ChangeCompleted sync failed");
                }
                self.udc_updating.store(false, Ordering::Release);
                ChangeResponse::Proceed
            }
        }
    }
}

fn uac_worker_loop(cmd_rx: mpsc::Receiver<UacCmd>, daemon: Arc<MicDaemon>) {
    info!("UacHandle worker thread started");
    loop {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(UacCmd::Stop { ack }) => {
                daemon.stop();
                let _ = ack.send(());
            }
            Ok(UacCmd::SyncState {
                device_enabled,
                process_enabled,
                ack,
            }) => {
                if device_enabled && process_enabled {
                    daemon.start();
                } else {
                    daemon.stop();
                }
                let _ = ack.send(());
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.tick();
            }
            Err(RecvTimeoutError::Disconnected) => {
                info!("UacHandle worker exiting: command channel closed");
                daemon.stop();
                break;
            }
        }
    }
    info!("UacHandle worker thread stopped");
}

struct MicDaemonInner {
    child: Option<Child>,
    program_path: String,
}

struct MicDaemon {
    inner: Mutex<MicDaemonInner>,
    is_running: AtomicBool,
    shutdown_requested: AtomicBool,
    state_tx: mpsc::Sender<bool>,
}

impl MicDaemon {
    fn is_process_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    fn new(state_tx: mpsc::Sender<bool>) -> Self {
        Self {
            inner: Mutex::new(MicDaemonInner {
                child: None,
                program_path: ARKKVM_MIC_PATH.to_string(),
            }),
            is_running: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(true),
            state_tx,
        }
    }

    fn notify_running(&self, running: bool) {
        if let Err(e) = self.state_tx.send(running) {
            warn!("mic process state notify channel closed: {}", e);
        }
    }

    fn start(&self) {
        self.shutdown_requested.store(false, Ordering::Release);
        if self.is_running.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        match Command::new(&guard.program_path).spawn() {
            Ok(child) => {
                guard.child = Some(child);
                self.is_running.store(true, Ordering::Release);
                info!("Successfully started process: {}", ARKKVM_MIC_NAME);
                self.notify_running(true);
            }
            Err(e) => {
                error!("Failed to start process {}: {:?}", ARKKVM_MIC_NAME, e);
                self.notify_running(false);
            }
        }
    }

    fn stop(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut child) = guard.child.take() {
            if let Err(e) = child.kill() {
                error!("Failed to kill process {}: {:?}", ARKKVM_MIC_NAME, e);
            } else {
                info!("Successfully stopped process: {}", ARKKVM_MIC_NAME);
            }
            let _ = child.wait();
        }
        self.is_running.store(false, Ordering::Release);
        self.notify_running(false);
    }

    fn tick(&self) {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return;
        }

        let mut process_exited = false;
        let running = {
            let mut guard = self.inner.lock().unwrap();
            if let Some(child) = &mut guard.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        debug!("Process {} exited with status: {:?}", ARKKVM_MIC_NAME, status);
                        guard.child = None;
                        process_exited = true;
                        false
                    }
                    Ok(None) => true,
                    Err(e) => {
                        error!("Failed to check process status {}: {:?}", ARKKVM_MIC_NAME, e);
                        guard.child = None;
                        process_exited = true;
                        false
                    }
                }
            } else {
                false
            }
        };
        self.is_running.store(running, Ordering::Release);

        if process_exited {
            self.notify_running(false);
        }

        if running || self.shutdown_requested.load(Ordering::Acquire) {
            return;
        }

        warn!("{} process died, restarting...", ARKKVM_MIC_NAME);
        thread::sleep(RESTART_DELAY);
        if !self.shutdown_requested.load(Ordering::Acquire) {
            self.start();
        }
    }
}
