use std::fs::File;
use std::io::{ErrorKind, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

pub const HID_WRITE_TIMEOUT: Duration = Duration::from_millis(100);

pub fn set_std_file_non_blocking(file: &File) -> Result<(), ()> {
    let fd = file.as_fd();
    let flags = fcntl_getfl(fd).map_err(|_| ())?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(|_| ())?;
    Ok(())
}

pub fn prepare_hid_write_file(file: &File) {
    let _ = set_std_file_non_blocking(file);
}

pub fn write_all_with_timeout(
    file: &mut File,
    mut buf: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    prepare_hid_write_file(file);
    let deadline = Instant::now() + timeout;
    while !buf.is_empty() {
        match file.write(buf) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "hid write timeout",
                    ));
                }
                let poll_timeout = Timespec::try_from(remaining).map_err(|_| {
                    std::io::Error::new(ErrorKind::TimedOut, "hid write timeout")
                })?;
                let mut poll_fd = PollFd::new(file, PollFlags::OUT);
                match poll(&mut [poll_fd], Some(&poll_timeout)) {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            ErrorKind::TimedOut,
                            "hid write timeout",
                        ));
                    }
                    Ok(_) => {}
                    Err(err) => return Err(std::io::Error::from(err)),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
