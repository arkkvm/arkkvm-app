use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
use parking_lot::RwLock;

/// Pluggable remote image reader.
/// Implementations should be cheap to clone or wrapped in Arc.
pub trait RemoteImageReader: Send + Sync + 'static {
    /// Read up to `buf.len()` bytes from `off` into `buf`.
    /// Returns how many bytes were written (≤ `buf.len()`). Short reads are allowed.
    fn read_at(&self, off: i64, buf: &mut [u8]) -> Result<usize>;
    /// Total size in bytes.
    fn size(&self) -> Result<i64>;
}

/// Global handle for current mounted virtual media.
static CURRENT_READER: Lazy<RwLock<Option<Arc<dyn RemoteImageReader>>>> =
    Lazy::new(|| RwLock::new(None));

/// Set current remote image reader. Passing None clears the reader.
pub fn set_current_remote_image_reader(reader: Option<Arc<dyn RemoteImageReader>>) {
    let mut guard = CURRENT_READER.write();
    *guard = reader;
}

/// Get a cloned Arc to current reader if any.
pub fn get_current_remote_image_reader() -> Option<Arc<dyn RemoteImageReader>> {
    CURRENT_READER.read().clone()
}

fn nbd_timeout_secs() -> u64 {
    const DEFAULT_TIMEOUT_SECS: u64 = 45;
    std::env::var("ARKKVM_NBD_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|v| *v >= 5 && *v <= 600)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// NBD device driver wrapper.
#[derive(Default)]
pub struct NbdDevice {
    #[cfg(target_os = "linux")]
    state: Option<linux::LinuxNbdState>,
}

impl NbdDevice {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start NBD server and connect it to /dev/nbd0 via a local Unix socket.
    pub fn start(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            if self.state.is_some() {
                return Ok(());
            }
            let st = linux::start_linux_nbd().context("failed to start NBD on Linux")?;
            self.state = Some(st);
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("NBD is not supported on this platform"))
        }
    }

    /// Close/teardown NBD resources.
    pub fn close(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if let Some(mut st) = self.state.take() {
                st.close();
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::thread::{self, JoinHandle};

    use super::*;

    /// NBD paths used by the device.
    const NBD_SOCKET_PATH: &str = "/var/run/nbd.socket";
    const NBD_DEVICE_PATH: &str = "/dev/nbd0";

    pub(super) struct LinuxNbdState {
        listener: Option<UnixListener>,
        server_conn: Option<UnixStream>,
        client_conn: Option<UnixStream>,
        device: Option<File>,
        server_thread: Option<JoinHandle<()>>,
        client_thread: Option<JoinHandle<()>>,
    }

    impl LinuxNbdState {
        pub fn close(&mut self) {
            // Try to disconnect client first
            if let Some(dev) = self.device.take() {
                let fd = dev.as_fd();
                let _ = ioctl_simple(fd, super::linux::NBD_DISCONNECT);
                let _ = ioctl_simple(fd, super::linux::NBD_CLEAR_SOCK);
                drop(dev);
            }
            if let Some(conn) = self.client_conn.take() {
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
            if let Some(conn) = self.server_conn.take() {
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
            if let Some(listener) = self.listener.take() {
                drop(listener);
            }
            if let Some(h) = self.server_thread.take() {
                let _ = h.join();
            }
            if let Some(h) = self.client_thread.take() {
                let _ = h.join();
            }
            let _ = rustix::fs::unlink(NBD_SOCKET_PATH);
        }
    }

    pub(super) fn start_linux_nbd() -> Result<LinuxNbdState> {
        if !Path::new(NBD_DEVICE_PATH).exists() {
            return Err(anyhow!("NBD device does not exist: {}", NBD_DEVICE_PATH));
        }

        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(NBD_DEVICE_PATH)
            .context("failed to open NBD device")?;

        if Path::new(NBD_SOCKET_PATH).exists() {
            rustix::fs::unlink(NBD_SOCKET_PATH).with_context(|| {
                format!("failed to remove existing socket: {}", NBD_SOCKET_PATH)
            })?;
        }

        let listener = UnixListener::bind(NBD_SOCKET_PATH)
            .with_context(|| format!("failed to bind unix socket: {}", NBD_SOCKET_PATH))?;

        let client_conn =
            UnixStream::connect(NBD_SOCKET_PATH).context("failed to connect unix socket")?;

        let (server_conn, _) = listener.accept().context("failed to accept unix socket")?;

        let server_thread = thread::spawn({
            let mut sc = server_conn.try_clone().expect("dup server conn");
            move || {
                if let Err(e) = run_server(&mut sc) {
                    if is_expected_disconnect_error(&e) {
                        tracing::warn!(error = %e, "nbd userspace server exited after disconnect");
                    } else {
                        tracing::error!(error = %e, "nbd userspace server exited with error");
                    }
                }
            }
        });

        let device_clone = device.try_clone().context("dup device")?;
        let client_conn_clone = client_conn.try_clone().context("dup client conn")?;
        let client_thread = thread::spawn(move || {
            if let Err(e) = run_client_connect(client_conn_clone, device_clone) {
                tracing::error!(error = %e, "nbd kernel client thread exited with error");
            }
        });

        Ok(LinuxNbdState {
            listener: Some(listener),
            server_conn: Some(server_conn),
            client_conn: Some(client_conn),
            device: Some(device),
            server_thread: Some(server_thread),
            client_thread: Some(client_thread),
        })
    }

    fn run_server(conn: &mut UnixStream) -> Result<()> {
        use nbd::server;

        // Kernel ioctl NBD: socket must enter transmission directly (no userspace handshake).
        let reader =
            get_current_remote_image_reader().ok_or_else(|| anyhow!("nbd: image not mounted"))?;
        let data_size = reader.size()? as u64;
        let export_size = align_export_size(data_size);
        let device = RemoteImageDevice::new(reader, data_size, export_size);
        server::transmission(&mut *conn, device)?;
        Ok(())
    }

    fn is_expected_disconnect_error(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
                matches!(
                    ioe.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::NotConnected
                )
            } else {
                false
            }
        })
    }

    #[inline]
    fn align_export_size(data_size: u64) -> u64 {
        const BLK: u64 = 4096;
        (data_size + BLK - 1) & !(BLK - 1)
    }

    struct RemoteImageDevice {
        reader: Arc<dyn RemoteImageReader>,
        data_size: u64,
        export_size: u64,
        pos: u64,
    }

    impl RemoteImageDevice {
        fn new(reader: Arc<dyn RemoteImageReader>, data_size: u64, export_size: u64) -> Self {
            Self {
                reader,
                data_size,
                export_size,
                pos: 0,
            }
        }
    }

    impl Read for RemoteImageDevice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.export_size {
                return Ok(0);
            }
            let max = (self.export_size - self.pos) as usize;
            let to_read = buf.len().min(max);
            if to_read == 0 {
                return Ok(0);
            }
            if self.pos >= self.data_size {
                buf[..to_read].fill(0);
                self.pos += to_read as u64;
                return Ok(to_read);
            }
            let n_from_image = to_read.min((self.data_size - self.pos) as usize);
            let mut filled = 0usize;
            while filled < n_from_image {
                let n = self
                    .reader
                    .read_at((self.pos + filled as u64) as i64, &mut buf[filled..n_from_image])
                    .map_err(to_io_other)?;
                if n > (n_from_image - filled) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "read_at wrote past buffer",
                    ));
                }
                if n == 0 {
                    break;
                }
                filled += n;
            }

            if filled < n_from_image {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "remote image short read at off={} want={} got={}",
                        self.pos, n_from_image, filled
                    ),
                ));
            }

            if n_from_image < to_read {
                // Padding region beyond source image EOF should be deterministic zeros.
                buf[n_from_image..to_read].fill(0);
            }

            self.pos += to_read as u64;
            Ok(to_read)
        }
    }

    impl Write for RemoteImageDevice {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "read-only export"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for RemoteImageDevice {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let new_pos: i128 = match pos {
                SeekFrom::Start(x) => x as i128,
                SeekFrom::Current(x) => self.pos as i128 + (x as i128),
                SeekFrom::End(x) => self.export_size as i128 + (x as i128),
            };
            if new_pos < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek before start",
                ));
            }
            let new_pos = new_pos as u64;
            if new_pos > self.export_size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek past end of export",
                ));
            }
            self.pos = new_pos;
            Ok(self.pos)
        }
    }

    // linux/nbd.h NBD_* ioctls
    const NBD_SET_SOCK: libc::c_ulong = 0x0000ab00;
    const NBD_SET_BLKSIZE: libc::c_ulong = 0x0000ab01;
    const NBD_SET_SIZE: libc::c_ulong = 0x0000ab02;
    const NBD_SET_SIZE_BLOCKS: libc::c_ulong = 0x0000ab07;
    const NBD_DO_IT: libc::c_ulong = 0x0000ab03;
    const NBD_CLEAR_SOCK: libc::c_ulong = 0x0000ab04;
    const NBD_DISCONNECT: libc::c_ulong = 0x0000ab08;
    const NBD_SET_TIMEOUT: libc::c_ulong = 0x0000ab09;
    const NBD_SET_FLAGS: libc::c_ulong = 0x0000ab0a;
    const NBD_FLAG_READ_ONLY_IOCTL: u64 = 2; // read-only

    fn ioctl_set_u64(fd: BorrowedFd<'_>, req: libc::c_ulong, val: u64) -> Result<()> {
        #[cfg(target_pointer_width = "32")]
        let arg: libc::c_ulong = match <libc::c_ulong as TryFrom<u64>>::try_from(val) {
            Ok(v) => v,
            Err(_) => return Err(anyhow!("ioctl value overflows c_ulong")),
        };

        #[cfg(target_pointer_width = "64")]
        let arg: libc::c_ulong = val as libc::c_ulong;

        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn ioctl_simple(fd: BorrowedFd<'_>, req: libc::c_ulong) -> Result<()> {
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), req as _) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[inline]
    fn to_io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
        std::io::Error::other(e.to_string())
    }

    fn run_client_connect(conn: UnixStream, device: File) -> Result<()> {
        let sock_fd = conn.as_raw_fd();
        let dev_fd = device.as_fd();

        // Set BLKSIZE before SIZE on old kernels (Rockchip BSP) or size can truncate.
        const NBD_BLKSIZE: u64 = 4096;
        if let Some(reader) = get_current_remote_image_reader() {
            let data_size = reader.size().context("get image size")? as u64;
            let export_size = align_export_size(data_size);
            let blocks = export_size / NBD_BLKSIZE;
            ioctl_set_u64(dev_fd, NBD_SET_BLKSIZE, NBD_BLKSIZE).context("ioctl NBD_SET_BLKSIZE")?;
            ioctl_set_u64(dev_fd, NBD_SET_SIZE_BLOCKS, blocks)
                .context("ioctl NBD_SET_SIZE_BLOCKS")?;
            // Omit NBD_SET_SIZE: ioctl(2) third arg is unsigned long; on 32-bit, byte sizes
            // > 2^32-1 cannot be passed. SET_SIZE_BLOCKS already sets the same bytesize.
            // ioctl_set_u64(dev_fd, NBD_SET_SIZE, export_size).context("ioctl NBD_SET_SIZE")?;
        }
        let _ = ioctl_set_u64(dev_fd, NBD_SET_FLAGS, NBD_FLAG_READ_ONLY_IOCTL);
        let timeout_secs = nbd_timeout_secs();
        let _ = ioctl_set_u64(dev_fd, NBD_SET_TIMEOUT, timeout_secs);
        tracing::info!(timeout_secs, "nbd kernel timeout configured");
        ioctl_set_u64(dev_fd, NBD_SET_SOCK, sock_fd as u64).context("ioctl NBD_SET_SOCK")?;

        let _ = ioctl_simple(dev_fd, NBD_DO_IT);

        let _ = ioctl_simple(dev_fd, NBD_CLEAR_SOCK);
        Ok(())
    }
}
