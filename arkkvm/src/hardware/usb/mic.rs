//! Microphone audio output: keep the arkkvm_mic child process alive via a watchdog thread, and forward audio data through Zenoh.
//!
//! ## Responsibility Breakdown
//!
//! - **DaemonHandle**: a "handle + single operation" wrapper for the child process. It only is responsible for:
//!   - Holding the `Child` and its running state;
//!   - Providing single operations: `start` / `stop` / `check_status` / `is_running`;
//!   - Not including the monitoring loop or restart policy.
//!
//! - **Watchdog thread**: lifecycle monitoring and automatic restart for the child process. It only is responsible for:
//!   - Starting the child process once during initialization via DaemonHandle;
//!   - Polling `DaemonHandle::check_status()` at a fixed interval;
//!   - When the process exits, restarting by calling `DaemonHandle::start` according to the policy (with delay), with no upper limit on restart attempts;
//!   - Not directly holding `Child`; all start/stop/check operations are done via DaemonHandle.

use std::process::{Command, Child};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::zenoh_bus;

const ARKKVM_MIC_NAME: &str = "arkkvm_mic";
const ARKKVM_MIC_PATH: &str = "/oem/usr/bin/arkkvm_mic";

pub enum AudioOutputError {
    Success = 0,
    ErrorInit = -1,
    ErrorSystem = -2,
    ErrorMemory = -3,
    ErrorInvalidHandle = -4,
    ErrorOutput = -5,
    ErrorParam = -6,
    ErrorFile = -7,
}

pub struct AudioOutput {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    daemon_handle: Option<Arc<DaemonHandle>>,
}

/// Child process handle and single-operation wrapper (**does not** handle the monitoring loop or restart policy).
///
/// Responsibilities: holds `Child`, maintains `is_running`, and provides **single** operations for the child process:
/// - `start`: start once;
/// - `stop`: stop once;
/// - `check_status`: check once whether it is still running and update `is_running`;
/// - `is_running`: read-only query.
///
/// It only holds fields that may need locked mutation; `is_running` / `program_name` are stored outside to reduce lock contention on the read path.
struct DaemonHandleInner {
    child: Option<Child>,
    program_path: String,
    args: Option<Vec<String>>,
}

/// Called by the **watchdog thread** in the loop to invoke `check_status()` and restart via `start()` based on the result.
struct DaemonHandle {
    inner: Mutex<DaemonHandleInner>,
    is_running: AtomicBool,
    /// Set on Drop; once detected by the watchdog thread, it exits the loop and stops restarting the child process.
    shutdown_requested: AtomicBool,
    program_name: String,
}

impl DaemonHandle {
    fn new(program_name: &str, program_path: &str, args: Option<Vec<String>>) -> Self {
        Self {
            inner: Mutex::new(DaemonHandleInner {
                child: None,
                program_path: program_path.to_string(),
                args,
            }),
            is_running: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            program_name: program_name.to_string(),
        }
    }

    /// Request shutdown: called in Drop; the watchdog thread will exit on the next loop iteration and stop restarting.
    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
    }

    fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Query the current cached running state only (lock-free; reads the atomic value).
    fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Start the child process once (using program_path and args configured at build-time); if already running, return success immediately.
    fn start(&self) -> bool {
        if self.is_running.load(Ordering::Acquire) {
            warn!("Process {} is already running", self.program_name);
            return true;
        }

        let mut guard = self.inner.lock().unwrap();
        let mut cmd = Command::new(&guard.program_path);
        if let Some(ref args) = guard.args {
            cmd.args(args);
        }
        match cmd.spawn() {
            Ok(child) => {
                guard.child = Some(child);
                self.is_running.store(true, Ordering::Release);
                info!("Successfully started process: {}", self.program_name);
                true
            }
            Err(e) => {
                error!("Failed to start process {}: {:?}", self.program_name, e);
                false
            }
        }
    }

    /// Stop the child process once (kill it and wait for it to exit, then clear the handle).
    fn stop(&self) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut child) = guard.child.take() {
            if let Err(e) = child.kill() {
                error!("Failed to kill process {}: {:?}", self.program_name, e);
            } else {
                info!("Successfully stopped process: {}", self.program_name);
            }
            self.is_running.store(false, Ordering::Release);
            let _ = child.wait();
        }
    }

    /// Check once whether the process is still running (`try_wait`), update the atomic `is_running`, and return whether it is running.
    fn check_status(&self) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let running = if let Some(child) = &mut guard.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    debug!("Process {} exited with status: {:?}", self.program_name, status);
                    false
                }
                Ok(None) => true,
                Err(e) => {
                    error!("Failed to check process status {}: {:?}", self.program_name, e);
                    false
                }
            }
        } else {
            false
        };
        self.is_running.store(running, Ordering::Release);
        running
    }
}

