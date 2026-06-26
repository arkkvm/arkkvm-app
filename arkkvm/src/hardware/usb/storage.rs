use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex as StdMutex};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
// use parking_lot::{Mutex, RwLock};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::assets::BuiltinImages;
use crate::config::get_config_manager;
use crate::hardware::block_device::{
    NbdDevice, RemoteImageReader, set_current_remote_image_reader,
};
use crate::hardware::fs_remote::{
    AsyncFileReader, AsyncFileWriter, FileInfo, FileSystemInfo, IMG_FILE_NAME, IMG_FILE_PATH,
    REMOTE_FS, get_img_file_size,
};
use crate::services::usb::UsbClient;

/// Virtual media source types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VirtualMediaSource {
    WebRTC,
    HTTP,
    Storage,
}

/// Virtual media mode: CDROM or Disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VirtualMediaMode {
    CDROM,
    Disk,
}

pub enum UsbTarget {
    Usb0Lun0,
    Usb1Lun0,
}

/// Virtual media state kept globally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualMediaState {
    pub source: VirtualMediaSource,
    pub mode: VirtualMediaMode,
    pub filename: Option<String>,
    pub url: Option<String>,
    pub size: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileTransferTarget {
    None,
    Kvm,
    RemoteUsb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransferState {
    pub target: FileTransferTarget,
}

const IMAGES_FOLDER: &str = "/userdata/arkkvm/images"; // virtual media

static CURRENT_VIRTUAL_MEDIA_STATE: RwLock<Option<VirtualMediaState>> = RwLock::new(None);
static NBD_DEVICE: RwLock<Option<NbdDevice>> = RwLock::new(None);

fn require_usb() -> Result<Arc<UsbClient>> {
    crate::services::get_usb().ok_or_else(|| anyhow!("USB sidecar not available"))
}

/// VM LUN mode from usb_devices sidecar runtime state.
pub async fn get_vm_mode_from_sidecar() -> Result<VirtualMediaMode> {
    use crate::proto::v1::{UmsTarget, UmsVmType};
    let usb = require_usb()?;
    let state = usb.ums_get_mount_state(UmsTarget::UmsVm).await?;
    match UmsVmType::try_from(state.vm_type) {
        Ok(UmsVmType::VmDisk) => Ok(VirtualMediaMode::Disk),
        _ => Ok(VirtualMediaMode::CDROM),
    }
}

/// Resolve VM/FT backing paths and VM LUN mode for usb_devices apply.
pub async fn resolve_ums_paths_for_apply() -> Result<(String, String, crate::proto::v1::UmsVmType)> {
    use crate::proto::v1::{UmsControlResponse, UmsTarget, UmsVmType};

    let usb = require_usb()?;
    let vm = usb.ums_get_mount_state(UmsTarget::UmsVm).await.unwrap_or_else(|e| {
        warn!("resolve_ums_paths: vm mount state query failed: {}", e);
        UmsControlResponse {
            ok: false,
            error: Some(e.to_string()),
            mounted: false,
            mounted_path: String::new(),
            vm_type: UmsVmType::VmCdRom as i32,
        }
    });
    let ft = usb.ums_get_mount_state(UmsTarget::UmsFt).await.unwrap_or_else(|e| {
        warn!("resolve_ums_paths: ft mount state query failed: {}", e);
        UmsControlResponse {
            ok: false,
            error: Some(e.to_string()),
            mounted: false,
            mounted_path: String::new(),
            vm_type: UmsVmType::Unknown as i32,
        }
    });
    let ums_vm_type = UmsVmType::try_from(vm.vm_type).unwrap_or(UmsVmType::VmCdRom);
    Ok((
        if vm.mounted {
            vm.mounted_path
        } else {
            String::new()
        },
        if ft.mounted {
            ft.mounted_path
        } else {
            String::new()
        },
        ums_vm_type,
    ))
}

/// Public API: get current virtual media state.
pub fn get_virtual_media_state() -> Option<VirtualMediaState> {
    CURRENT_VIRTUAL_MEDIA_STATE.read().clone()
}

pub async fn get_file_transfer_state() -> Result<FileTransferState> {
    let remote_fs = REMOTE_FS.read().await;
    if remote_fs.is_mounted() {
        return Ok(FileTransferState { target: FileTransferTarget::Kvm });
    }
    drop(remote_fs);

    if let Some(usb) = crate::services::get_usb() {
        use crate::proto::v1::{UmsControlResponse, UmsTarget};
        let ft = usb.ums_get_mount_state(UmsTarget::UmsFt).await.unwrap_or_else(|e| {
            warn!("get_file_transfer_state: ft mount state query failed: {}", e);
            UmsControlResponse {
                ok: false,
                error: Some(e.to_string()),
                mounted: false,
                mounted_path: String::new(),
                vm_type: 0,
            }
        });
        if ft.mounted {
            let expected = format!("{}/{}", IMG_FILE_PATH, IMG_FILE_NAME);
            if ft.mounted_path == expected {
                return Ok(FileTransferState { target: FileTransferTarget::RemoteUsb });
            }
        }
    }
    Ok(FileTransferState { target: FileTransferTarget::None })
}

/// Mount remote HTTP image via NBD and point mass storage to /dev/nbd0.
pub async fn mount_with_http(url: &str, mode: VirtualMediaMode) -> Result<()> {
    ensure_images_folder().await?;
    set_mass_storage_mode(mode == VirtualMediaMode::CDROM, true, UsbTarget::Usb0Lun0)
        .await?;

    // Create HTTP/HTTPS reader and determine size
    let reader = HttpRangeReader::new(url)?;
    let size = reader.size()?;
    info!("using remote HTTP url: {}, size: {}", reader.url(), size);

    // Update state, then start NBD
    {
        let mut st = CURRENT_VIRTUAL_MEDIA_STATE.write();
        if st.is_some() {
            return Err(anyhow!("another virtual media is already mounted"));
        }
        *st = Some(VirtualMediaState {
            source: VirtualMediaSource::HTTP,
            mode,
            filename: None,
            url: Some(url.to_string()),
            size,
        });
    }

    set_current_remote_image_reader(Some(Arc::new(reader)));
    start_nbd()?;
    wait_nbd_ready().await?;
    set_mass_storage_image("/dev/nbd0", UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage image to nbd0")?;
    info!("usb mass storage mounted");
    Ok(())
}

/// Mount WebRTC provided image via NBD. Requires external read handler to be set.
pub async fn mount_with_webrtc(filename: &str, size: i64, mode: VirtualMediaMode) -> Result<()> {
    set_mass_storage_mode(mode == VirtualMediaMode::CDROM, true, UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage mode")?;

    {
        let mut st = CURRENT_VIRTUAL_MEDIA_STATE.write();
        if st.is_some() {
            return Err(anyhow!("another virtual media is already mounted"));
        }
        *st = Some(VirtualMediaState {
            source: VirtualMediaSource::WebRTC,
            mode,
            filename: Some(filename.to_string()),
            url: None,
            size,
        });
    }

    let reader = WebRtcDiskReader::new(size);
    set_current_remote_image_reader(Some(Arc::new(reader)));

    start_nbd()?;
    wait_nbd_ready().await?;
    set_mass_storage_image("/dev/nbd0", UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage image to nbd0")?;
    info!("usb mass storage mounted");
    Ok(())
}

/// Mount a local storage file as mass storage directly (no NBD).
pub async fn mount_with_storage(filename: &str, mode: VirtualMediaMode) -> Result<()> {
    let filename = sanitize_filename(filename)?;
    ensure_images_folder().await?;
    let full_path = Path::new(IMAGES_FOLDER).join(&filename);
    let meta = fs::metadata(&full_path)
        .await
        .with_context(|| format!("failed to stat file: {}", filename))?;

    let cdrom = mode == VirtualMediaMode::CDROM;
    // Local disk image: allow host writes when presented as fixed/removable disk (not CD-ROM).
    set_mass_storage_mode(cdrom, cdrom, UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage mode")?;
    set_mass_storage_image(
        full_path.to_str().ok_or_else(|| anyhow!("invalid path"))?,
        UsbTarget::Usb0Lun0,
    )
    .await
    .context("failed to set mass storage image")?;

    *CURRENT_VIRTUAL_MEDIA_STATE.write() = Some(VirtualMediaState {
        source: VirtualMediaSource::Storage,
        mode,
        filename: Some(filename),
        url: None,
        size: meta.len() as i64,
    });
    Ok(())
}

/// Mount a local storage file as mass storage directly (no NBD).
pub async fn mount_with_file_img() -> Result<()> {
    info!("File Transfer mount with file image");
    match get_file_transfer_state().await?.target {
        FileTransferTarget::None => {} // do nothing
        FileTransferTarget::Kvm => unload_with_file_img().await?,
        FileTransferTarget::RemoteUsb => return Ok(()),
    }

    let full_path = Path::new(IMG_FILE_PATH).join(IMG_FILE_NAME);
    if !full_path.exists() {
        return Err(anyhow!("image file not found"));
    }

    let _meta = fs::metadata(&full_path)
        .await
        .with_context(|| format!("failed to stat file: {}", IMG_FILE_NAME))?;

    set_mass_storage_mode(false, false, UsbTarget::Usb1Lun0)
        .await
        .context("failed to set mass storage mode")?;
    set_mass_storage_image(
        full_path.to_str().ok_or_else(|| anyhow!("invalid path"))?,
        UsbTarget::Usb1Lun0,
    )
    .await
    .context("failed to set mass storage image")?;

    let config = get_config_manager();
    let _ = config.set_ft_mount_target(FileTransferTarget::RemoteUsb).await?;
    info!("File Transfer mount target set to RemoteUsb");
    Ok(())
}

pub async fn unmount_file_img() -> Result<()> {
    info!("File Transfer unmounted file image from RemoteUsb");
    let result = match get_file_transfer_state().await?.target {
        FileTransferTarget::None => return Ok(()),
        FileTransferTarget::Kvm => unload_with_file_img().await,
        FileTransferTarget::RemoteUsb => require_usb()?.ums_unmount_ft().await,
    };

    if let Err(e) = result {
        error!("Failed to unmount file image: {:?}", e);
        return Err(e);
    }

    let config = get_config_manager();
    let _ = config.set_ft_mount_target(FileTransferTarget::None).await?;
    info!("File Transfer unmounted RemoteUsb successfully");
    Ok(())
}

/// Unmount any mounted image and stop NBD if running.
pub async fn unmount_image() -> Result<()> {
    if let Ok(usb) = require_usb() {
        usb.ums_unmount_vm()
            .await
            .unwrap_or_else(|e| warn!("failed to unmount vm via sidecar: {}", e));
    }
    sleep(Duration::from_millis(500)).await;
    stop_nbd();
    *CURRENT_VIRTUAL_MEDIA_STATE.write() = None;
    set_current_remote_image_reader(None);
    Ok(())
}

/// Ensure images folder exists.
async fn ensure_images_folder() -> Result<()> {
    fs::create_dir_all(IMAGES_FOLDER).await.context("failed to create images folder")
}

/// Sanitize filename to prevent path traversal.
fn sanitize_filename(filename: &str) -> Result<String> {
    use std::path::Component;
    let p = Path::new(filename);
    if p.is_absolute() {
        return Err(anyhow!("invalid filename"));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(anyhow!("invalid filename"));
    }
    let base = p.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("invalid filename"))?;
    if base.is_empty() || base == "." {
        return Err(anyhow!("invalid filename"));
    }
    Ok(base.to_string())
}

fn is_ums_unmount_path(image_path: &str) -> bool {
    image_path.is_empty() || image_path == "\n"
}

/// Get current mass storage image file path via sidecar.
pub async fn get_mass_storage_image(usb_target: UsbTarget) -> Result<Option<String>> {
    use crate::proto::v1::UmsTarget;
    let usb = require_usb()?;
    let target = match usb_target {
        UsbTarget::Usb0Lun0 => UmsTarget::UmsVm,
        UsbTarget::Usb1Lun0 => UmsTarget::UmsFt,
    };
    let state = usb.ums_get_mount_state(target).await?;
    if state.mounted && !state.mounted_path.is_empty() {
        return Ok(Some(state.mounted_path));
    }
    Ok(None)
}

/// Mount/unmount mass storage image via usb_devices sidecar.
pub async fn set_mass_storage_image(image_path: &str, usb_target: UsbTarget) -> Result<()> {
    let usb = require_usb()?;
    match usb_target {
        UsbTarget::Usb0Lun0 => {
            if is_ums_unmount_path(image_path) {
                usb.ums_unmount_vm().await
            } else {
                let mode = CURRENT_VIRTUAL_MEDIA_STATE
                    .read()
                    .as_ref()
                    .map(|s| s.mode);
                let mode = match mode {
                    Some(m) => m,
                    None => get_vm_mode_from_sidecar().await?,
                };
                usb.ums_mount_vm(image_path, mode).await
            }
        }
        UsbTarget::Usb1Lun0 => {
            if is_ums_unmount_path(image_path) {
                usb.ums_unmount_ft().await
            } else {
                usb.ums_mount_ft(image_path).await
            }
        }
    }
}

/// Set mass storage VM mode (CD-ROM vs disk) via sidecar.
pub async fn set_mass_storage_mode(cdrom: bool, _read_only: bool, usb_target: UsbTarget) -> Result<()> {
    match usb_target {
        UsbTarget::Usb0Lun0 => {
            let usb = require_usb()?;
            let mode = if cdrom {
                VirtualMediaMode::CDROM
            } else {
                VirtualMediaMode::Disk
            };
            usb.ums_switch_vm_mode(mode).await
        }
        UsbTarget::Usb1Lun0 => Ok(()),
    }
}

fn start_nbd() -> Result<()> {
    let mut guard = NBD_DEVICE.write();
    if guard.is_none() {
        *guard = Some(NbdDevice::new());
    }
    if let Some(dev) = guard.as_mut() {
        dev.start().context("failed to start nbd device")?;
        debug!("nbd device started");
        return Ok(());
    }
    Err(anyhow!("nbd device not available"))
}

fn stop_nbd() {
    let mut guard = NBD_DEVICE.write();
    if let Some(dev) = guard.as_mut() {
        dev.close();
    }
    *guard = None;
}

const NBD_READY_TIMEOUT: Duration = Duration::from_secs(8);
const NBD_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn read_env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|v| *v >= min && *v <= max)
        .unwrap_or(default)
}

fn nbd_ready_timeout() -> Duration {
    Duration::from_secs(read_env_u64(
        "ARKKVM_NBD_READY_TIMEOUT_SECS",
        NBD_READY_TIMEOUT.as_secs(),
        1,
        60,
    ))
}

async fn wait_nbd_ready() -> Result<()> {
    let deadline = Instant::now() + nbd_ready_timeout();
    loop {
        let pid_ok = tokio::fs::read_to_string("/sys/block/nbd0/pid")
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|pid| pid > 0);
        let size_ok = tokio::fs::read_to_string("/sys/block/nbd0/size")
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|blocks| blocks > 0);

        if pid_ok && size_ok {
            info!("nbd ready gate passed");
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "nbd ready timeout: pid_ok={}, size_ok={}",
                pid_ok,
                size_ok
            ));
        }
        sleep(NBD_READY_POLL_INTERVAL).await;
    }
}

