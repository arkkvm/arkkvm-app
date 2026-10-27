use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use rustix::fs::{Mode as FileMode, OFlags, fcntl_getfl, fcntl_setfl, open};
use rustix::io::{Errno, read, write};
use rustix::process::{Pid, Signal, ioctl_tiocsctty, kill_process_group, setsid};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use serde::{Deserialize, Serialize};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use webrtc::data_channel::{DataChannel, RTCDataChannelMessage};

use crate::webrtc::{DcCallbacks, spawn_dc_events, wait_dc_open};

const CMD_QUEUE_CAP: usize = 256;
const OUTBOX_CAP: usize = 64;
const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Terminal size information (DataChannel resize JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// How to spawn the PTY child and paths to remove when the session ends.
pub(crate) struct SpawnSpec {
    pub command: Command,
    pub cleanup_paths: Vec<PathBuf>,
}

enum TerminalCmd {
    Data(Bytes),
    Resize(TerminalSize),
}

/// Session-owned handle: cancel is the single source of truth for Stopping.
pub struct TerminalHandle {
    cancel: CancellationToken,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl TerminalHandle {
    /// Signal Stopping (idempotent). Actor races this against all I/O.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Cancel and wait for the actor to finish reclaiming the PTY/child.
    pub async fn stop_and_wait(self) {
        self.cancel.cancel();
        let join = self.join.lock().take();
        if let Some(handle) = join {
            match handle.await {
                Ok(()) => {}
                Err(e) if e.is_cancelled() => {}
                Err(e) => warn!("Terminal actor join error: {}", e),
            }
        }
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        // Always signal Stopping. Detach the join handle (do not abort) so the
        // actor can finish kill/wait even if Session forgot stop_and_wait.
        self.cancel.cancel();
        let _ = self.join.lock().take();
    }
}

struct PtyMaster {
    fd: OwnedFd,
    slave_path: String,
}

impl PtyMaster {
    fn new() -> Result<Self> {
        let master_fd =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("Failed to open PTY master")?;
        grantpt(&master_fd).context("Failed to grant PTY access")?;
        unlockpt(&master_fd).context("Failed to unlock PTY")?;
        let slave_name = ptsname(&master_fd, Vec::new()).context("Failed to get PTY slave name")?;
        let slave_path = slave_name.to_string_lossy().to_string();
        Ok(Self { fd: master_fd, slave_path })
    }

    fn slave_path(&self) -> &str {
        &self.slave_path
    }
}

fn spawn_with_pty(cmd: &mut Command, slave_path: &str) -> Result<Child> {
    let slave_fd =
        open(slave_path, OFlags::RDWR, FileMode::empty()).context("Failed to open PTY slave")?;
    let slave_file = std::fs::File::from(slave_fd);
    let stdin_file = slave_file.try_clone().context("Failed to clone slave fd for stdin")?;
    let stdout_file = slave_file.try_clone().context("Failed to clone slave fd for stdout")?;
    let stderr_file = slave_file;

    cmd.stdin(Stdio::from(stdin_file));
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            crate::process_spawn::close_fds_from(crate::process_spawn::CHILD_FD_KEEP_BELOW);
            setsid().map_err(std::io::Error::from)?;
            ioctl_tiocsctty(std::io::stdin().as_fd()).map_err(std::io::Error::from)?;
            Ok(())
        });
    }

    cmd.spawn().context("Failed to start process with PTY")
}

fn start_pty_with_command(cmd: &mut Command) -> Result<(OwnedFd, Child)> {
    let pty_master = PtyMaster::new().context("Failed to create PTY master")?;
    let flags = fcntl_getfl(&pty_master.fd).context("getfl failed")?;
    fcntl_setfl(&pty_master.fd, flags | OFlags::NONBLOCK).context("set O_NONBLOCK failed")?;
    let child = spawn_with_pty(cmd, pty_master.slave_path())?;
    Ok((pty_master.fd, child))
}

fn set_winsize(ptmx: &OwnedFd, size: &TerminalSize) -> Result<()> {
    let winsize = Winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
    tcsetwinsize(ptmx, winsize).context("Failed to set terminal window size")?;
    Ok(())
}

