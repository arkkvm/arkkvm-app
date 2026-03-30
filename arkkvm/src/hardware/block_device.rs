use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
use parking_lot::RwLock;

/// Pluggable remote image reader.
/// Implementations should be cheap to clone or wrapped in Arc.
pub trait RemoteImageReader: Send + Sync + 'static {
    /// Read `len` bytes starting at `off`.
    fn read_at(&self, off: i64, len: i64) -> Result<Vec<u8>>;
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
                // Best-effort disconnect; ignore errors, avoid raw ioctl here
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
            // Remove stale socket path
            let _ = rustix::fs::unlink(NBD_SOCKET_PATH);
        }
    }

    pub(super) fn start_linux_nbd() -> Result<LinuxNbdState> {
        // Ensure device exists
        if !Path::new(NBD_DEVICE_PATH).exists() {
            return Err(anyhow!("NBD device does not exist: {}", NBD_DEVICE_PATH));
        }

        // Open device read-write; ioctl may require write; kernel ignores writes in read-only mode
        let device = OpenOptions::new()
            .read(true)
            .write(true) // ioctl may require write; kernel ignores writes for RO
            .open(NBD_DEVICE_PATH)
            .context("failed to open NBD device")?;

        // Clean stale socket
        if Path::new(NBD_SOCKET_PATH).exists() {
            rustix::fs::unlink(NBD_SOCKET_PATH).with_context(|| {
                format!("failed to remove existing socket: {}", NBD_SOCKET_PATH)
            })?;
        }

        let listener = UnixListener::bind(NBD_SOCKET_PATH)
            .with_context(|| format!("failed to bind unix socket: {}", NBD_SOCKET_PATH))?;

        // Dial to self to create a pair
        let client_conn =
            UnixStream::connect(NBD_SOCKET_PATH).context("failed to connect unix socket")?;

        // Accept the server side
        let (server_conn, _) = listener.accept().context("failed to accept unix socket")?;

        // Spawn server loop: serve NBD protocol, export name arkkvm
        let server_thread = thread::spawn({
            let mut sc = server_conn.try_clone().expect("dup server conn");
            move || {
                let _ = run_server(&mut sc);
            }
        });

        // Spawn client loop: connect device to the socket
        let device_clone = device.try_clone().context("dup device")?;
        let client_conn_clone = client_conn.try_clone().context("dup client conn")?;
        let client_thread = thread::spawn(move || {
            let _ = run_client_connect(client_conn_clone, device_clone);
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

        // Prepare export via handshake
        let device = server::handshake(&mut *conn, |export_name| {
            // Single export named "arkkvm". Kernel oldstyle may send empty name; accept both.
            if !(export_name.is_empty() || export_name == "arkkvm") {
                return Err(std::io::Error::other("unknown export"));
            }
            let res: Result<nbd::Export<_>> = (|| -> Result<_> {
                let reader = get_current_remote_image_reader()
                    .ok_or_else(|| anyhow!("image not mounted"))?;
                let size = reader.size()?;
                Ok(nbd::Export {
                    size: size as u64,
                    readonly: true,
                    resizeable: false,
                    rotational: false,
                    send_trim: false,
                    send_flush: false,
                    data: RemoteImageDevice::new(reader, size as u64),
                })
            })();
            anyhow_to_io(res)
        })?;

        server::transmission(&mut *conn, device)?;
        Ok(())
    }

    struct RemoteImageDevice {
        reader: Arc<dyn RemoteImageReader>,
        size: u64,
        pos: u64,
    }

    impl RemoteImageDevice {
        fn new(reader: Arc<dyn RemoteImageReader>, size: u64) -> Self {
            Self { reader, size, pos: 0 }
        }
    }

    impl Read for RemoteImageDevice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.size {
                return Ok(0);
            }
            let max = (self.size - self.pos) as usize;
            let to_read = buf.len().min(max);
            if to_read == 0 {
                return Ok(0);
            }
            let data = self.reader.read_at(self.pos as i64, to_read as i64).map_err(to_io_other)?;
            let n = data.len().min(to_read);
            buf[..n].copy_from_slice(&data[..n]);
            self.pos += n as u64;
            Ok(n)
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
                SeekFrom::End(x) => self.size as i128 + (x as i128),
            };
            if new_pos < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek before start",
                ));
            }
            let new_pos = new_pos as u64;
            self.pos = new_pos;
            Ok(self.pos)
        }
    }

    // Minimal ioctl bindings for NBD device
    const NBD_SET_SOCK: libc::c_ulong = 0x0000ab00;
    const NBD_SET_BLKSIZE: libc::c_ulong = 0x0000ab01;
    const NBD_SET_SIZE: libc::c_ulong = 0x0000ab02;
    const NBD_DO_IT: libc::c_ulong = 0x0000ab03;
    const NBD_CLEAR_SOCK: libc::c_ulong = 0x0000ab04;
    const NBD_DISCONNECT: libc::c_ulong = 0x0000ab08;
    const NBD_SET_TIMEOUT: libc::c_ulong = 0x0000ab09;
    const NBD_SET_FLAGS: libc::c_ulong = 0x0000ab0a;
    const NBD_FLAG_READ_ONLY_IOCTL: u64 = 2; // read-only

    fn ioctl_set_u64(fd: BorrowedFd<'_>, req: libc::c_ulong, val: u64) -> Result<()> {
        // On 64-bit, third arg is unsigned long; cast directly. On 32-bit, check overflow.
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

    #[inline]
    fn anyhow_to_io<T>(res: Result<T>) -> std::io::Result<T> {
        res.map_err(to_io_other)
    }

    fn run_client_connect(conn: UnixStream, device: File) -> Result<()> {
        let sock_fd = conn.as_raw_fd();
        let dev_fd = device.as_fd();

        // Configure device parameters before attaching socket
        if let Some(reader) = get_current_remote_image_reader() {
            let size = reader.size().context("get image size")? as u64;
            ioctl_set_u64(dev_fd, NBD_SET_SIZE, size).context("ioctl NBD_SET_SIZE")?;
        }
        // Read-only flag
        let _ = ioctl_set_u64(dev_fd, NBD_SET_FLAGS, NBD_FLAG_READ_ONLY_IOCTL);
        // Timeout (seconds)
        let _ = ioctl_set_u64(dev_fd, NBD_SET_TIMEOUT, 5);
        // Set block size to 4KiB
        ioctl_set_u64(dev_fd, NBD_SET_BLKSIZE, 4096).context("ioctl NBD_SET_BLKSIZE")?;
        // Associate socket with kernel NBD
        ioctl_set_u64(dev_fd, NBD_SET_SOCK, sock_fd as u64).context("ioctl NBD_SET_SOCK")?;

        // Run. This blocks until disconnect
        let _ = ioctl_simple(dev_fd, NBD_DO_IT);

        // Teardown
        let _ = ioctl_simple(dev_fd, NBD_CLEAR_SOCK);
        Ok(())
    }
}
