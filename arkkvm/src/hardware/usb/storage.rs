use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use once_cell::sync::Lazy;
// use parking_lot::{Mutex, RwLock};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
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

    if let Some(path) = get_mass_storage_image(UsbTarget::Usb1Lun0).await? {
        if path == format!("{}/{}", IMG_FILE_PATH, IMG_FILE_NAME) {
            return Ok(FileTransferState { target: FileTransferTarget::RemoteUsb });
        }
    }
    Ok(FileTransferState { target: FileTransferTarget::None })
}

/// Public API: set initial state by reading configfs and current file.
pub async fn set_initial_virtual_media_state() -> Result<()> {
    let cdrom_enabled = get_mass_storage_cdrom_enabled(UsbTarget::Usb0Lun0)
        .await
        .context("failed to read mass storage cdrom")?;
    let disk_path = get_mass_storage_image(UsbTarget::Usb0Lun0)
        .await
        .context("failed to get mass storage image")?;

    let mut initial = VirtualMediaState {
        source: VirtualMediaSource::Storage,
        mode: VirtualMediaMode::Disk,
        filename: None,
        url: None,
        size: 0,
    };
    if cdrom_enabled {
        initial.mode = VirtualMediaMode::CDROM;
    }

    let state_opt = match disk_path.as_deref() {
        None => None,
        Some("") => None,
        Some("/dev/nbd0") => {
            // Unknown remote; placeholder for legacy state
            initial.source = VirtualMediaSource::HTTP;
            initial.url = Some("/".to_string());
            initial.size = 1;
            Some(initial)
        }
        Some(path) => {
            let filename =
                Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let meta =
                fs::metadata(path).await.context("failed to stat mass storage image file")?;
            initial.filename = Some(filename);
            initial.size = meta.len() as i64;
            Some(initial)
        }
    };

    *CURRENT_VIRTUAL_MEDIA_STATE.write() = state_opt.clone();
    info!(?state_opt, "initial virtual media state set");
    Ok(())
}

