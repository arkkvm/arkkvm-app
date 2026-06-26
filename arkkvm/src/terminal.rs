use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::io::{self, ErrorKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use parking_lot::{Mutex, RwLock};
use rustix::fs::{Mode as FileMode, OFlags, fcntl_getfl, fcntl_setfl, open};
use rustix::io::{Errno, dup, read, write};
use rustix::process::{ioctl_tiocsctty, setsid};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;

/// Terminal size information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// Internal state of the terminal handler with health tracking
struct TerminalState {
    /// The PTY master file descriptor using rustix
    ptmx: Option<OwnedFd>,
    /// The child process running in the PTY
    cmd: Option<Child>,
    /// Track if the terminal is in a healthy state
    is_healthy: bool,
}

impl TerminalState {
    fn new() -> Self {
        Self { ptmx: None, cmd: None, is_healthy: true }
    }

    fn is_ready(&self) -> bool {
        self.ptmx.is_some() && self.cmd.is_some() && self.is_healthy
    }

    fn mark_unhealthy(&mut self) {
        self.is_healthy = false;
        warn!("Terminal state marked as unhealthy");
    }

    fn reset_health(&mut self) {
        self.is_healthy = true;
    }
}

/// Safe wrapper for PTY operations using rustix
struct PtyMaster {
    fd: OwnedFd,
    slave_path: String,
}

impl PtyMaster {
    /// Create a new PTY master with safe initialization using rustix
    fn new() -> Result<Self> {
        // Open PTY master using rustix
        let master_fd =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("Failed to open PTY master")?;

        // Grant access to the PTY slave
        grantpt(&master_fd).context("Failed to grant PTY access")?;

        // Unlock the PTY slave
        unlockpt(&master_fd).context("Failed to unlock PTY")?;

        // Get the PTY slave name
        let slave_name = ptsname(&master_fd, Vec::new()).context("Failed to get PTY slave name")?;
        let slave_path = slave_name.to_string_lossy().to_string();

        Ok(Self { fd: master_fd, slave_path })
    }

    /// Get the slave path
    fn slave_path(&self) -> &str {
        &self.slave_path
    }

    /// Get a borrowed fd
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Set terminal window size using rustix
    fn set_window_size(&self, size: &TerminalSize) -> Result<()> {
        let winsize = Winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };

        // Use rustix for safe fd access
        tcsetwinsize(&self.fd, winsize).context("Failed to set terminal window size")?;

        Ok(())
    }

    /// Write to PTY using rustix with retry logic
    fn write(&self, data: &[u8]) -> Result<usize> {
        write(&self.fd, data).context("Failed to write to PTY")
    }
}

/// Safe child process creator for PTY using rustix for slave file operations
struct PtyChildProcess;

impl PtyChildProcess {
    /// Spawn a child process attached to PTY slave using rustix for file operations
    fn spawn_with_pty(cmd: &mut Command, slave_path: &str) -> Result<Child> {
        // Use rustix to open slave file descriptors safely
        let slave_fd = open(slave_path, OFlags::RDWR, FileMode::empty())
            .context("Failed to open PTY slave")?;

        // Convert OwnedFd to std::fs::File for compatibility with std::process
        let slave_file = std::fs::File::from(slave_fd);

        // Duplicate the file descriptor for each stdio stream
        let stdin_file = slave_file.try_clone().context("Failed to clone slave fd for stdin")?;
        let stdout_file = slave_file.try_clone().context("Failed to clone slave fd for stdout")?;
        let stderr_file = slave_file;

        cmd.stdin(Stdio::from(stdin_file));
        cmd.stdout(Stdio::from(stdout_file));
        cmd.stderr(Stdio::from(stderr_file));

        // New session + controlling terminal so job control and INTR (^C) match an SSH-like PTY
        // (foreground process group / isig semantics on the slave).
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                setsid().map_err(std::io::Error::from)?;
                ioctl_tiocsctty(std::io::stdin().as_fd()).map_err(std::io::Error::from)?;
                Ok(())
            });
        }

        let child = cmd.spawn().context("Failed to start shell process")?;

        Ok(child)
    }
}