// ==========================
// HTTP/HTTPS Range Reader (reqwest + rustls)
// ==========================

const HTTP_RANGE_USER_AGENT: &str = "arkkvm-virtual-media/1.0";
const HTTP_RANGE_SEND_MAX_RETRIES: u32 = 4;
const HTTP_RANGE_SEND_RETRY_BASE_DELAY_MS: u64 = 200;
/// Max refetch attempts for the remaining range within a single read_at call.
const HTTP_RANGE_READ_REISSUE_MAX: u32 = 4;
const HTTP_PREFETCH_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB
const HTTP_PREFETCH_ENABLE_SEQ_READS: u32 = 3;
const HTTP_BLOCK_SIZE: usize = 4096;
const HTTP_BLOCK_CACHE_CAPACITY: usize = 2048;
const HTTP_INFLIGHT_MAX: usize = 4;
static HTTP_RANGE_REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct HttpMetrics {
    cache_hit: AtomicU64,
    cache_miss: AtomicU64,
    send_retry: AtomicU64,
    timeout_error: AtomicU64,
    connect_error: AtomicU64,
    short_body_error: AtomicU64,
    range_mismatch: AtomicU64,
}

struct InflightGate {
    inner: StdMutex<usize>,
    cv: Condvar,
    max: usize,
}