fn kill_pty_process_group(child: &Child) {
    let Some(raw) = child.id() else {
        return;
    };
    let Some(pid) = Pid::from_raw(raw as i32) else {
        return;
    };
    if let Err(e) = kill_process_group(pid, Signal::KILL) {
        debug!("kill_process_group({raw}): {e}");
    }
}

/// Attach WebTerminal to a DataChannel; returns a Session-owned handle.
pub(crate) fn attach(data_channel: Arc<dyn DataChannel>, spawn: SpawnSpec) -> TerminalHandle {
    let channel_id = data_channel.id();
    let cancel = CancellationToken::new();
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_QUEUE_CAP);
    let started = Arc::new(AtomicBool::new(false));
    let spawn_slot = Arc::new(Mutex::new(Some(spawn)));

    let cancel_close = cancel.clone();
    let on_close = Box::new(move || {
        cancel_close.cancel();
    });

    let started_msg = Arc::clone(&started);
    let cmd_tx_msg = cmd_tx.clone();
    let on_message = Box::new(move |msg: RTCDataChannelMessage| {
        if !started_msg.load(Ordering::Acquire) {
            debug!("PTY session not ready; dropping message on channel {:?}", channel_id);
            return;
        }
        let cmd = parse_terminal_message(msg);
        if let Err(e) = cmd_tx_msg.try_send(cmd) {
            warn!("terminal cmd queue full/closed on channel {:?}: {}", channel_id, e);
        }
    });

    spawn_dc_events(
        data_channel.clone(),
        DcCallbacks {
            on_open: None,
            on_message: Some(on_message),
            on_close: Some(on_close),
            ..Default::default()
        },
    );

    let cancel_boot = cancel.clone();
    let started_boot = Arc::clone(&started);
    let spawn_boot = Arc::clone(&spawn_slot);
    let dc_boot = data_channel.clone();
    let join = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel_boot.cancelled() => {
                debug!("Terminal cancelled before open on channel {:?}", channel_id);
                return;
            }
            open = wait_dc_open(&dc_boot, Duration::from_secs(30)) => {
                if let Err(e) = open {
                    debug!("Terminal wait_dc_open on {:?}: {}", channel_id, e);
                    return;
                }
            }
        }

        if started_boot.swap(true, Ordering::SeqCst) {
            return;
        }

        let Some(mut spawn) = spawn_boot.lock().take() else {
            error!("Terminal spawn spec missing on channel {:?}", channel_id);
            started_boot.store(false, Ordering::SeqCst);
            let _ = dc_boot.close().await;
            return;
        };

        let (ptmx, child) = match start_pty_with_command(&mut spawn.command) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to start PTY on channel {:?}: {}", channel_id, e);
                started_boot.store(false, Ordering::SeqCst);
                let _ = dc_boot.close().await;
                return;
            }
        };

        debug!("PTY child started pid={:?} on channel {:?}", child.id(), channel_id);
        info!("Terminal session actor started for channel {:?}", channel_id);

        run_actor(ActorArgs {
            data_channel: dc_boot,
            channel_id,
            ptmx,
            child,
            cmd_rx,
            cleanup_paths: spawn.cleanup_paths,
            cancel: cancel_boot,
        })
        .await;

        info!("Terminal session actor stopped for channel {:?}", channel_id);
    });

    TerminalHandle { cancel, join: Mutex::new(Some(join)) }
}

fn parse_terminal_message(msg: RTCDataChannelMessage) -> TerminalCmd {
    if msg.is_string {
        match String::from_utf8(msg.data.to_vec()) {
            Ok(text) => {
                let maybe_json = text.trim();
                if maybe_json.len() > 1
                    && maybe_json.starts_with('{')
                    && maybe_json.ends_with('}')
                {
                    match serde_json::from_str::<TerminalSize>(maybe_json) {
                        Ok(size) => TerminalCmd::Resize(size),
                        Err(e) => {
                            error!("terminal resize parse error: {maybe_json}, {e:?}");
                            TerminalCmd::Data(Bytes::from(text.into_bytes()))
                        }
                    }
                } else {
                    TerminalCmd::Data(Bytes::from(text.into_bytes()))
                }
            }
            Err(_) => TerminalCmd::Data(Bytes::copy_from_slice(&msg.data)),
        }
    } else {
        TerminalCmd::Data(Bytes::copy_from_slice(&msg.data))
    }
}