impl AudioOutput {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        let daemon_handle = Arc::new(DaemonHandle::new(
            ARKKVM_MIC_NAME,
            ARKKVM_MIC_PATH,
            None,
        ));
        let daemon_handle_clone = Arc::clone(&daemon_handle);

        // Watchdog thread: only responsible for "lifecycle monitoring + policy-based automatic restart"; all child process operations go through DaemonHandle.
        thread::spawn(move || {
            info!("arkkvm_mic daemon thread started");
            if !daemon_handle_clone.start() {
                error!("Failed to start arkkvm_mic on initialization");
                return;
            }

            let mut restart_count: u64 = 0;
            const RESTART_DELAY: Duration = Duration::from_secs(1);

            loop {
                thread::sleep(Duration::from_millis(500));

                if daemon_handle_clone.is_shutdown_requested() {
                    info!("arkkvm_mic daemon thread exiting: shutdown requested");
                    break;
                }

                let should_restart = !daemon_handle_clone.check_status();

                if should_restart {
                    if daemon_handle_clone.is_shutdown_requested() {
                        info!("arkkvm_mic daemon thread exiting: shutdown requested (no restart)");
                        break;
                    }
                    restart_count += 1;
                    warn!("arkkvm_mic process died, restarting (attempt #{})...", restart_count);

                    thread::sleep(RESTART_DELAY);

                    if daemon_handle_clone.start() {
                        info!("Successfully restarted arkkvm_mic");
                    } else {
                        error!("Failed to restart arkkvm_mic");
                    }
                } else {
                    restart_count = 0;
                }
            }
            info!("arkkvm_mic daemon thread stopped");
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let daemon_handle_for_sender = Arc::clone(&daemon_handle);
        handle.spawn(async move {
            info!("mic sender started");
            let session = zenoh_bus::get_mic_session();
            while let Some(data) = rx.recv().await {
                if !daemon_handle_for_sender.is_running() {
                    warn!("arkkvm_mic is not running, dropping audio data ({} bytes)", data.len());
                    continue;
                }

                // info!("send mic data size: {}", data.len());
                let value = zenoh::bytes::ZBytes::from(data);
                if let Err(e) = session.put("arkkvm_mic/data", value).await {
                    error!("Failed to send mic data: {:?}", e);
                }
            }
            info!("mic sender stopped");
        });

        Self {
            tx,
            daemon_handle: Some(daemon_handle),
        }
    }

    /// Send audio data (slice-based). Reusing the caller's buffer avoids allocations on the hot path.
    pub fn send_data(&self, data: &[u8]) {
        // if !self.is_process_running() {
        //     warn!("arkkvm_mic is not running, skipping audio data ({} bytes)", data.len());
        //     return;
        // }

        if let Err(e) = self.tx.try_send(data.to_vec()) {
            error!("Failed to queue audio data: {:?}", e);
        }
    }

    /// Return whether the child process is running. It only reads cached state (refreshed periodically by the watchdog thread via check_status), not real-time detection.
    pub fn is_process_running(&self) -> bool {
        match self.daemon_handle.as_ref() {
            Some(daemon) => daemon.is_running(),
            None => false,
        }
    }

}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        if let Some(handle) = &self.daemon_handle {
            handle.request_shutdown();
            handle.stop();
        }
    }
}