impl InflightGate {
    fn new(max: usize) -> Self {
        Self {
            inner: StdMutex::new(0),
            cv: Condvar::new(),
            max,
        }
    }

    fn acquire(&self) {
        let mut running = self.inner.lock().expect("inflight mutex poisoned");
        while *running >= self.max {
            running = self.cv.wait(running).expect("inflight cv poisoned");
        }
        *running += 1;
    }

    fn release(&self) {
        let mut running = self.inner.lock().expect("inflight mutex poisoned");
        if *running > 0 {
            *running -= 1;
        }
        self.cv.notify_one();
    }
}

struct InflightPermit<'a> {
    gate: &'a InflightGate,
}

impl<'a> InflightPermit<'a> {
    fn new(gate: &'a InflightGate) -> Self {
        gate.acquire();
        Self { gate }
    }
}

impl Drop for InflightPermit<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

struct BlockCache {
    map: HashMap<i64, Vec<u8>>,
    lru: VecDeque<i64>,
    cap: usize,
}

impl BlockCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            lru: VecDeque::new(),
            cap,
        }
    }

    fn touch(&mut self, key: i64) {
        if let Some(pos) = self.lru.iter().position(|k| *k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key);
    }

    fn put(&mut self, key: i64, value: Vec<u8>) {
        self.map.insert(key, value);
        self.touch(key);
        while self.map.len() > self.cap {
            if let Some(evict) = self.lru.pop_front() {
                self.map.remove(&evict);
            }
        }
    }
}

struct PrefetchState {
    enabled: bool,
    seq_reads: u32,
    last_off: Option<i64>,
    last_len: usize,
    buf: Vec<u8>,
    buf_off: i64,
    buf_len: usize,
    cache: BlockCache,
}

impl PrefetchState {
    fn new() -> Self {
        Self {
            enabled: false,
            seq_reads: 0,
            last_off: None,
            last_len: 0,
            buf: vec![0u8; HTTP_PREFETCH_CHUNK_SIZE],
            buf_off: 0,
            buf_len: 0,
            cache: BlockCache::new(HTTP_BLOCK_CACHE_CAPACITY),
        }
    }

    fn clear_buffer(&mut self) {
        self.buf_off = 0;
        self.buf_len = 0;
    }
}

struct HttpRangeReader {
    client: reqwest::blocking::Client,
    url: String,
    size: i64,
    prefetch: StdMutex<PrefetchState>,
    inflight: InflightGate,
    metrics: HttpMetrics,
}

impl HttpRangeReader {
    fn parse_content_range(s: &str) -> Option<(i64, i64, i64)> {
        // Expected: "bytes START-END/TOTAL"
        let s = s.trim();
        let rest = s.strip_prefix("bytes ")?;
        let (range_part, total_part) = rest.split_once('/')?;
        let (start_part, end_part) = range_part.split_once('-')?;
        let start = start_part.trim().parse::<i64>().ok()?;
        let end = end_part.trim().parse::<i64>().ok()?;
        let total = total_part.trim().parse::<i64>().ok()?;
        if start < 0 || end < start || total <= 0 {
            return None;
        }
        Some((start, end, total))
    }

    fn send_with_retry<F>(&self, mut build: F) -> Result<reqwest::blocking::Response>
    where
        F: FnMut() -> reqwest::blocking::RequestBuilder,
    {
        let mut last_err: Option<reqwest::Error> = None;
        for attempt in 0..=HTTP_RANGE_SEND_MAX_RETRIES {
            if attempt > 0 {
                self.metrics.send_retry.fetch_add(1, Ordering::Relaxed);
                let backoff_ms = HTTP_RANGE_SEND_RETRY_BASE_DELAY_MS * (1u64 << (attempt - 1));
                let jitter = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.subsec_millis() % 100) as u64)
                    .unwrap_or(0);
                thread::sleep(Duration::from_millis(backoff_ms + jitter));
            }
            match build().send() {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if e.is_timeout() {
                        self.metrics.timeout_error.fetch_add(1, Ordering::Relaxed);
                    }
                    if e.is_connect() {
                        self.metrics.connect_error.fetch_add(1, Ordering::Relaxed);
                    }
                    warn!(
                        attempt,
                        retries = HTTP_RANGE_SEND_MAX_RETRIES,
                        is_timeout = e.is_timeout(),
                        is_connect = e.is_connect(),
                        error = %e,
                        "http send failed, retrying if attempts remain"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err
            .map(|e| anyhow::Error::from(e))
            .unwrap_or_else(|| anyhow!("http send failed")))
    }