struct ActorArgs {
    data_channel: Arc<dyn DataChannel>,
    channel_id: u16,
    ptmx: OwnedFd,
    child: Child,
    cmd_rx: mpsc::Receiver<TerminalCmd>,
    cleanup_paths: Vec<PathBuf>,
    cancel: CancellationToken,
}

async fn run_actor(args: ActorArgs) {
    let ActorArgs {
        data_channel,
        channel_id,
        ptmx,
        mut child,
        mut cmd_rx,
        cleanup_paths,
        cancel,
    } = args;

    let async_fd = match AsyncFd::with_interest(ptmx, Interest::READABLE | Interest::WRITABLE) {
        Ok(fd) => fd,
        Err(e) => {
            error!("AsyncFd PTY registration failed for channel {:?}: {}", channel_id, e);
            reclaim_child(&mut child, channel_id).await;
            cleanup_files(cleanup_paths).await;
            let _ = data_channel.close().await;
            return;
        }
    };

    let mut write_pending: VecDeque<Bytes> = VecDeque::new();
    let mut outbox: VecDeque<Bytes> = VecDeque::new();
    let mut read_buf = [0u8; 1024];
    let mut child_alive = true;
    let mut reaper = tokio::time::interval(Duration::from_millis(50));
    reaper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // In-flight DC send, always raced with `cancel` at select top-level.
    let mut inflight_send: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>,
    > = None;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        if inflight_send.is_none()
            && let Some(chunk) = outbox.pop_front()
        {
            let dc = data_channel.clone();
            inflight_send = Some(Box::pin(async move {
                dc.send(BytesMut::from(chunk.as_ref()))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }));
        }

        let has_pending = !write_pending.is_empty();
        let can_read = outbox.len() < OUTBOX_CAP;
        let sending = inflight_send.is_some();

        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                break;
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    None => break,
                    Some(TerminalCmd::Resize(size)) => {
                        if let Err(e) = set_winsize(async_fd.get_ref(), &size) {
                            warn!("Failed to set terminal size on {:?}: {}", channel_id, e);
                        } else {
                            info!(
                                "Set terminal size to {}x{} for channel {:?}",
                                size.cols, size.rows, channel_id
                            );
                        }
                    }
                    Some(TerminalCmd::Data(data)) => {
                        write_pending.push_back(data);
                    }
                }
            }

            send_result = async {
                match inflight_send.as_mut() {
                    Some(fut) => fut.as_mut().await,
                    None => std::future::pending().await,
                }
            }, if sending => {
                inflight_send = None;
                if let Err(e) = send_result {
                    warn!("Failed to send pty output on {:?}: {}", channel_id, e);
                    break;
                }
            }

            r = async_fd.readable(), if can_read => {
                let mut guard = match r {
                    Ok(g) => g,
                    Err(e) => {
                        error!("PTY readable poll failed on {:?}: {}", channel_id, e);
                        break;
                    }
                };

                let mut eof = false;
                let mut fatal = false;

                loop {
                    if outbox.len() >= OUTBOX_CAP {
                        break;
                    }
                    match guard.try_io(|io_ref| -> io::Result<usize> {
                        let fd = io_ref.get_ref();
                        loop {
                            match read(fd, &mut read_buf) {
                                Ok(n) => return Ok(n),
                                Err(Errno::INTR) => continue,
                                Err(Errno::AGAIN) => {
                                    return Err(io::Error::from(ErrorKind::WouldBlock));
                                }
                                Err(err) => return Err(io::Error::other(err.to_string())),
                            }
                        }
                    }) {
                        Ok(Ok(0)) => {
                            eof = true;
                            break;
                        }
                        Ok(Ok(n)) => {
                            outbox.push_back(Bytes::copy_from_slice(&read_buf[..n]));
                        }
                        Ok(Err(e)) => {
                            error!("PTY read error on {:?}: {}", channel_id, e);
                            fatal = true;
                            break;
                        }
                        Err(_would_block) => break,
                    }
                }
                drop(guard);

                if eof || fatal {
                    break;
                }
            }

            w = async_fd.writable(), if has_pending => {
                let mut guard = match w {
                    Ok(g) => g,
                    Err(e) => {
                        error!("PTY writable poll failed on {:?}: {}", channel_id, e);
                        break;
                    }
                };

                let mut fatal = false;
                while let Some(front) = write_pending.front() {
                    if front.is_empty() {
                        write_pending.pop_front();
                        continue;
                    }
                    let wrote = match guard.try_io(|io_ref| -> io::Result<usize> {
                        let fd = io_ref.get_ref();
                        match write(fd, front) {
                            Ok(n) => Ok(n),
                            Err(Errno::INTR) => Err(io::Error::from(ErrorKind::Interrupted)),
                            Err(Errno::AGAIN) => Err(io::Error::from(ErrorKind::WouldBlock)),
                            Err(err) => Err(io::Error::other(err.to_string())),
                        }
                    }) {
                        Ok(Ok(n)) => n,
                        Ok(Err(e)) if e.kind() == ErrorKind::Interrupted => continue,
                        Ok(Err(e)) => {
                            error!("PTY write error on {:?}: {}", channel_id, e);
                            fatal = true;
                            break;
                        }
                        Err(_would_block) => break,
                    };

                    if wrote == 0 {
                        warn!("PTY write returned 0 on {:?}", channel_id);
                        fatal = true;
                        break;
                    }
                    if wrote >= front.len() {
                        write_pending.pop_front();
                    } else {
                        let rest = front.slice(wrote..);
                        write_pending.pop_front();
                        write_pending.push_front(rest);
                        break;
                    }
                }
                drop(guard);
                if fatal {
                    break;
                }
            }

            _ = reaper.tick(), if child_alive => {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        child_alive = false;
                        info!("PTY child exited {:?} on channel {:?}", st, channel_id);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("PTY child try_wait error on {:?}: {}", channel_id, e);
                        child_alive = false;
                        break;
                    }
                }
            }
        }
    }

    // Drop in-flight send without awaiting (Stopping wins).
    drop(inflight_send);

    if child_alive {
        reclaim_child(&mut child, channel_id).await;
    }

    cleanup_files(cleanup_paths).await;
    let _ = data_channel.close().await;
}

