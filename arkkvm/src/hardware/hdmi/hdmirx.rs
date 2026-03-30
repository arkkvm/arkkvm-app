//! HDMIRX control module
//!
//! This module provides functions to enable, disable, and restart HDMIRX
//! for HDMI video capture devices.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::fs;
use tracing::info;

/// HDMIRX enable control file path
const HDMIRX_EN_PATH: &str = "/sys/devices/platform/ff460000.i2c/i2c-3/3-0050/hdmirx_en";

/// Disable HDMIRX (set hdmirx_en to 0)
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
pub async fn disable() -> Result<()> {
    let path = Path::new(HDMIRX_EN_PATH);
    
    if !path.exists() {
        return Err(anyhow::anyhow!("HDMIRX control file does not exist: {}", HDMIRX_EN_PATH));
    }
    
    fs::write(path, "0")
        .await
        .with_context(|| format!("Failed to disable HDMIRX: {}", HDMIRX_EN_PATH))?;
    
    info!("Disabled HDMIRX");
    Ok(())
}

/// Enable HDMIRX (set hdmirx_en to 1)
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
pub async fn enable() -> Result<()> {
    let path = Path::new(HDMIRX_EN_PATH);
    
    if !path.exists() {
        return Err(anyhow::anyhow!("HDMIRX control file does not exist: {}", HDMIRX_EN_PATH));
    }
    
    fs::write(path, "1")
        .await
        .with_context(|| format!("Failed to enable HDMIRX: {}", HDMIRX_EN_PATH))?;
    
    info!("Enabled HDMIRX");
    Ok(())
}

/// Restart HDMIRX (disable then enable) to apply configuration changes
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
pub async fn restart() -> Result<()> {
    info!("Restarting HDMIRX to apply configuration changes");
    
    // Disable HDMIRX
    disable().await?;
    
    // Wait a short moment before re-enabling
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    
    // Enable HDMIRX
    enable().await?;
    
    info!("HDMIRX restarted successfully");
    Ok(())
}