    fn fetch_range_once(&self, req_off: i64, req_end: i64, out: &mut [u8]) -> Result<usize> {
        let req_id = HTTP_RANGE_REQUEST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let _permit = InflightPermit::new(&self.inflight);
        let mut resp = self.send_with_retry(|| {
            self.client
                .get(&self.url)
                .header(reqwest::header::USER_AGENT, HTTP_RANGE_USER_AGENT)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(reqwest::header::RANGE, format!("bytes={}-{}", req_off, req_end))
        })?;

        let status = resp.status();
        let content_length =
            resp.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok());
        let etag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok());
        let cr_raw =
            resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok());
        let cr = cr_raw.and_then(Self::parse_content_range);
        let acceptable = status == reqwest::StatusCode::PARTIAL_CONTENT || cr.is_some();
        if !acceptable || cr.is_none() {
            let err = anyhow!(
                "range GET: HTTP {} (need 206 Partial Content, or 2xx with Content-Range); server may not support Range",
                status
            );
            error!(
                req_off,
                req_end,
                %status,
                content_length,
                etag,
                content_range = cr_raw,
                url = %self.url,
                "{}",
                err
            );
            return Err(err);
        }
        let (got_start, got_end, _total) = cr.expect("checked is_some above");
        if got_start != req_off || got_end < got_start {
            self.metrics.range_mismatch.fetch_add(1, Ordering::Relaxed);
            let err = anyhow!(
                "range GET: mismatched Content-Range {}-{} for requested {}-{}",
                got_start,
                got_end,
                req_off,
                req_end
            );
            error!(
                req_off,
                req_end,
                %status,
                content_length,
                etag,
                content_range = cr_raw,
                url = %self.url,
                "{}",
                err
            );
            return Err(err);
        }
        let expected = (got_end - got_start + 1) as usize;
        if expected == 0 {
            let err = anyhow!("range GET: zero-length Content-Range");
            error!(
                req_off,
                req_end,
                %status,
                content_length,
                etag,
                content_range = cr_raw,
                url = %self.url,
                "{}",
                err
            );
            return Err(err);
        }
        if expected > out.len() {
            return Err(anyhow!(
                "range response larger than output buffer: expected={} out={}",
                expected,
                out.len()
            ));
        }

        let mut read = 0usize;
        while read < expected {
            let n = resp
                .read(&mut out[read..expected])
                .map_err(|e| anyhow!("failed to read response body: {e}"))?;
            if n == 0 {
                break;
            }
            read += n;
        }
        if read != expected {
            self.metrics.short_body_error.fetch_add(1, Ordering::Relaxed);
            return Err(anyhow!(
                "short HTTP range body: expected {} bytes, got {} (off={}, end={})",
                expected,
                read,
                req_off,
                req_end
            ));
        }

        let mut probe = [0u8; 1];
        match resp.read(&mut probe) {
            Ok(0) => {}
            Ok(_) => return Err(anyhow!("range response too long: got more than expected bytes")),
            Err(e) => return Err(anyhow!("failed to drain response: {e}")),
        }
        if req_id % 256 == 0 {
            debug!(req_id, req_off, req_end, expected, "http range fetch sample");
        }
        Ok(expected)
    }

    fn try_read_block_cache(&self, off: i64, want: usize, buf: &mut [u8]) -> Option<usize> {
        if want == 0 {
            return Some(0);
        }
        let block_size = HTTP_BLOCK_SIZE as i64;
        let start_block = off.div_euclid(block_size);
        let end_block = (off + want as i64 - 1).div_euclid(block_size);
        let mut copied = 0usize;
        let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
        for block_id in start_block..=end_block {
            let Some(block_data) = prefetch.cache.map.get(&block_id).cloned() else {
                self.metrics.cache_miss.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            prefetch.cache.touch(block_id);
            let block_start = block_id * block_size;
            let offset_in_block = (off + copied as i64 - block_start).max(0) as usize;
            let remaining = want - copied;
            let n = remaining.min(block_data.len().saturating_sub(offset_in_block));
            if n == 0 {
                self.metrics.cache_miss.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            buf[copied..copied + n]
                .copy_from_slice(&block_data[offset_in_block..offset_in_block + n]);
            copied += n;
            if copied == want {
                self.metrics.cache_hit.fetch_add(1, Ordering::Relaxed);
                return Some(copied);
            }
        }
        None
    }

    fn store_into_block_cache(&self, off: i64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let block_size = HTTP_BLOCK_SIZE as i64;
        let mut cursor = 0usize;
        let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
        while cursor < data.len() {
            let global_off = off + cursor as i64;
            let block_id = global_off.div_euclid(block_size);
            let offset_in_block = (global_off - block_id * block_size) as usize;
            let writable = (HTTP_BLOCK_SIZE - offset_in_block).min(data.len() - cursor);
            let mut block_data = prefetch
                .cache
                .map
                .remove(&block_id)
                .unwrap_or_else(|| vec![0u8; HTTP_BLOCK_SIZE]);
            block_data[offset_in_block..offset_in_block + writable]
                .copy_from_slice(&data[cursor..cursor + writable]);
            prefetch.cache.put(block_id, block_data);
            cursor += writable;
        }
    }

    fn begin_read_with_prefetch_state(&self, off: i64, want: usize) -> bool {
        let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
        let end = off + want as i64;
        let is_sequential = prefetch
            .last_off
            .map(|last_off| last_off + prefetch.last_len as i64 == off)
            .unwrap_or(false);
        let in_cache = prefetch.buf_len > 0
            && off >= prefetch.buf_off
            && end <= (prefetch.buf_off + prefetch.buf_len as i64);
        let overlaps_cache = prefetch.buf_len > 0
            && off < (prefetch.buf_off + prefetch.buf_len as i64)
            && end > prefetch.buf_off;
        if is_sequential {
            prefetch.seq_reads = prefetch.seq_reads.saturating_add(1);
        } else {
            if prefetch.enabled && !in_cache {
                if overlaps_cache {
                    debug!(
                        off,
                        want,
                        cache_off = prefetch.buf_off,
                        cache_len = prefetch.buf_len,
                        "http non-sequential read partially overlaps prefetch cache; disabling prefetch"
                    );
                }
                prefetch.enabled = false;
                prefetch.clear_buffer();
                prefetch.seq_reads = 1;
            } else if prefetch.enabled && in_cache {
                prefetch.seq_reads = 0;
            } else {
                prefetch.seq_reads = 1;
            }
        }
        if !prefetch.enabled && prefetch.seq_reads >= HTTP_PREFETCH_ENABLE_SEQ_READS {
            prefetch.enabled = true;
            prefetch.clear_buffer();
        }
        prefetch.enabled
    }

    fn read_at_with_prefetch(&self, off: i64, want: usize, buf: &mut [u8]) -> Result<Option<usize>> {
        if want > HTTP_PREFETCH_CHUNK_SIZE {
            return Ok(None);
        }

        let mut detached_buf: Option<Vec<u8>> = None;
        let mut fetch_len = 0usize;
        {
            let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
            if !prefetch.enabled {
                return Ok(None);
            }

            let in_cache = prefetch.buf_len > 0
                && off >= prefetch.buf_off
                && (off + want as i64) <= (prefetch.buf_off + prefetch.buf_len as i64);
            if in_cache {
                let start = (off - prefetch.buf_off) as usize;
                let available = prefetch.buf_len.saturating_sub(start);
                let n = available.min(want);
                if n == 0 {
                    prefetch.last_off = Some(off);
                    prefetch.last_len = 0;
                    return Ok(Some(0));
                }
                buf[..n].copy_from_slice(&prefetch.buf[start..start + n]);
                prefetch.last_off = Some(off);
                prefetch.last_len = n;
                return Ok(Some(n));
            }

            let remain = (self.size - off).max(0) as usize;
            if remain == 0 {
                prefetch.last_off = Some(off);
                prefetch.last_len = 0;
                return Ok(Some(0));
            }

            fetch_len = remain.min(HTTP_PREFETCH_CHUNK_SIZE);
            let mut local = std::mem::take(&mut prefetch.buf);
            if local.len() < HTTP_PREFETCH_CHUNK_SIZE {
                local.resize(HTTP_PREFETCH_CHUNK_SIZE, 0);
            }
            prefetch.clear_buffer();
            detached_buf = Some(local);
        }

        let mut local_buf = detached_buf.expect("local prefetch buffer should exist");
        let fetch_end = off + fetch_len as i64 - 1;
        let fetched = match self.fetch_range_once(off, fetch_end, &mut local_buf[..fetch_len]) {
            Ok(n) => n,
            Err(e) => {
                let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
                prefetch.buf = local_buf;
                prefetch.clear_buffer();
                return Err(e);
            }
        };

        let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
        prefetch.buf = local_buf;
        if !prefetch.enabled {
            let n = fetched.min(want);
            if n == 0 {
                prefetch.clear_buffer();
                prefetch.last_off = Some(off);
                prefetch.last_len = 0;
                return Ok(Some(0));
            }
            buf[..n].copy_from_slice(&prefetch.buf[..n]);
            prefetch.clear_buffer();
            prefetch.last_off = Some(off);
            prefetch.last_len = n;
            return Ok(Some(n));
        }
        prefetch.buf_off = off;
        prefetch.buf_len = fetched;
        let n = fetched.min(want);
        if n == 0 {
            prefetch.last_off = Some(off);
            prefetch.last_len = 0;
            return Ok(Some(0));
        }
        buf[..n].copy_from_slice(&prefetch.buf[..n]);
        prefetch.last_off = Some(off);
        prefetch.last_len = n;
        Ok(Some(n))
    }

    fn read_at_direct(&self, off: i64, end: i64, want: usize, buf: &mut [u8]) -> Result<usize> {
        let mut total = 0usize;
        let mut reissue = 0u32;

        while total < want {
            let req_off = off + total as i64;
            let req_end = end;
            let expected = self.fetch_range_once(req_off, req_end, &mut buf[total..want])?;
            let start_idx = total;
            total += expected;

            if expected < (want - start_idx) {
                reissue += 1;
                warn!(
                    off,
                    len = want,
                    req_off,
                    req_end,
                    got = expected,
                    expected = want - start_idx,
                    reissue,
                    url = %self.url,
                    "short HTTP range body; reissuing remaining range"
                );
                if reissue > HTTP_RANGE_READ_REISSUE_MAX {
                    return Err(anyhow!(
                        "short HTTP range body after {} reissues (off={}, len={}, got={})",
                        HTTP_RANGE_READ_REISSUE_MAX,
                        off,
                        want,
                        total
                    ));
                }
            }
        }
        Ok(total)
    }

    fn record_last_read(&self, off: i64, total: usize) {
        let mut prefetch = self.prefetch.lock().expect("prefetch mutex poisoned");
        prefetch.last_off = Some(off);
        prefetch.last_len = total;
    }

    fn new(url: &str) -> Result<Self> {
        // Accept http and https
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(anyhow!("unsupported url scheme"));
        }
        let connect_timeout_secs = read_env_u64("ARKKVM_HTTP_CONNECT_TIMEOUT_SECS", 20, 3, 300);
        let request_timeout_secs = read_env_u64("ARKKVM_HTTP_REQUEST_TIMEOUT_SECS", 45, 5, 600);
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout_secs))
            .timeout(Duration::from_secs(request_timeout_secs))
            .build()
            .context("failed to build http client")?;

        let mut reader = Self {
            client,
            url: url.to_string(),
            size: 0,
            prefetch: StdMutex::new(PrefetchState::new()),
            inflight: InflightGate::new(HTTP_INFLIGHT_MAX),
            metrics: HttpMetrics::default(),
        };

        let mut effective_url = url.to_string();
        if let Ok(resp) = reader.send_with_retry(|| {
            reader
                .client
                .head(url)
                .header(reqwest::header::USER_AGENT, HTTP_RANGE_USER_AGENT)
        }) {
            effective_url = resp.url().as_str().to_owned();
        }

        let resp = reader
            .send_with_retry(|| {
                reader
                    .client
                .get(effective_url.as_str())
                .header(reqwest::header::USER_AGENT, HTTP_RANGE_USER_AGENT)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(reqwest::header::RANGE, "bytes=0-0")
            })
            .context("range probe GET failed")?;
        effective_url = resp.url().as_str().to_owned();

        let status = resp.status();
        let has_cr = resp.headers().get(reqwest::header::CONTENT_RANGE).is_some();
        let acceptable = status == reqwest::StatusCode::PARTIAL_CONTENT
            || (status.is_success() && has_cr);
        if !acceptable {
            return Err(anyhow!(
                "range probe: HTTP {} (need 206 or 2xx with Content-Range); byte ranges required for NBD",
                status
            ));
        }

        let size = if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE)
            && let Ok(s) = cr.to_str()
            && let Some((start, end, total)) = Self::parse_content_range(s)
            && start == 0
            && end == 0
        {
            total
        } else {
            return Err(anyhow!(
                "range probe: missing or invalid Content-Range header (got HTTP {})",
                status
            ));
        };

        let _ = resp.bytes().context("range probe: read body")?;
        reader.url = effective_url;
        reader.size = size;
        Ok(reader)
    }

    fn url(&self) -> &str {
        &self.url
    }
}