async fn reclaim_child(child: &mut Child, channel_id: u16) {
    kill_pty_process_group(child);
    if let Err(e) = child.start_kill() {
        debug!("start_kill on {:?}: {}", channel_id, e);
    }
    match tokio::time::timeout(CHILD_WAIT_TIMEOUT, child.wait()).await {
        Ok(Ok(st)) => debug!("child wait after kill {:?}: {:?}", channel_id, st),
        Ok(Err(e)) => debug!("child wait after kill error on {:?}: {}", channel_id, e),
        Err(_) => warn!("child wait timed out on channel {:?}", channel_id),
    }
}

async fn cleanup_files(paths: Vec<PathBuf>) {
    for path in paths {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            debug!("cleanup remove {}: {}", path.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_roundtrip() {
        let size = TerminalSize { rows: 24, cols: 80 };
        let json = serde_json::to_string(&size).unwrap();
        let back: TerminalSize = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, 24);
        assert_eq!(back.cols, 80);
    }

    #[test]
    fn terminal_handle_stop_is_idempotent() {
        let cancel = CancellationToken::new();
        let handle = TerminalHandle {
            cancel: cancel.clone(),
            join: Mutex::new(None),
        };
        assert!(!cancel.is_cancelled());
        handle.stop();
        assert!(cancel.is_cancelled());
        handle.stop();
        assert!(cancel.is_cancelled());
    }
}