/// Forwards PTY output to the DataChannel; uses `AsyncFd` so the reactor wakes
/// the task only when readable (no sleep-based polling when idle).
struct OutputForwarder {
    handle: Option<JoinHandle<Result<()>>>,
    shutdown_tx: mpsc::UnboundedSender<()>,
}

impl OutputForwarder {
    /// Reads from a non-blocking PTY in a Tokio task: wait for readiness from
    /// the reactor, then drain reads until EAGAIN.
    fn start(
        state: Arc<RwLock<TerminalState>>,
        data_channel: Arc<RTCDataChannel>,
        shutdown_flag: Arc<AtomicBool>,
        channel_id: u16,
    ) -> Self {
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();

        // snapshot and dup ptmx once, avoid holding lock in read loop
        let ptmx_dup = {
            let g = state.read();
            g.ptmx.as_ref().map(dup).transpose()
        };

        let handle = tokio::spawn(async move {
            let ptmx = match ptmx_dup {
                Ok(Some(fd)) => fd,
                _ => {
                    debug!(
                        "PTY not available, stopping output reader for channel {:?}",
                        channel_id
                    );
                    return Ok(());
                }
            };

            let async_fd = match AsyncFd::with_interest(ptmx, Interest::READABLE) {
                Ok(fd) => fd,
                Err(e) => {
                    warn!(
                        "AsyncFd PTY registration failed for channel {:?}: {}",
                        channel_id, e
                    );
                    bail!("failed to register PTY with reactor: {}", e);
                }
            };

            let mut buf = [0u8; 1024];
            let mut error_count = 0u32;
            const MAX_ERRORS: u32 = 5;

            debug!("Output reader task started for channel {:?}", channel_id);

            'outer: loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                tokio::select! {
                    biased;
                    msg = shutdown_rx.recv() => {
                        let _ = msg;
                        break;
                    }
                    r = async_fd.readable() => {
                        let mut guard = match r {
                            Ok(g) => g,
                            Err(e) => {
                                error!("PTY readable poll failed on channel {:?}: {}", channel_id, e);
                                break;
                            }
                        };

                        'drain: loop {
                            if shutdown_flag.load(Ordering::Relaxed) {
                                break 'outer;
                            }

                            match guard.try_io(|io_ref| -> io::Result<usize> {
                                let fd = io_ref.get_ref();
                                loop {
                                    match read(fd, &mut buf) {
                                        Ok(n) => return Ok(n),
                                        Err(Errno::INTR) => continue,
                                        Err(Errno::AGAIN) => {
                                            return Err(io::Error::from(ErrorKind::WouldBlock));
                                        }
                                        Err(err) => {
                                            return Err(io::Error::other(err.to_string()));
                                        }
                                    }
                                }
                            }) {
                                Ok(Ok(0)) => {
                                    debug!("PTY EOF reached for channel {:?}", channel_id);
                                    break 'outer;
                                }
                                Ok(Ok(n)) => {
                                    error_count = 0;
                                    let data = buf[..n].to_vec();
                                    if let Err(e) =
                                        data_channel.send(&data.into()).await
                                    {
                                        warn!(
                                            "Failed to send pty output for channel {:?}: {}",
                                            channel_id, e
                                        );
                                    }
                                    continue 'drain;
                                }
                                Ok(Err(e)) => {
                                    error_count += 1;
                                    error!(
                                        "Failed to read from pty for channel {:?}: {} (error {}/{})",
                                        channel_id,
                                        e,
                                        error_count,
                                        MAX_ERRORS
                                    );
                                    if error_count >= MAX_ERRORS {
                                        bail!("Too many consecutive read errors");
                                    }
                                    break 'outer;
                                }
                                Err(_would_block) => break 'drain,
                            }
                        }
                    }
                }
            }