impl RemoteImageReader for HttpRangeReader {
    fn read_at(&self, off: i64, buf: &mut [u8]) -> Result<usize> {
        if off < 0 {
            return Err(anyhow!("invalid range"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len();
        let end = off + want as i64 - 1;
        if let Some(n) = self.try_read_block_cache(off, want, buf) {
            return Ok(n);
        }
        self.begin_read_with_prefetch_state(off, want);

        if let Some(n) = self.read_at_with_prefetch(off, want, buf)? {
            self.store_into_block_cache(off, &buf[..n]);
            return Ok(n);
        }

        let total = self.read_at_direct(off, end, want, buf)?;
        self.store_into_block_cache(off, &buf[..total]);
        self.record_last_read(off, total);
        let seq = HTTP_RANGE_REQUEST_SEQ.load(Ordering::Relaxed);
        if seq > 0 && seq % 512 == 0 {
            info!(
                seq,
                cache_hit = self.metrics.cache_hit.load(Ordering::Relaxed),
                cache_miss = self.metrics.cache_miss.load(Ordering::Relaxed),
                send_retry = self.metrics.send_retry.load(Ordering::Relaxed),
                timeout_error = self.metrics.timeout_error.load(Ordering::Relaxed),
                connect_error = self.metrics.connect_error.load(Ordering::Relaxed),
                short_body_error = self.metrics.short_body_error.load(Ordering::Relaxed),
                range_mismatch = self.metrics.range_mismatch.load(Ordering::Relaxed),
                "http range reader metrics snapshot"
            );
        }
        Ok(total)
    }
    fn size(&self) -> Result<i64> {
        Ok(self.size)
    }
}

// Public helper to probe remote HTTP/HTTPS URL usability and size
#[derive(Debug, Clone, Serialize)]
pub struct UrlCheckResult {
    #[serde(rename = "Usable")]
    pub usable: bool,
    #[serde(rename = "Reason", skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(rename = "Size")]
    pub size: i64,
}

pub fn check_mount_url(url: &str) -> Result<UrlCheckResult> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(UrlCheckResult {
            usable: false,
            reason: Some("unsupported scheme".into()),
            size: 0,
        });
    }

    match HttpRangeReader::new(url) {
        Ok(r) => {
            let size = r.size().unwrap_or(0);
            Ok(UrlCheckResult {
                usable: true,
                reason: None,
                size,
            })
        }
        Err(e) => Ok(UrlCheckResult {
            usable: false,
            reason: Some(e.to_string()),
            size: 0,
        }),
    }
}

// ==========================
// WebRTC reader bridge
// ==========================

/// External handler for WebRTC disk reads. Must be installed by the WebRTC module.
pub trait WebRtcReadHandler: Send + Sync {
    fn read(&self, offset: i64, size: i64) -> Result<Vec<u8>>;
}

static WEBRTC_READ_HANDLER: RwLock<Option<Arc<dyn WebRtcReadHandler>>> = RwLock::new(None);

/// Install WebRTC read handler. Should be called when the "disk" data channel is available.
pub fn set_webrtc_read_handler(handler: Option<Arc<dyn WebRtcReadHandler>>) {
    *WEBRTC_READ_HANDLER.write() = handler;
}

struct WebRtcDiskReader {
    size: i64,
}

impl WebRtcDiskReader {
    fn new(size: i64) -> Self {
        Self { size }
    }
}

impl RemoteImageReader for WebRtcDiskReader {
    fn read_at(&self, off: i64, buf: &mut [u8]) -> Result<usize> {
        let handler =
            WEBRTC_READ_HANDLER.read().clone().ok_or_else(|| anyhow!("not active session"))?;
        if off < 0 {
            return Err(anyhow!("invalid range"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let end = (off + buf.len() as i64).min(self.size);
        let req_len = (end - off).max(0);
        if req_len == 0 {
            return Ok(0);
        }
        let data = handler.read(off, req_len)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
    fn size(&self) -> Result<i64> {
        Ok(self.size)
    }
}

// ============
// Path utils
// ============

trait CleanPath {
    fn clean(&self) -> PathBuf;
}
impl CleanPath for Path {
    fn clean(&self) -> PathBuf {
        // Simplified clean: just canonicalize components without following symlinks
        let s = self.to_string_lossy();
        let mut parts = Vec::new();
        for p in s.split('/') {
            if p.is_empty() || p == "." {
                continue;
            }
            if p == ".." {
                let _ = parts.pop();
                continue;
            }
            parts.push(p);
        }
        let mut out = String::new();
        for p in parts {
            out.push('/');
            out.push_str(p);
        }
        if out.is_empty() {
            out.push('.');
        }
        PathBuf::from(out)
    }
}

// ==========================
// Upload management (shared by RPC and HTTP endpoints)
// ==========================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFileUpload {
    #[serde(rename = "alreadyUploadedBytes")]
    pub already_uploaded_bytes: i64,
    #[serde(rename = "dataChannel")]
    pub data_channel: String,
}

struct PendingUpload {
    file: File,
    size: i64,
    already_uploaded_bytes: i64,
    upload_path: PathBuf,
    final_path: PathBuf,
}

struct FtPendingUpload {
    writer: Option<AsyncFileWriter>,
    size: i64,
    already_uploaded_bytes: i64,
    upload_path: String,
    final_path: String,
}

static PENDING_UPLOADS: Lazy<AsyncMutex<HashMap<String, PendingUpload>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));

static FT_PENDING_UPLOADS: Lazy<AsyncMutex<HashMap<String, FtPendingUpload>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));
static FT_PENDING_UPLOADS_CLOSED: Lazy<AsyncMutex<HashMap<String, FtPendingUpload>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));