/// Mount remote HTTP image via NBD and point mass storage to /dev/nbd0.
pub async fn mount_with_http(url: &str, mode: VirtualMediaMode) -> Result<()> {
    ensure_images_folder().await?;
    set_mass_storage_mode(mode == VirtualMediaMode::CDROM, UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage mode")?;

    // Create HTTP/HTTPS reader and determine size
    let reader = HttpRangeReader::new(url).context("failed to init HTTP reader")?;
    let size = reader.size().context("failed to get HTTP size")?;
    info!(url, size, "using remote HTTP url");

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

    // TODO: replace with ready polling if needed
    sleep(Duration::from_secs(1)).await;
    set_mass_storage_image("/dev/nbd0", UsbTarget::Usb0Lun0)
        .await
        .context("failed to set mass storage image to nbd0")?;
    info!("usb mass storage mounted");
    Ok(())
}

/// Mount WebRTC provided image via NBD. Requires external read handler to be set.
pub async fn mount_with_webrtc(filename: &str, size: i64, mode: VirtualMediaMode) -> Result<()> {
    set_mass_storage_mode(mode == VirtualMediaMode::CDROM, UsbTarget::Usb0Lun0)
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
    sleep(Duration::from_secs(1)).await;
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

    set_mass_storage_mode(mode == VirtualMediaMode::CDROM, UsbTarget::Usb0Lun0)
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

    set_mass_storage_mode(false, UsbTarget::Usb1Lun0)
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
        FileTransferTarget::RemoteUsb => set_mass_storage_image("\n", UsbTarget::Usb1Lun0).await,
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
    set_mass_storage_image("\n", UsbTarget::Usb0Lun0)
        .await
        .unwrap_or_else(|e| warn!("failed to clear mass storage image: {}", e));
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

async fn get_mass_storage_lun0_by_target(usb_target: UsbTarget) -> Result<PathBuf> {
    Ok(match usb_target {
        UsbTarget::Usb0Lun0 => resolve_mass_storage_lun0().await?,
        UsbTarget::Usb1Lun0 => resolve_mass_storage_lun1().await?,
    })
}

/// Resolve configfs usb0 lun.0 directory for mass_storage.usb0.
async fn resolve_mass_storage_lun0() -> Result<PathBuf> {
    let gadgets_root = Path::new("/sys/kernel/config/usb_gadget");
    let mut entries = fs::read_dir(gadgets_root).await.context("failed to read usb_gadget root")?;
    while let Some(entry) = entries.next_entry().await? {
        let p = entry.path().join("functions").join("mass_storage.0").join("lun.0");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!("mass_storage.0/lun.0 not found under configfs"))
}

/// Resolve configfs usb1 lun.0 directory for mass_storage.usb0.
async fn resolve_mass_storage_lun1() -> Result<PathBuf> {
    let gadgets_root = Path::new("/sys/kernel/config/usb_gadget");
    let mut entries = fs::read_dir(gadgets_root).await.context("failed to read usb_gadget root")?;
    while let Some(entry) = entries.next_entry().await? {
        let p = entry.path().join("functions").join("mass_storage.1").join("lun.0");
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!("mass_storage.1/lun.0 not found under configfs"))
}

async fn read_trimmed(path: &Path) -> Result<String> {
    let s = fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(s.trim().to_string())
}

/// Get current mass storage image file path.
pub async fn get_mass_storage_image(usb_target: UsbTarget) -> Result<Option<String>> {
    let lun = get_mass_storage_lun0_by_target(usb_target).await?;
    let file_path = lun.join("file");
    let s = read_trimmed(&file_path).await?;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

/// Set mass storage image file path.
pub async fn set_mass_storage_image(image_path: &str, usb_target: UsbTarget) -> Result<()> {
    let lun = get_mass_storage_lun0_by_target(usb_target).await?;
    let file_path = lun.join("file");
    // Write empty then the path (twice)
    write_file(&file_path, "\n").await.ok();
    write_file(&file_path, image_path).await?;
    write_file(&file_path, image_path).await?;
    Ok(())
}

/// Set mass storage mode (cdrom on/off).
pub async fn set_mass_storage_mode(cdrom: bool, usb_target: UsbTarget) -> Result<()> {
    let lun = get_mass_storage_lun0_by_target(usb_target).await?;
    let cdrom_path = lun.join("cdrom");
    write_file(&cdrom_path, if cdrom { "1" } else { "0" }).await?;
    Ok(())
}

/// Get mass storage cdrom enabled flag.
pub async fn get_mass_storage_cdrom_enabled(usb_target: UsbTarget) -> Result<bool> {
    let lun = get_mass_storage_lun0_by_target(usb_target).await?;
    let cdrom_path = lun.join("cdrom");
    let s = read_trimmed(&cdrom_path).await?;
    Ok(s == "1")
}

async fn write_file(path: &Path, data: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        // .create(true)
        // .truncate(true)
        .open(path)
        // .or_else(|_| OpenOptions::new().write(true).create(true).open(path))
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut writer = BufWriter::new(&mut f);
    writer
        .write_all(data.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    writer.flush().await.ok();
    Ok(())
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

// ==========================
// HTTP/HTTPS Range Reader (reqwest + rustls)
// ==========================

struct HttpRangeReader {
    client: reqwest::blocking::Client,
    url: String,
    size: i64,
}

impl HttpRangeReader {
    fn new(url: &str) -> Result<Self> {
        // Accept http and https
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(anyhow!("unsupported url scheme"));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("failed to build http client")?;

        // Try HEAD first
        let mut size: Option<i64> = None;
        if let Ok(resp) = client.head(url).send()
            && resp.status().is_success()
            && let Some(len) = resp.headers().get(reqwest::header::CONTENT_LENGTH)
            && let Ok(s) = len.to_str()
            && let Ok(n) = s.parse::<i64>()
        {
            size = Some(n);
        }
        // Fallback: GET Range 0-0 and parse Content-Range: bytes 0-0/total
        if size.is_none() {
            let resp = client
                .get(url)
                .header(reqwest::header::RANGE, "bytes=0-0")
                .send()
                .context("range request failed")?;
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT
            {
                return Err(anyhow!("range probe failed with status {}", resp.status()));
            }
            if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE)
                && let Ok(s) = cr.to_str()
            {
                // format: bytes 0-0/12345
                if let Some((_, total)) = s.rsplit_once('/')
                    && let Ok(n) = total.trim().parse::<i64>()
                {
                    size = Some(n);
                }
            }
            if size.is_none() {
                // As a last resort, use content-length when 200 OK and whole file sent (not ideal)
                if let Some(len) = resp.headers().get(reqwest::header::CONTENT_LENGTH)
                    && let Ok(s) = len.to_str()
                    && let Ok(n) = s.parse::<i64>()
                {
                    size = Some(n);
                }
            }
        }

        let size = size.ok_or_else(|| anyhow!("failed to determine remote size"))?;
        Ok(Self { client, url: url.to_string(), size })
    }
}

impl RemoteImageReader for HttpRangeReader {
    fn read_at(&self, off: i64, len: i64) -> Result<Vec<u8>> {
        if off < 0 || len < 0 {
            return Err(anyhow!("invalid range"));
        }
        let end = (off + len - 1).max(off);
        let resp = self
            .client
            .get(&self.url)
            .header(reqwest::header::RANGE, format!("bytes={}-{}", off, end))
            .send()
            .context("range request failed")?;
        if !(resp.status().is_success() || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT) {
            return Err(anyhow!("GET range request failed with status {}", resp.status()));
        }
        let bytes = resp.bytes().context("failed to read response body")?;
        Ok(bytes.to_vec())
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

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build http client")?;

    // Try HEAD first
    if let Ok(resp) = client.head(url).send()
        && resp.status().is_success()
        && let Some(len) = resp.headers().get(reqwest::header::CONTENT_LENGTH)
        && let Ok(s) = len.to_str()
        && let Ok(n) = s.parse::<i64>()
    {
        return Ok(UrlCheckResult { usable: true, reason: None, size: n });
    }

    // Fallback: GET Range 0-0
    match client.get(url).header(reqwest::header::RANGE, "bytes=0-0").send() {
        Ok(resp) => {
            if !(resp.status().is_success()
                || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT)
            {
                return Ok(UrlCheckResult {
                    usable: false,
                    reason: Some(format!("status {}", resp.status())),
                    size: 0,
                });
            }
            if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE)
                && let Ok(s) = cr.to_str()
                && let Some((_, total)) = s.rsplit_once('/')
                && let Ok(n) = total.trim().parse::<i64>()
            {
                return Ok(UrlCheckResult { usable: true, reason: None, size: n });
            }
            if let Some(len) = resp.headers().get(reqwest::header::CONTENT_LENGTH)
                && let Ok(s) = len.to_str()
                && let Ok(n) = s.parse::<i64>()
            {
                return Ok(UrlCheckResult { usable: true, reason: None, size: n });
            }
            Ok(UrlCheckResult { usable: true, reason: None, size: 0 })
        }
        Err(e) => Ok(UrlCheckResult { usable: false, reason: Some(e.to_string()), size: 0 }),
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
    fn read_at(&self, off: i64, len: i64) -> Result<Vec<u8>> {
        let handler =
            WEBRTC_READ_HANDLER.read().clone().ok_or_else(|| anyhow!("not active session"))?;
        if off < 0 || len < 0 {
            return Err(anyhow!("invalid range"));
        }
        let end = (off + len).min(self.size);
        let req_len = (end - off).max(0);
        if req_len == 0 {
            return Ok(Vec::new());
        }
        handler.read(off, req_len)
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

static PENDING_UPLOADS: Lazy<Mutex<HashMap<String, PendingUpload>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static FT_PENDING_UPLOADS: Lazy<Mutex<HashMap<String, FtPendingUpload>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static FT_PENDING_UPLOADS_CLOSED: Lazy<Mutex<HashMap<String, FtPendingUpload>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

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
    let mut map = PENDING_UPLOADS.lock().await;
    let Some(entry) = map.get_mut(upload_id) else {
        return Err(anyhow!("upload not found"));
    };
    entry
        .file
        .write_all(data)
        .await
        .with_context(|| format!("failed to write upload: {}", upload_id))?;
    // entry.file.flush().with_context(|| format!("failed to flush upload: {}", upload_id))?;
    entry.already_uploaded_bytes += data.len() as i64;
    Ok(entry.already_uploaded_bytes)
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
        let mut map = FT_PENDING_UPLOADS_CLOSED.lock().await;
        let cache_size = remote_fs.file_size(file_cache_path.as_str()).await?;
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
            remote_fs.delete_file(file_cache_path.as_str()).await?;
            (None, None)
        };
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
    let mut map = FT_PENDING_UPLOADS.lock().await;
    let Some(entry) = map.get_mut(upload_id) else {
        return Err(anyhow!("upload not found"));
    };

    let Some(writer) = entry.writer.as_mut() else {
        return Err(anyhow!("writer not found"));
    };

    let size = writer.write_chunk(data).await?;
    if size != data.len() {
        return Err(anyhow!("Failed to write all data in file"));
    }
    entry.already_uploaded_bytes += data.len() as i64;
    Ok(entry.already_uploaded_bytes)
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
    {
        let mut process = FT_PENDING_UPLOADS.lock().await;
        for item in process.values_mut() {
            if let Some(writer) = item.writer.as_mut() {
                let _ = writer.shutdown().await;
            }
        }
        process.clear();
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
