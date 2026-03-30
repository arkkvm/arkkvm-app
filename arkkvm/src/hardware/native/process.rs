//! Native process management and supervision.
//!
//! Process lifecycle: extract binary (future), run, supervise, restart on exit.
//!
//! TODO(native-resources): Implement resource packaging/version helpers:
//! - ensureBinaryUpdated / shouldOverwrite (extract embedded native bin, write sha256)
//! - getNativeSha256 / getNativeVersion (query embedded version/hash)
//!   Consider integrating with `assets.rs` (RustEmbed) for shipping the native binary.
//!   Keep this file focused on process lifecycle; put extraction/version logic into a
//!   small companion module when needed.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, warn};

/// Handle to a supervised native process.
pub struct NativeSupervisor {
    binary_path: String,
    child: Arc<Mutex<Option<Child>>>,
    supervisor_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NativeSupervisor {
    pub fn new(binary_path: impl Into<String>) -> Self {
        Self {
            binary_path: binary_path.into(),
            child: Arc::new(Mutex::new(None)),
            supervisor_task: Arc::new(Mutex::new(None)),
        }
    }

    /// Ensure the binary exists and is executable. Caller provides location.
    fn ensure_binary(&self) -> Result<()> {
        let path = Path::new(&self.binary_path);
        if !path.exists() {
            // TODO(native-resources): Optionally auto-extract embedded native binary here
            // using a helper like `ensure_binary_updated()`.
            anyhow::bail!("native binary not found: {}", self.binary_path);
        }
        Ok(())
    }

    /// Start the process immediately. If already running, returns Ok.
    pub async fn start_now(&self) -> Result<()> {
        self.ensure_binary()?;
        let mut child_guard = self.child.lock().await;
        if child_guard.is_some() {
            return Ok(());
        }
        let binary = self.binary_path.clone();
        // run spawn in blocking pool
        let child = tokio::task::spawn_blocking(move || {
            std::process::Command::new(&binary)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .with_context(|| format!("failed to start native binary: {}", &binary))
        })
        .await
        .map_err(|e| anyhow::anyhow!("join error: {}", e))??;
        info!("native binary started pid={}", child.id());
        *child_guard = Some(child);
        Ok(())
    }

    /// Start background supervisor loop.
    pub async fn supervise(self: &Arc<Self>) {
        // ensure only one supervisor task
        {
            let task_guard = self.supervisor_task.lock().await;
            if task_guard.is_some() {
                return;
            }
        }

        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            loop {
                {
                    let mut child_guard = this.child.lock().await;
                    let need_spawn = match child_guard.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => {
                                if status.success() {
                                    info!("native exited successfully");
                                } else {
                                    warn!("native exited with status: {}", status);
                                }
                                true
                            }
                            Ok(None) => false,
                            Err(e) => {
                                warn!("native try_wait error: {}", e);
                                true
                            }
                        },
                        None => true,
                    };

                    if need_spawn {
                        // allow a short cooldown before restart
                        sleep(Duration::from_secs(10)).await;
                        match Command::new(&this.binary_path)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                        {
                            Ok(child) => {
                                info!("restarted native pid={}", child.id());
                                *child_guard = Some(child);
                            }
                            Err(e) => {
                                warn!("failed to restart native: {}", e);
                            }
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        });
        let mut task_guard = self.supervisor_task.lock().await;
        *task_guard = Some(handle);
    }
}