pub async fn start_storage_file_upload(filename: &str, size: i64) -> Result<StorageFileUpload> {
    let filename = sanitize_filename(filename)?;
    ensure_images_folder().await?;

    let final_path = Path::new(IMAGES_FOLDER).join(&filename);
    let mut upload_path = final_path.clone();
    let base = final_path.file_name().ok_or_else(|| anyhow!("invalid filename"))?;
    let mut appended = std::ffi::OsString::from(base);
    appended.push(".incomplete");
    upload_path.set_file_name(appended);

    if final_path.exists() {
        return Err(anyhow!("file already exists: {}", filename));
    }

    let mut already_uploaded_bytes: i64 = 0;
    if let Ok(meta) = fs::metadata(&upload_path).await {
        already_uploaded_bytes = meta.len() as i64;
    }

    let file =
        OpenOptions::new().append(true).create(true).open(&upload_path).await.with_context(
            || format!("failed to open file for upload: {}", upload_path.display()),
        )?;

    let upload_id = format!("upload_{}", Uuid::new_v4());
    {
        let mut map = PENDING_UPLOADS.lock().await;
        map.insert(
            upload_id.clone(),
            PendingUpload { file, size, already_uploaded_bytes, upload_path, final_path },
        );
    }

    Ok(StorageFileUpload { already_uploaded_bytes, data_channel: upload_id })
}

pub async fn append_upload_data(upload_id: &str, data: &[u8]) -> Result<i64> {
    let mut entry = {
        let mut map = PENDING_UPLOADS.lock().await;
        map.remove(upload_id)
    };
    let Some(mut entry) = entry.take() else {
        return Err(anyhow!("upload not found"));
    };
    let write_res = entry
        .file
        .write_all(data)
        .await
        .with_context(|| format!("failed to write upload: {}", upload_id));
    if let Err(e) = write_res {
        let mut map = PENDING_UPLOADS.lock().await;
        map.insert(upload_id.to_owned(), entry);
        return Err(e);
    }
    entry.already_uploaded_bytes += data.len() as i64;
    let current = entry.already_uploaded_bytes;
    let mut map = PENDING_UPLOADS.lock().await;
    map.insert(upload_id.to_owned(), entry);
    Ok(current)
}

