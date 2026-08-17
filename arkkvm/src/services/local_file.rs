//! Local file access under `/userdata/arkkvm` with path sandboxing, range reads,
//! and WebRTC DataChannel backpressure helpers.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use serde::Serialize;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannel;

use crate::config::CONFIG_DIR;

pub const LOCAL_FILE_CHUNK_SIZE: usize = 4096;
pub const DC_BUFFER_LOW_WATER: usize = 64 * 1024;
pub const DC_BUFFER_HIGH_WATER: usize = 256 * 1024;

/// Inclusive byte range `[start, end]` within `[0, file_size - 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
  pub fn len(&self) -> u64 {
        if self.end < self.start {
            0
        } else {
            self.end - self.start + 1
        }
    }

    /// Bytes to transfer for a file of `file_size` (handles empty files).
    pub fn content_len(&self, file_size: u64) -> u64 {
        if file_size == 0 {
            0
        } else {
            self.len()
        }
    }

    pub fn full_file(file_size: u64) -> Self {
        if file_size == 0 {
            Self { start: 0, end: 0 }
        } else {
            Self { start: 0, end: file_size - 1 }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFileInfo {
    pub name: String,
    pub size: u64,
    pub path: String,
}

static BASE_CANON: OnceLock<PathBuf> = OnceLock::new();

async fn base_canonical() -> Result<PathBuf> {
    if let Some(base) = BASE_CANON.get() {
        return Ok(base.clone());
    }
    let _ = fs::create_dir_all(CONFIG_DIR).await;
    let canon = fs::canonicalize(CONFIG_DIR)
        .await
        .with_context(|| format!("failed to canonicalize {CONFIG_DIR}"))?;
    let _ = BASE_CANON.set(canon.clone());
    Ok(canon)
}

fn validate_relative_components(relative: &str) -> Result<()> {
    if relative.is_empty() {
        bail!("empty path");
    }
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir | Component::CurDir => {
                bail!("unsafe path component in '{relative}'");
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute path not allowed in '{relative}'");
            }
            Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn user_path_to_relative(user_path: &str) -> Result<String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        bail!("empty path");
    }

    let relative = if let Some(rest) = trimmed.strip_prefix(CONFIG_DIR) {
        rest.trim_start_matches('/')
    } else if trimmed.starts_with('/') {
        bail!("path outside allowed directory");
    } else {
        trimmed
    };

    if relative.is_empty() {
        bail!("empty path");
    }

    validate_relative_components(relative)?;
    Ok(relative.replace('\\', "/"))
}

/// Resolve a user path to a canonical file path under `CONFIG_DIR`.
pub async fn resolve_local_file_path(user_path: &str) -> Result<PathBuf> {
    let relative = user_path_to_relative(user_path)?;
    let base = base_canonical().await?;
    let candidate = base.join(&relative);

    let metadata = fs::metadata(&candidate)
        .await
        .with_context(|| "file not found")?;
    if !metadata.is_file() {
        bail!("not a regular file");
    }

    let resolved = fs::canonicalize(&candidate)
        .await
        .with_context(|| "failed to resolve file path")?;
    if !resolved.starts_with(&base) {
        bail!("path outside allowed directory");
    }

    Ok(resolved)
}

fn validate_evidence_basename(path: &str) -> Result<()> {
    let relative = user_path_to_relative(path)?;
    let components: Vec<_> = Path::new(&relative).components().collect();
    if components.len() != 1 {
        bail!("path must be a filename without directories");
    }
    if !relative.ends_with(".zip") && !relative.ends_with(".tgz") {
        bail!("only .zip and .tgz evidence archives can be deleted");
    }
    Ok(())
}

/// Delete a collectEvidence archive under `CONFIG_DIR` (basename only, `.zip`/`.tgz`).
pub async fn delete_collected_evidence_file(user_path: &str) -> Result<()> {
    validate_evidence_basename(user_path)?;
    let relative = user_path_to_relative(user_path)?;
    let base = base_canonical().await?;
    let candidate = base.join(&relative);

    if fs::metadata(&candidate).await.is_err() {
        return Ok(());
    }

    let resolved = resolve_local_file_path(user_path).await?;
    fs::remove_file(&resolved)
        .await
        .context("failed to delete evidence file")?;
    Ok(())
}

pub async fn get_local_file_info(user_path: &str) -> Result<LocalFileInfo> {
    let relative = user_path_to_relative(user_path)?;
    let resolved = resolve_local_file_path(user_path).await?;
    let metadata = fs::metadata(&resolved).await?;
    let name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    Ok(LocalFileInfo {
        name,
        size: metadata.len(),
        path: relative,
    })
}

/// Parse RFC 7233 `Range: bytes=...` (single range only).
pub fn parse_range_header(header: &str, file_size: u64) -> Result<Option<ByteRange>> {
    let header = header.trim();
    if header.is_empty() {
        return Ok(None);
    }

    let Some(spec) = header.strip_prefix("bytes=") else {
        bail!("invalid range unit");
    };

    if spec.contains(',') {
        bail!("multipart ranges are not supported");
    }

    let range = if let Some(suffix) = spec.strip_prefix('-') {
        let suffix_len: u64 = suffix.parse().map_err(|_| anyhow!("invalid suffix range"))?;
        if suffix_len == 0 {
            bail!("invalid suffix range");
        }
        if file_size == 0 {
            bail!("range not satisfiable");
        }
        let start = file_size.saturating_sub(suffix_len);
        ByteRange { start, end: file_size - 1 }
    } else if let Some((start_s, end_s)) = spec.split_once('-') {
        let start: u64 = start_s.parse().map_err(|_| anyhow!("invalid range start"))?;
        if end_s.is_empty() {
            if file_size == 0 {
                if start > 0 {
                    bail!("range not satisfiable");
                }
                ByteRange { start: 0, end: 0 }
            } else {
                if start >= file_size {
                    bail!("range not satisfiable");
                }
                ByteRange { start, end: file_size - 1 }
            }
        } else {
            let end: u64 = end_s.parse().map_err(|_| anyhow!("invalid range end"))?;
            if file_size == 0 || start > end || end >= file_size {
                bail!("range not satisfiable");
            }
            ByteRange { start, end }
        }
    } else {
        bail!("invalid range spec");
    };

    Ok(Some(range))
}

pub struct LocalFileReader {
    file: fs::File,
    chunk_size: usize,
    range_start: u64,
    current_position: u64,
    end_position: u64,
    file_size: u64,
}

impl LocalFileReader {
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn range_len(&self) -> u64 {
        if self.file_size == 0 {
            0
        } else {
            self.end_position.saturating_sub(self.range_start) + 1
        }
    }

    pub fn position(&self) -> u64 {
        self.current_position
    }

    pub async fn read_next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.current_position > self.end_position {
            return Ok(None);
        }

        let remaining = self.end_position - self.current_position + 1;
        let read_size = std::cmp::min(self.chunk_size, remaining as usize);

        let mut buffer = vec![0u8; read_size];
        let mut total_read = 0usize;
        while total_read < read_size {
            let n = self
                .file
                .read(&mut buffer[total_read..])
                .await
                .with_context(|| format!("read failed at {}", self.current_position))?;
            if n == 0 {
                break;
            }
            total_read += n;
        }

        if total_read == 0 {
            return Ok(None);
        }

        buffer.truncate(total_read);
        self.current_position += total_read as u64;
        Ok(Some(buffer))
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.file.shutdown().await.context("failed to shutdown file")
    }
}