            info!("Output reader task exiting for channel {:?}", channel_id);
            Ok(())
        });

        Self { handle: Some(handle), shutdown_tx }
    }

    /// Gracefully shutdown the forwarder with timeout
    fn shutdown(&mut self) -> Result<()> {
        // Send shutdown signal
        if self.shutdown_tx.send(()).is_err() {
            warn!("Failed to send shutdown signal to output forwarder");
        }

        // Abort the task instead of waiting
        if let Some(handle) = self.handle.take() {
            handle.abort();
            debug!("Output forwarder task aborted");
        }

        Ok(())
    }
}

impl Drop for OutputForwarder {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            error!("Error during output forwarder cleanup: {}", e);
        }
    }
}

/// Terminal handler managing PTY operations with parking_lot for superior thread safety
pub struct TerminalHandler {
    /// Shared state protected by parking_lot RwLock for better read performance
    state: Arc<RwLock<TerminalState>>,
    /// Data channel for communication
    data_channel: Arc<RTCDataChannel>,
    /// Thread management with proper lifecycle using parking_lot Mutex
    output_forwarder: Mutex<Option<OutputForwarder>>,
    /// Atomic flag for graceful shutdown
    shutdown_flag: Arc<AtomicBool>,
    /// Scoped logger with channel ID for better debugging
    channel_id: u16,
}

impl TerminalHandler {
    /// Create a new terminal handler
    pub fn new(data_channel: Arc<RTCDataChannel>) -> Arc<Self> {
        let channel_id = data_channel.id();
        let handler = Arc::new(Self {
            state: Arc::new(RwLock::new(TerminalState::new())),
            data_channel: data_channel.clone(),
            output_forwarder: Mutex::new(None),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            channel_id,
        });

        Self::setup_event_handlers(Arc::clone(&handler), data_channel);
        handler
    }