pub async fn complete_upload(upload_id: &str) -> Result<()> {
    let entry = {
        let mut map = PENDING_UPLOADS.lock().await;
        map.remove(upload_id)
    };
    let Some(mut entry) = entry else {
        return Err(anyhow!("upload not found"));
    };
    entry.file.flush().await.ok();
    entry.file.sync_data().await.ok();
    drop(entry.file);
    if entry.already_uploaded_bytes == entry.size {
        fs::rename(&entry.upload_path, &entry.final_path).await.with_context(|| {
            format!(
                "failed to rename uploaded file: {} -> {}",
                entry.upload_path.display(),
                entry.final_path.display()
            )
        })?;
    }
    Ok(())
}

pub async fn get_upload_progress(upload_id: &str) -> Result<(i64, i64)> {
    let map = PENDING_UPLOADS.lock().await;
    let entry = map.get(upload_id).ok_or_else(|| anyhow!("upload not found"))?;
    Ok((entry.size, entry.already_uploaded_bytes))
}

pub async fn start_ft_file_upload(
    path: String,
    name: String,
    size: i64,
) -> Result<StorageFileUpload> {
    let remote_fs = REMOTE_FS.read().await;

    let file_path = format!("{}/{}", &path, &name).replace("//", "/");
    let file_cache_path = format!("{}/{}.ftcache", &path, &name).replace("//", "/");

    if remote_fs.exists(file_path.as_str()).await? {
        return Err(anyhow!("File already exists"));
    }

    let task = if remote_fs.exists(file_cache_path.as_str()).await? {
        let cache_size = remote_fs.file_size(file_cache_path.as_str()).await?;
        let mut should_delete_cache = false;
        let mut map = FT_PENDING_UPLOADS_CLOSED.lock().await;
        let mut old_upload_id = map.iter().find_map(|(upload_id, item)| {
            if item.final_path == file_path && item.upload_path == file_cache_path && item.already_uploaded_bytes == cache_size as i64 {
                Some(upload_id.clone())
            }
            else {
                None
            }
        });

        let task = if let Some(upload_id) = old_upload_id.take() {
            let entry = map.remove(&upload_id);
            (Some(upload_id.clone()), entry)
        }
        else {
            should_delete_cache = true;
            (None, None)
        };
        drop(map);
        if should_delete_cache {
            remote_fs.delete_file(file_cache_path.as_str()).await?;
        }
        task
    }
    else {
        (None, None)
    };

    let (upload_id, entry) = if task.0.is_some() && task.1.is_some() {
        let mut pending_upload = task.1.unwrap();
        pending_upload.writer = Some(remote_fs
            .create_async_writer(
                pending_upload.upload_path.as_str(),
                1024 * 4,
                pending_upload.already_uploaded_bytes as u64,
            )
            .await?);
        
        (task.0.unwrap(), pending_upload)
    }
    else {
        remote_fs.create_empty_file(file_cache_path.as_str()).await?;
        (format!("ft_upload_{}", Uuid::new_v4()), FtPendingUpload {
            writer: Some(remote_fs
                .create_async_writer(
                    file_cache_path.as_str(),
                    1024 * 4,
                    0,
                )
                .await?),
            size,
            already_uploaded_bytes: 0,
            upload_path: file_cache_path.clone(),
            final_path: file_path.clone(),
        })
    };

    let already_uploaded_bytes = entry.already_uploaded_bytes;
    
    {
        let mut map = FT_PENDING_UPLOADS.lock().await;
        map.insert(
            upload_id.clone(),
            entry,
        );
    }

    info!(
        "Starting upload of {} bytes to {}, cache_path: {}, channel: {}, already_uploaded_bytes: {}",
        size, &file_path, &file_cache_path, &upload_id, already_uploaded_bytes
    );

    Ok(StorageFileUpload {
        already_uploaded_bytes: already_uploaded_bytes,
        data_channel: upload_id,
    })
}

pub async fn append_ft_upload_data(upload_id: &str, data: &[u8]) -> Result<i64> {
    let mut entry = {
        let mut map = FT_PENDING_UPLOADS.lock().await;
        map.remove(upload_id)
    };
    let Some(mut entry) = entry.take() else {
        return Err(anyhow!("upload not found"));
    };

    let Some(writer) = entry.writer.as_mut() else {
        let mut map = FT_PENDING_UPLOADS.lock().await;
        map.insert(upload_id.to_owned(), entry);
        return Err(anyhow!("writer not found"));
    };

    let size = match writer.write_chunk(data).await {
        Ok(n) => n,
        Err(e) => {
            let mut map = FT_PENDING_UPLOADS.lock().await;
            map.insert(upload_id.to_owned(), entry);
            return Err(e);
        }
    };
    if size != data.len() {
        let mut map = FT_PENDING_UPLOADS.lock().await;
        map.insert(upload_id.to_owned(), entry);
        return Err(anyhow!("Failed to write all data in file"));
    }
    entry.already_uploaded_bytes += data.len() as i64;
    let current = entry.already_uploaded_bytes;
    let mut map = FT_PENDING_UPLOADS.lock().await;
    map.insert(upload_id.to_owned(), entry);
    Ok(current)
}

pub async fn complete_ft_upload(upload_id: &str) -> Result<()> {
    let entry = {
        let mut map = FT_PENDING_UPLOADS.lock().await;
        map.remove(upload_id)
    };
    let Some(mut entry) = entry else {
        return Err(anyhow!("upload not found"));
    };

    if let Some(writer) = entry.writer.as_mut() {
        writer.flush().await?;
        if let Err(e) = writer.shutdown().await {
            warn!("error shutting down writer: {:?}", e);
        }
    }
    entry.writer = None;

    let upload_path = entry.upload_path.clone();
    let final_path = entry.final_path.clone();
    let finish = entry.already_uploaded_bytes == entry.size;

    if finish {
        let remote_fs = REMOTE_FS.read().await;
        remote_fs.rename(upload_path.as_str(), final_path.as_str()).await?;
    }
    else {
        let mut map = FT_PENDING_UPLOADS_CLOSED.lock().await;
        map.insert(upload_id.to_owned(), entry);
    }
    Ok(())
}

pub async fn get_ft_upload_progress(upload_id: &str) -> Result<(i64, i64)> {
    let map = FT_PENDING_UPLOADS.lock().await;
    let entry = map.get(upload_id).ok_or_else(|| anyhow!("upload not found"))?;
    Ok((entry.size, entry.already_uploaded_bytes))
}

// ==========================
// Storage directory utilities
// ==========================

#[derive(Debug, Clone, Serialize)]
pub struct StorageFileEntry {
    pub filename: String,
    pub size: i64,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "totalBytes")]
    pub total_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageFilesList {
    pub files: Vec<StorageFileEntry>,
}

