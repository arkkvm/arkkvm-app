use crate::error::UsbError;
use std::path::Path;
use std::process::Command;

const CONFIGFS_MOUNT_POINT: &str = "/sys/kernel/config";
const CONFIGFS_FS_TYPE: &str = "configfs";
const PROC_MOUNTS_PATH: &str = "/proc/mounts";
const MOUNT_CMD: &str = "mount";
const MOUNT_NONE_SOURCE: &str = "none";

pub fn ensure_mounted() -> Result<(), UsbError> {
    ensure_mount_point_exists()?;

    if !is_configfs_mounted()? {
        mount_configfs()?;
    }

    Ok(())
}

fn ensure_mount_point_exists() -> Result<(), UsbError> {
    let mount_point = Path::new(CONFIGFS_MOUNT_POINT);
    if mount_point.exists() {
        return Ok(());
    }

    std::fs::create_dir_all(mount_point)
        .map_err(|e| UsbError::GadgetError(format!("failed to create configfs dir: {}", e)))
}

fn is_configfs_mounted() -> Result<bool, UsbError> {
    let mounts = std::fs::read_to_string(PROC_MOUNTS_PATH)
        .map_err(|e| UsbError::GadgetError(format!("failed to read {}: {}", PROC_MOUNTS_PATH, e)))?;

    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let _src = parts.next();
        let mount_point = parts.next().unwrap_or_default();
        let fs_type = parts.next().unwrap_or_default();

        if mount_point == CONFIGFS_MOUNT_POINT && fs_type == CONFIGFS_FS_TYPE {
            return Ok(true);
        }
    }

    Ok(false)
}

fn mount_configfs() -> Result<(), UsbError> {
    let status = Command::new(MOUNT_CMD)
        .args(["-t", CONFIGFS_FS_TYPE, MOUNT_NONE_SOURCE, CONFIGFS_MOUNT_POINT])
        .status()
        .map_err(|e| UsbError::GadgetError(format!("failed to execute mount for configfs: {}", e)))?;

    if status.success() {
        return Ok(());
    }

    Err(UsbError::GadgetError(format!(
        "mount configfs failed with status: {}",
        status
    )))
}