    /// Setup all WebRTC data channel event handlers with comprehensive error recovery
    fn setup_event_handlers(handler: Arc<Self>, data_channel: Arc<RTCDataChannel>) {
        // Setup OnOpen handler
        let handler_open = Arc::clone(&handler);
        data_channel.on_open(Box::new(move || {
            let handler = Arc::clone(&handler_open);
            Box::pin(async move {
                if let Err(e) = handler.on_open().await {
                    error!("Failed to start PTY on channel {:?}: {}", handler.channel_id, e);
                    if let Err(close_err) = handler.data_channel.close().await {
                        error!("Failed to close data channel after PTY error: {}", close_err);
                    }
                }
            })
        }));

        // Setup OnMessage handler
        let handler_message = Arc::clone(&handler);
        data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
            let handler = Arc::clone(&handler_message);
            Box::pin(async move {
                if let Err(e) = handler.on_message(msg).await {
                    warn!("Failed to handle message on channel {:?}: {}", handler.channel_id, e);
                }
            })
        }));

        // Setup OnClose handler
        let handler_close = Arc::clone(&handler);
        data_channel.on_close(Box::new(move || {
            let handler = Arc::clone(&handler_close);
            Box::pin(async move {
                if let Err(e) = handler.on_close().await {
                    error!(
                        "Error during close handler for channel {:?}: {}",
                        handler.channel_id, e
                    );
                }
            })
        }));

        // Setup OnError handler
        let handler_error = Arc::clone(&handler);
        data_channel.on_error(Box::new(move |err| {
            let handler = Arc::clone(&handler_error);
            Box::pin(async move {
                handler.on_error(err).await;
            })
        }));
    }

    /// OnOpen handler with comprehensive error handling
    async fn on_open(&self) -> Result<()> {
        self.shutdown_flag.store(false, Ordering::Relaxed);
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-i");
        let (ptmx, child) = self.start_pty_with_command(&mut cmd)?;

        {
            let mut state = self.state.write();
            state.ptmx = Some(ptmx);
            state.cmd = Some(child);
            state.reset_health();
        }

        // Start and manage output forwarder properly
        self.start_output_forwarder()?;

        debug!("Terminal PTY started successfully on channel {:?}", self.channel_id);
        Ok(())
    }

    /// Start PTY with command - using rustix for better resource management
    fn start_pty_with_command(&self, cmd: &mut Command) -> Result<(OwnedFd, Child)> {
        // Create PTY master using rustix wrapper
        let pty_master = PtyMaster::new().context("Failed to create PTY master")?;

        debug!(
            "PTY slave created at: {} for channel {:?}",
            pty_master.slave_path(),
            self.channel_id
        );

        let flags = fcntl_getfl(&pty_master.fd).context("getfl failed")?;
        fcntl_setfl(&pty_master.fd, flags | OFlags::NONBLOCK).context("set O_NONBLOCK failed")?;

        // Start the child process using rustix for slave file operations
        let child = PtyChildProcess::spawn_with_pty(cmd, pty_master.slave_path())
            .context("Failed to spawn child process with PTY")?;

        debug!("Child process started with PID: {:?} on channel {:?}", child.id(), self.channel_id);

        // Extract the file descriptor from the wrapper
        Ok((pty_master.fd, child))
    }

    /// Proper output forwarder management with parking_lot
    fn start_output_forwarder(&self) -> Result<()> {
        let forwarder = OutputForwarder::start(
            Arc::clone(&self.state),
            Arc::clone(&self.data_channel),
            Arc::clone(&self.shutdown_flag),
            self.channel_id,
        );

        let mut output_forwarder_guard = self.output_forwarder.lock();
        *output_forwarder_guard = Some(forwarder);

        Ok(())
    }

    /// OnMessage handler with proper error propagation
    async fn on_message(&self, msg: DataChannelMessage) -> Result<()> {
        let is_ready = {
            let state = self.state.read();
            state.is_ready()
        };

        if !is_ready {
            debug!("PTY not ready for message on channel {:?}", self.channel_id);
            return Ok(());
        }

        debug!("msg isString: {}, context: {:?}", msg.is_string, msg.data);
        // Handle string messages for terminal resize
        if msg.is_string {
            let text = String::from_utf8(msg.data.to_vec())
                .context("Failed to decode message as UTF-8")?;

            let maybe_json = text.trim();

            // Check if this resembles JSON
            if maybe_json.len() > 1 && maybe_json.starts_with('{') && maybe_json.ends_with('}') {
                match serde_json::from_str::<TerminalSize>(maybe_json) {
                    Ok(size) => {
                        if let Err(e) = self.set_terminal_size(&size) {
                            warn!(
                                "Failed to set terminal size for channel {:?}: {}",
                                self.channel_id, e
                            );
                        } else {
                            info!(
                                "Set terminal size to {}x{} for channel {:?}",
                                size.cols, size.rows, self.channel_id
                            );
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        // Not a valid terminal size JSON, continue to write as data
                        error!("terminal msg: {maybe_json}, error: {e:?}");
                    }
                }
            }
        }

        // Write to PTY
        self.write_to_pty(&msg.data)?;
        Ok(())
    }

    /// Write to PTY with rustix for improved performance and error handling
    fn write_to_pty(&self, data: &[u8]) -> Result<()> {
        let ptmx = {
            let state = self.state.read();
            if !state.is_healthy {
                bail!("Terminal is in unhealthy state");
            }
            match state.ptmx.as_ref() {
                Some(fd) => dup(fd)?,
                None => bail!("PTY not available"),
            }
        };

        let mut bytes_written = 0;
        let mut retries = 0u32;
        const MAX_RETRIES: u32 = 4;

        while bytes_written < data.len() {
            match write(&ptmx, &data[bytes_written..]) {
                Ok(n) if n > 0 => {
                    bytes_written += n;
                    retries = 0;
                }
                Ok(_) => bail!("Write returned 0 bytes"),
                Err(e) if e == Errno::AGAIN && retries < MAX_RETRIES => {
                    std::hint::spin_loop(); // non-blocking retry
                    retries += 1;
                    continue;
                }
                Err(e) if e == Errno::INTR => {
                    continue;
                } // retry on EINTR
                Err(e) if e == Errno::AGAIN => bail!("Write would block after retries"),
                Err(e) => bail!("Failed to write to PTY: {}", e),
            }
        }
        debug!("Wrote {} bytes to PTY for channel {:?}", data.len(), self.channel_id);
        Ok(())
    }

    /// Set terminal size with rustix - completely safe and efficient
    fn set_terminal_size(&self, size: &TerminalSize) -> Result<()> {
        let state = self.state.read();

        if !state.is_healthy {
            bail!("Terminal is in unhealthy state");
        }

        if let Some(ref ptmx) = state.ptmx {
            let winsize =
                Winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };

            // Use rustix for safe and efficient fd access
            tcsetwinsize(ptmx, winsize).context("Failed to set terminal window size")?;
        } else {
            bail!("PTY not available");
        }
        Ok(())
    }

    /// OnClose handler with comprehensive cleanup and error handling
    async fn on_close(&self) -> Result<()> {
        info!("Terminal channel {:?} closing, starting cleanup", self.channel_id);

        // Gracefully stop output forwarder
        self.shutdown_flag.store(true, Ordering::Relaxed);

        {
            let mut forwarder_guard = self.output_forwarder.lock();
            if let Some(mut forwarder) = forwarder_guard.take()
                && let Err(e) = forwarder.shutdown()
            {
                error!(
                    "Failed to shutdown output forwarder for channel {:?}: {}",
                    self.channel_id, e
                );
            }
        }

        let mut state = self.state.write();

        // Close PTY first - rustix will handle this automatically via Drop
        if let Some(_ptmx) = state.ptmx.take() {
            debug!("PTY closed for channel {:?}", self.channel_id);
        }

        // Kill child process
        if let Some(mut cmd) = state.cmd.take() {
            if let Err(e) = cmd.kill() {
                error!("Failed to kill child process for channel {:?}: {}", self.channel_id, e);
            } else {
                debug!("Child process killed for channel {:?}", self.channel_id);
            }
        }

        state.mark_unhealthy();
        info!("Terminal channel {:?} closed", self.channel_id);
        Ok(())
    }

    /// OnError handler
    async fn on_error(&self, err: webrtc::Error) {
        error!("Terminal channel {:?} error: {}", self.channel_id, err);

        // Mark state as unhealthy on critical errors
        let mut state = self.state.write();
        state.mark_unhealthy();
    }
}