pub async fn list_storage_files() -> Result<StorageFilesList> {
    ensure_images_folder().await?;

    let mut pending_totals: HashMap<String, i64> = HashMap::new();
    {
        let map = PENDING_UPLOADS.lock().await;
        for (_id, pu) in map.iter() {
            if let Some(name) = pu.upload_path.file_name().and_then(|s| s.to_str()) {
                pending_totals.insert(name.to_string(), pu.size);
            }
        }
    }

    let mut out = Vec::new();
    let mut entries = fs::read_dir(IMAGES_FOLDER).await.context("failed to read images folder")?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        if meta.is_file() {
            let name = entry.file_name();
            let filename = name.to_string_lossy().to_string();
            let size = meta.len() as i64;
            let created_at = meta
                .modified()
                .ok()
                .map(|t| {
                    chrono::DateTime::<chrono::Utc>::from(t)
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                })
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
            let total_bytes = if filename.ends_with(".incomplete") {
                pending_totals.get(&filename).copied().unwrap_or(size)
            } else {
                size
            };
            out.push(StorageFileEntry { filename, size, created_at, total_bytes });
        }
    }
    Ok(StorageFilesList { files: out })
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageSpace {
    #[serde(rename = "bytesUsed")]
    pub bytes_used: i64,
    #[serde(rename = "bytesFree")]
    pub bytes_free: i64,
}

pub async fn get_storage_space() -> Result<StorageSpace> {
    // Get IMG_FILE_NAME file size to subtract from storage space
    let (img_file_size, img_file_used) = get_img_file_size().await.unwrap_or((0, 0));
    tokio::task::spawn_blocking(move || {
        let path = Path::new(IMAGES_FOLDER);
        let st = rustix::fs::statfs(path).context("failed to statfs images folder")?;
        let total = st.f_blocks * st.f_bsize as u64;
        let used = (st.f_blocks - st.f_bfree) * st.f_bsize as u64;
        // let free = (st.f_bfree as u128 * st.f_bsize as u128) as i64;
        // let used = total.saturating_sub(free);

        // Subtract IMG_FILE_NAME file size from total
        // This prevents users from being misled by the space occupied by the fs functionality
        let adjusted_total = (total as i64).saturating_sub(img_file_size as i64).max(0);
        let adjusted_used = (used as i64).saturating_sub(img_file_used as i64).max(0);
        let adjusted_free = adjusted_total.saturating_sub(adjusted_used);
        
        Ok(StorageSpace { 
            bytes_used: adjusted_used, 
            bytes_free: adjusted_free,
        })
    })
    .await
    .context("spawn_blocking failed")?
}

pub async fn delete_storage_file(filename: &str) -> Result<()> {
    let filename = sanitize_filename(filename)?;
    let full_path = Path::new(IMAGES_FOLDER).join(&filename);
    if !full_path.exists() {
        return Err(anyhow!("file does not exist: {}", filename));
    }
    fs::remove_file(&full_path)
        .await
        .with_context(|| format!("failed to delete file: {}", filename))?;
    Ok(())
}

pub async fn mount_built_in_image(filename: &str) -> Result<()> {
    // If file exists in images folder, mount it directly
    let name = sanitize_filename(filename)?;
    ensure_images_folder().await?;
    let image_path = Path::new(IMAGES_FOLDER).join(&name);
    if image_path.exists() {
        return mount_with_storage(&name, VirtualMediaMode::Disk).await;
    }

    // Try to load from embedded assets and write out
    if let Some(data) = BuiltinImages::get(&name) {
        let bytes = data.data.as_ref();
        let mut file = fs::File::create(&image_path)
            .await
            .with_context(|| format!("failed to create file: {}", image_path.display()))?;

        let mut writer = BufWriter::new(&mut file);
        writer
            .write_all(bytes)
            .await
            .with_context(|| format!("failed to write embedded image: {}", image_path.display()))?;
        writer.flush().await.ok();
        drop(writer);

        return mount_with_storage(&name, VirtualMediaMode::Disk).await;
    }

    Err(anyhow!("image not found in built-in resources: {}", name))
}

// =================================== File Tranfser Begin =====================================

pub async fn load_with_file_img() -> Result<()> {
    info!("File Transfer mount with kvm");
    match get_file_transfer_state().await?.target {
        FileTransferTarget::None => {} // do nothing
        FileTransferTarget::Kvm => return Ok(()),
        FileTransferTarget::RemoteUsb => unmount_file_img().await?,
    }

    let mut remote_fs = REMOTE_FS.write().await;
    if let Err(e) = remote_fs.mount().await {
        return Err(anyhow!("Failed to mount file system: {:?}", e));
    }

    let config = get_config_manager();
    let _ = config.set_ft_mount_target(FileTransferTarget::Kvm).await?;
    info!("File Transfer mount Target Set To KVM");
    Ok(())
}

pub async fn repair_file_transfer() -> Result<()> {
    let mut remote_fs = REMOTE_FS.write().await;
    remote_fs.repair_filesystem().await
}

pub async fn format_file_transfer() -> Result<()> {
    let mut remote_fs = REMOTE_FS.write().await;
    remote_fs.format_filesystem().await
}

pub async fn unload_with_file_img() -> Result<()> {
    info!("File Tranfser Unloading with kvm");
    let mut drained = Vec::new();
    {
        let mut process = FT_PENDING_UPLOADS.lock().await;
        drained.extend(process.drain().map(|(_, item)| item));
    }
    for item in &mut drained {
        if let Some(writer) = item.writer.as_mut() {
            let _ = writer.shutdown().await;
        }
    }

    let mut remote_fs = REMOTE_FS.write().await;
    if let Err(e) = remote_fs.unmount().await {
        return Err(anyhow!("File Tranfser Unloading unmount fatfs error: {:?}", e));
    }

    info!("File Tranfser Unmounted FAT32 Filesystem");
    Ok(())
}

pub async fn list_with_file_img(path: String) -> Result<Vec<FileInfo>> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.list_directory(path.as_str()).await
}

pub async fn fs_info_with_file_img() -> Result<FileSystemInfo> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.get_filesystem_usage().await
}

pub async fn fs_del_file_with_file_img(path: String) -> Result<()> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.delete_file(path.as_str()).await
}

pub async fn fs_del_dir_with_file_img(path: String) -> Result<()> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.delete_directory(path.as_str()).await
}

pub async fn fs_create_dir_with_file_img(path: String) -> Result<()> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.create_directory(path.as_str()).await
}

pub async fn fs_create_empty_file_with_file_img(path: String) -> Result<()> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.create_empty_file(&path).await
}

pub async fn fs_download_file(file_path: &str) -> Result<AsyncFileReader> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.create_async_reader(file_path, 1024 * 4).await
}

pub async fn fs_file_exists(file_path: &str) -> Result<bool> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.exists(file_path).await
}

pub async fn fs_get_file_info(file_path: &str) -> Result<FileInfo> {
    let remote_fs = REMOTE_FS.read().await;
    remote_fs.get_file_info(file_path).await
}

// =================================== File Tranfser End =====================================
