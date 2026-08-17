use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context as AnyhowContext, Result, bail};
use serde::Serialize;
use tokio::fs;

use crate::hardware::usb::storage;

const FT_MOUNT_POINT: &str = "/mnt/ft_disk";

static FT_BASE_CANON: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FtFileInfo {
    pub name: String,
    pub size: u64,
    pub path: String,
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

fn normalize_relative_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("empty path");
    }
    if trimmed == "/" {
        return Ok(String::new());
    }
    let relative = trimmed.trim_start_matches('/');
    if relative.is_empty() {
        return Ok(String::new());
    }
    validate_relative_components(relative)?;
    Ok(relative.replace('\\', "/"))
}

fn normalize_filename(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("empty name");
    }
    validate_relative_components(trimmed)?;
    if trimmed.contains('/') || trimmed.contains('\\') {
        bail!("name must not contain directory separators");
    }
    Ok(trimmed.to_string())
}

fn build_relative_file_path(path: &str, name: &str) -> Result<String> {
    let rel_path = normalize_relative_path(path)?;
    let rel_name = normalize_filename(name)?;
    let full = if rel_path.is_empty() {
        PathBuf::from(rel_name)
    } else {
        Path::new(&rel_path).join(rel_name)
    };
    let full = full
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid utf-8 path"))?
        .replace('\\', "/");
    validate_relative_components(&full)?;
    Ok(full)
}

async fn ensure_ft_mounted() -> Result<()> {
    storage::fs_info_with_file_img()
        .await
        .context("file transfer filesystem is not mounted")?;
    Ok(())
}

async fn ft_base_canonical() -> Result<PathBuf> {
    ensure_ft_mounted().await?;
    if let Some(base) = FT_BASE_CANON.get() {
        return Ok(base.clone());
    }
    let canon = fs::canonicalize(FT_MOUNT_POINT)
        .await
        .with_context(|| format!("failed to canonicalize {FT_MOUNT_POINT}"))?;
    let _ = FT_BASE_CANON.set(canon.clone());
    Ok(canon)
}

pub async fn resolve_ft_file_path(path: &str, name: &str) -> Result<PathBuf> {
    let relative = build_relative_file_path(path, name)?;
    let base = ft_base_canonical().await?;
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

pub async fn get_ft_file_info(path: &str, name: &str) -> Result<FtFileInfo> {
    let relative = build_relative_file_path(path, name)?;
    let resolved = resolve_ft_file_path(path, name).await?;
    let metadata = fs::metadata(&resolved).await?;
    let name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(FtFileInfo { name, size: metadata.len(), path: relative })
}