// Complete Drop implementation - parking_lot handles locks safely
impl Drop for TerminalHandler {
    fn drop(&mut self) {
        debug!("TerminalHandler dropping for channel {:?}", self.channel_id);

        // Signal shutdown
        self.shutdown_flag.store(true, Ordering::Relaxed);

        // Properly shutdown output forwarder - parking_lot never poisons
        let mut forwarder_guard = self.output_forwarder.lock();
        if let Some(mut forwarder) = forwarder_guard.take()
            && let Err(e) = forwarder.shutdown()
        {
            error!("Error shutting down forwarder during drop: {}", e);
        }

        // Cleanup state - parking_lot RwLock is always safe
        let mut state = self.state.write();
        if let Some(mut cmd) = state.cmd.take()
            && let Err(e) = cmd.kill()
        {
            error!("Error killing child process during drop: {}", e);
        }
        state.ptmx.take(); // OwnedFd will be auto-closed

        debug!("TerminalHandler dropped for channel {:?}", self.channel_id);
    }
}

/// Setup terminal channel with comprehensive error handling
pub async fn setup_terminal_channel(channel: Arc<RTCDataChannel>) -> Result<Arc<TerminalHandler>> {
    let channel_id = channel.id();
    info!("Terminal data channel (ID: {:?}) established", channel_id);

    // Create handler which sets up all the event handlers
    let handler = TerminalHandler::new(channel);

    info!("Terminal channel setup completed for ID: {:?}", channel_id);
    Ok(handler)
}