pub async fn open_local_file_reader(
    user_path: &str,
    range: Option<ByteRange>,
) -> Result<LocalFileReader> {
    let resolved = resolve_local_file_path(user_path).await?;
    let mut file = fs::File::open(&resolved)
        .await
        .with_context(|| format!("failed to open {:?}", resolved))?;

    let file_size = file
        .metadata()
        .await
        .with_context(|| "failed to stat file")?
        .len();

    let range = range.unwrap_or_else(|| ByteRange::full_file(file_size));
    if file_size > 0 && range.start >= file_size {
        bail!("range start beyond file size");
    }

    let end_position = if file_size == 0 {
        0
    } else {
        std::cmp::min(range.end, file_size - 1)
    };

    if range.start > end_position && file_size > 0 {
        bail!("invalid byte range");
    }

    if range.start > 0 || (file_size > 0 && end_position < file_size - 1) {
        file.seek(std::io::SeekFrom::Start(range.start))
            .await
            .context("failed to seek file")?;
    }

    Ok(LocalFileReader {
        file,
        chunk_size: LOCAL_FILE_CHUNK_SIZE,
        range_start: range.start,
        current_position: range.start,
        end_position,
        file_size,
    })
}

/// Cooperative backpressure on RTCDataChannel SCTP send queue.
pub struct DataChannelBackpressure {
    high_threshold: usize,
    notify: Arc<Notify>,
}

impl DataChannelBackpressure {
    pub async fn attach(channel: &RTCDataChannel) -> Arc<Self> {
        channel
            .set_buffered_amount_low_threshold(DC_BUFFER_LOW_WATER)
            .await;

        let bp = Arc::new(Self {
            high_threshold: DC_BUFFER_HIGH_WATER,
            notify: Arc::new(Notify::new()),
        });

        let notify = bp.notify.clone();
        channel.on_buffered_amount_low(Box::new(move || {
            let notify = notify.clone();
            Box::pin(async move {
                notify.notify_waiters();
            })
        }));

        bp
    }

    pub async fn wait_writable(&self, channel: &RTCDataChannel) {
        loop {
            let buffered = channel.buffered_amount().await;
            if buffered <= self.high_threshold {
                return;
            }
            self.notify.notified().await;
        }
    }
}
