use std::path::Path;

use anyhow::{Result, anyhow};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{error, info, warn};
use std::os::unix::fs::PermissionsExt;

use crate::config;

// =====================
// SSH key management
// =====================
const SSH_KEY_DIR: &str = "/root/.ssh";
const USER_SSH_KEY_DIR: &str = "/userdata/arkkvm/ssh";
const SSH_KEY_FILE: &str = "authorized_keys";
const SSHD_PATH: &str = "/etc/init.d/S91sshd";

pub async fn init_ssh_key() -> Result<()> {
    let dev_mode = config::get_dev_mode_state().await?;
    if !dev_mode.enabled {
        stop_sshd().await?;
        return Ok(());
    }

    let user_ssh_key_file = format!("{}/{}", USER_SSH_KEY_DIR, SSH_KEY_FILE);
    let ssh_key_file = format!("{}/{}", SSH_KEY_DIR, SSH_KEY_FILE);
    if !Path::new(&user_ssh_key_file).exists() {
        return Ok(());
    }

    if !Path::new(SSH_KEY_DIR).exists() {
        tokio::fs::create_dir_all(SSH_KEY_DIR).await?;
        let dir_meta = tokio::fs::metadata(SSH_KEY_DIR).await?;
        let mut dir_perm = dir_meta.permissions();
        dir_perm.set_mode(0o700);
        tokio::fs::set_permissions(SSH_KEY_DIR, dir_perm).await?;
    }

    let ssh_key_file_path = Path::new(&ssh_key_file);
    if ssh_key_file_path.exists() {
        if let Ok(metadata) = ssh_key_file_path.metadata() {
            if metadata.len() > 0 {
                return Ok(());
            }
        }
    }

    tokio::fs::copy(&user_ssh_key_file, &ssh_key_file).await?;
    Ok(())
}

pub async fn get_ssh_key() -> Option<String> {
    match tokio::fs::read_to_string(format!("{}/{}", SSH_KEY_DIR, SSH_KEY_FILE)).await {
        Ok(s) => Some(s),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                warn!("SSH key file not found");
            } else {
                error!("error reading SSH key file: {:?}", e);
            }
            None
        }
    }
}

pub async fn set_ssh_key(key: Option<&str>) -> Result<()> {
    if let Some(ssh_key) = key { write_ssh_key(ssh_key).await } else { clear_ssh_key().await }
}

async fn write_ssh_key(key: &str) -> Result<()> {
    if !Path::new(USER_SSH_KEY_DIR).exists() {
        tokio::fs::create_dir_all(USER_SSH_KEY_DIR).await?;
    }

    if !Path::new(SSH_KEY_DIR).exists() {
        tokio::fs::create_dir_all(SSH_KEY_DIR).await?;
        let dir_meta = tokio::fs::metadata(SSH_KEY_DIR).await?;
        let mut dir_perm = dir_meta.permissions();
        dir_perm.set_mode(0o700);
        tokio::fs::set_permissions(SSH_KEY_DIR, dir_perm).await?;
    }

    // create file
    let user_ssh_key_file = format!("{}/{}", USER_SSH_KEY_DIR, SSH_KEY_FILE);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&user_ssh_key_file).await?;
    file.write_all(key.as_bytes()).await?;

    // set file perimssions
    let mut dir_perm = file.metadata().await?.permissions();
    dir_perm.set_mode(0o600);
    file.set_permissions(dir_perm).await?;
    drop(file);

    // copy file to root directory
    let ssh_key_file = format!("{}/{}", SSH_KEY_DIR, SSH_KEY_FILE);
    tokio::fs::copy(&user_ssh_key_file, &ssh_key_file).await?;
    Ok(())
}

async fn clear_ssh_key() -> Result<()> {
    let user_ssh_key_file = format!("{}/{}", USER_SSH_KEY_DIR, SSH_KEY_FILE);
    if Path::new(&user_ssh_key_file).exists() {
        // Remove file when empty string
        if let Err(e) = tokio::fs::remove_file(&user_ssh_key_file).await {
            return Err(anyhow!("failed to remove SSH key file: {:?}", e));
        }
    }

    let ssh_key_file = format!("{}/{}", SSH_KEY_DIR, SSH_KEY_FILE);
    if Path::new(&ssh_key_file).exists() {
        // Remove file when empty string
        if let Err(e) = tokio::fs::remove_file(&ssh_key_file).await {
            return Err(anyhow!("failed to remove SSH key file: {:?}", e));
        }
    }
    Ok(())
}

// =====================
// SSHD service management
// =====================

/// Start sshd service
pub async fn start_sshd() -> Result<()> {
    if !Path::new(SSHD_PATH).exists() {
        return Err(anyhow!("SSHD init script not found: {}", SSHD_PATH));
    }

    let output = Command::new(SSHD_PATH)
        .arg("start")
        .output()
        .await
        .map_err(|e| anyhow!("failed to execute SSHD start command: {:?}", e))?;

    if output.status.success() {
        info!("SSHD service started successfully");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("Failed to start SSHD service: {}", stderr);
        Err(anyhow!("SSHD start failed: {}", stderr))
    }
}

/// Stop sshd service
pub async fn stop_sshd() -> Result<()> {
    if !Path::new(SSHD_PATH).exists() {
        return Err(anyhow!("SSHD init script not found: {}", SSHD_PATH));
    }

    let output = Command::new(SSHD_PATH)
        .arg("stop")
        .output()
        .await
        .map_err(|e| anyhow!("failed to execute SSHD stop command: {:?}", e))?;

    if output.status.success() {
        info!("SSHD service stopped successfully");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("Failed to stop SSHD service: {}", stderr);
        Err(anyhow!("SSHD stop failed: {}", stderr))
    }
}

/// Restart sshd service
pub async fn restart_sshd() -> Result<()> {
    if !Path::new(SSHD_PATH).exists() {
        return Err(anyhow!("SSHD init script not found: {}", SSHD_PATH));
    }

    let output = Command::new(SSHD_PATH)
        .arg("restart")
        .output()
        .await
        .map_err(|e| anyhow!("failed to execute SSHD restart command: {:?}", e))?;

    if output.status.success() {
        info!("SSHD service restarted successfully");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("Failed to restart SSHD service: {}", stderr);
        Err(anyhow!("SSHD restart failed: {}", stderr))
    }
}
