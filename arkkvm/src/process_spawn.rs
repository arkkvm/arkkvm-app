//! Child-process FD isolation for multithreaded `arkkvm_app`.
//!
//! After `fork`, the child inherits every open FD (Zenoh SHM SEQPACKET, HTTP
//! listeners, video devices, …). No external child needs those; Zenoh peers
//! connect via named unix sockets. This module closes `fd >= 3` in `pre_exec`
//! (async-signal-safe), keeping only the stdio the caller already wired.
//!
//! # Safety
//!
//! Closures registered with [`isolate_tokio_command`] / [`isolate_std_command`]
//! run after `fork` and may only call async-signal-safe functions.

use std::io;
use std::os::unix::process::CommandExt;

/// Lowest FD closed by isolation. 0/1/2 stay as the child's stdin/stdout/stderr.
pub const CHILD_FD_KEEP_BELOW: i32 = 3;

/// Close every open FD with number `>= min_fd`.
///
/// Prefers Linux `close_range` (syscall); falls back to a signal-safe `close` loop.
///
/// # Safety
///
/// Must only be called from a `pre_exec` (or equivalent post-`fork`) context.
pub unsafe fn close_fds_from(min_fd: i32) {
    if min_fd < 0 {
        return;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // Prefer the syscall so we do not require a libc symbol (uclibc may lack it).
        #[allow(unused_imports)]
        use libc::{c_long, c_ulong};
        let first = min_fd as c_ulong;
        let last = c_ulong::MAX;
        let flags: c_ulong = 0;
        // SYS_close_range is 436 on arm/aarch64/x86_64 Linux.
        let sys_nr: c_long = {
            #[cfg(any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "x86_64",
                target_arch = "riscv32",
                target_arch = "riscv64"
            ))]
            {
                436
            }
            #[cfg(not(any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "x86_64",
                target_arch = "riscv32",
                target_arch = "riscv64"
            )))]
            {
                -1
            }
        };
        if sys_nr >= 0 {
            let ret = unsafe { libc::syscall(sys_nr, first, last, flags) };
            if ret == 0 {
                return;
            }
        }
    }

    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max_fd = if max > 0 { max as i32 } else { 1024 };
    for fd in min_fd..max_fd {
        // Ignore errors: FD may not be open.
        unsafe {
            let _ = libc::close(fd);
        }
    }
}

/// Register `pre_exec` that closes all FDs `>= `[`CHILD_FD_KEEP_BELOW`].
///
/// Call **after** configuring `stdin`/`stdout`/`stderr` so those land on 0/1/2.
pub fn isolate_tokio_command(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            close_fds_from(CHILD_FD_KEEP_BELOW);
            Ok(())
        });
    }
}

/// Same as [`isolate_tokio_command`] for [`std::process::Command`].
pub fn isolate_std_command(cmd: &mut std::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            close_fds_from(CHILD_FD_KEEP_BELOW);
            Ok(())
        });
    }
}

/// Apply FD isolation then await [`tokio::process::Command::output`].
pub async fn output(mut cmd: tokio::process::Command) -> io::Result<std::process::Output> {
    isolate_tokio_command(&mut cmd);
    cmd.output().await
}

/// Apply FD isolation then [`std::process::Command::output`].
pub fn output_std(mut cmd: std::process::Command) -> io::Result<std::process::Output> {
    isolate_std_command(&mut cmd);
    cmd.output()
}

/// Apply FD isolation then [`std::process::Command::status`].
pub fn status_std(mut cmd: std::process::Command) -> io::Result<std::process::ExitStatus> {
    isolate_std_command(&mut cmd);
    cmd.status()
}

/// Apply FD isolation then [`std::process::Command::spawn`].
pub fn spawn_std(mut cmd: std::process::Command) -> io::Result<std::process::Child> {
    isolate_std_command(&mut cmd);
    cmd.spawn()
}

/// Apply FD isolation then [`tokio::process::Command::spawn`].
pub fn spawn_tokio(mut cmd: tokio::process::Command) -> io::Result<tokio::process::Child> {
    isolate_tokio_command(&mut cmd);
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_below_is_stdio() {
        assert_eq!(CHILD_FD_KEEP_BELOW, 3);
    }
}
