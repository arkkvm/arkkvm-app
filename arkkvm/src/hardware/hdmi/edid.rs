//! EDID (Extended Display Identification Data) management for HDMI
//! 
//! This module provides functions to get, set, and restore default EDID
//! for HDMI video capture devices using v4l2-ctl commands.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::fs;
use tokio::process::Command;
use tracing::{info, warn};

use super::{hdmirx, SUB_DEV};

/// Custom EDID storage directory path
const EDID_PATH: &str = "/userdata/arkkvm/edid";
/// Custom EDID file name
const EDID_FILE_NAME: &str = "custom_edid.bin";

const EDID_DEFAULT: &str = "00ffffffffffff0049738d6200888888081e0103800000780a0dc9a05747982712484c00000001010101010101010101010101010101023a801871382d40582c4500c48e2100001e000000110000000000000000000000000000000000fc0041726b4b564d446973706c6179000000fd00147801ff1d000a202020202020016a02031871429022230904018301000065030c001000e200cb023a801871382d40582c450020c23100001e023a80d072382d40102c458020c23100001e011d801871382d40582c4500c06c00000018011d8018711c1620582c2500c06c0000001800000000000000000000000000000000000000000000000000000000000000a0";

/// Initialize EDID on device startup
/// 
/// Checks if the current device EDID configuration matches user settings,
/// and updates if inconsistent.
pub async fn init_edid() -> Result<()> {
    let edid_dir = Path::new(EDID_PATH);
    let file_path = edid_dir.join(EDID_FILE_NAME);
    
    // Check if user custom EDID file exists
    if !file_path.exists() {
        info!("No custom EDID file found, using device default");
        return Ok(());
    }
    
    // Read user saved EDID
    let saved_edid = fs::read(&file_path)
        .await
        .with_context(|| format!("Failed to read EDID file: {}", file_path.display()))?;
    
    // Get current device EDID
    let current_edid = get_edid().await?;
    
    // Compare EDID data
    if saved_edid == current_edid {
        info!("Device EDID matches user settings, no update needed");
        return Ok(());
    }
    
    // EDID mismatch, update device
    info!("Device EDID differs from user settings, updating device");
    flush_edid_to_device().await?;
    info!("EDID initialized successfully");
    Ok(())
}

/// Get current device EDID as hex string
/// 
/// # Returns
/// 
/// Returns the EDID data as a hex-encoded string
pub async fn get_edid_str() -> Result<String> {
    let edid_data = get_edid().await?;
    Ok(edid_to_string(&edid_data))
}

pub fn get_default_edid_str() -> String {
    EDID_DEFAULT.to_string()
}

/// Update user custom EDID configuration and apply to device
/// 
/// # Arguments
/// 
/// * `edid_str` - EDID data as hex string. If None, delete custom EDID file and restore default
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
pub async fn update_edid(edid_str: Option<&str>) -> Result<()> {
    let edid_dir = Path::new(EDID_PATH);
    let file_path = edid_dir.join(EDID_FILE_NAME);
    
    match edid_str {
        Some(edid_str) => {
            // Convert string to bytes
            let edid_data = str_to_edid(edid_str)?;
            
            // Save to file
            save_edid_to_file(&edid_data).await?;
            
            // Apply to device
            flush_edid_to_device().await?;
            
            info!("Updated custom EDID and applied to device");
        }
        None => {
            // Delete custom EDID file if exists
            if file_path.exists() {
                let edid_data = str_to_edid(EDID_DEFAULT)?;
                save_edid_to_file(&edid_data).await?;
                flush_edid_to_device().await?;
                
                fs::remove_file(&file_path).await?;
                info!("Removed custom EDID and restored default");
            }
            else {
                warn!("No custom EDID file found, skipping restore");
            }
        }
    }
    
    Ok(())
}


/// Get EDID from the device
/// 
/// # Returns
/// 
/// Returns the EDID data as a vector of bytes (128 or 256 bytes)
async fn get_edid() -> Result<Vec<u8>> {
    let output = Command::new("v4l2-ctl")
        .arg("--device")
        .arg(SUB_DEV)
        .arg("--get-edid")
        .arg("pad=0,format=raw")
        .output()
        .await
        .context("Failed to execute v4l2-ctl command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to get EDID: {}", stderr));
    }
    
    info!("Got EDID: {} bytes", output.stdout.len());
    Ok(output.stdout)
}

/// Flush EDID from file to device
/// 
/// Reads EDID from the fixed file path (/userdata/arkkvm/edid/custom_edid.bin)
/// and applies it to the device. This function is used to synchronize the device
/// EDID with the saved user configuration.
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
async fn flush_edid_to_device() -> Result<()> {
    let edid_dir = Path::new(EDID_PATH);
    let file_path = edid_dir.join(EDID_FILE_NAME);
    
    // Validate file exists
    if !file_path.exists() {
        anyhow::bail!("EDID file does not exist: {}", file_path.display());
    }
    
    info!("Setting EDID from file: {}", file_path.display());
    
    let output = Command::new("v4l2-ctl")
        .arg("--device")
        .arg(SUB_DEV)
        .arg("--set-edid")
        .arg(format!("pad=0,file={},format=raw", file_path.display()))
        .output()
        .await
        .context("Failed to execute v4l2-ctl command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to set EDID from file: {}", stderr));
    }
    
    info!("Set EDID from file: {} successfully", file_path.display());
    
    // Restart HDMIRX to apply EDID changes
    hdmirx::restart().await?;
    
    Ok(())
}

/// Save EDID raw data to file
/// 
/// # Arguments
/// 
/// * `edid_data` - EDID data to save
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
async fn save_edid_to_file(edid_data: &[u8]) -> Result<()> {
    // Validate EDID size
    validate_edid_size(edid_data)?;
    
    // Create directory if it doesn't exist
    let edid_dir = Path::new(EDID_PATH);
    if !edid_dir.exists() {
        fs::create_dir_all(edid_dir)
            .await
            .with_context(|| format!("Failed to create EDID directory: {}", EDID_PATH))?;
        info!("Created EDID directory: {}", EDID_PATH);
    }
    
    // Build full file path
    let file_path = edid_dir.join(EDID_FILE_NAME);
    
    // Write EDID data to file
    fs::write(&file_path, edid_data)
        .await
        .with_context(|| format!("Failed to write EDID to file: {}", file_path.display()))?;
    
    info!("Saved EDID to file: {} ({} bytes)", file_path.display(), edid_data.len());
    Ok(())
}

/// Restore default EDID (equivalent to v4l2-ctl --set-edid=pad=0,type=hdmi)
/// 
/// # Returns
/// 
/// Returns Ok(()) on success
async fn restore_default_edid() -> Result<()> {
    info!("Restoring default EDID");
    
    let output = Command::new("v4l2-ctl")
        .arg("--device")
        .arg(SUB_DEV)
        .arg("--set-edid")
        .arg("pad=0,type=hdmi")
        .output()
        .await
        .context("Failed to execute v4l2-ctl command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to restore default EDID: {}", stderr));
    }
    
    info!("Restored default EDID successfully");
    
    // Restart HDMIRX to apply EDID changes
    hdmirx::restart().await?;
    
    Ok(())
}

/// Validate EDID size
/// 
/// # Arguments
/// 
/// * `edid_data` - EDID raw data to validate
/// 
/// # Returns
/// 
/// Returns Ok(()) if size is valid (128 or 256 bytes), otherwise returns an error
fn validate_edid_size(edid_data: &[u8]) -> Result<()> {
    if edid_data.len() != 128 && edid_data.len() != 256 {
        anyhow::bail!(
            "Invalid EDID size: {} bytes (must be 128 or 256 bytes)",
            edid_data.len()
        );
    }
    Ok(())
}

/// Convert EDID raw data to hex string
/// 
/// # Arguments
/// 
/// * `edid_data` - EDID raw data (bytes)
/// 
/// # Returns
/// 
/// Returns hex-encoded string representation of EDID data
fn edid_to_string(edid_data: &[u8]) -> String {
    hex::encode(edid_data)
}

/// Convert hex string to EDID raw data
/// 
/// # Arguments
/// 
/// * `edid_string` - Hex-encoded string representation of EDID data
/// 
/// # Returns
/// 
/// Returns EDID raw data as Vec<u8>
fn str_to_edid(edid_str: &str) -> Result<Vec<u8>> {
    let edid_data = hex::decode(edid_str)
        .map_err(|e| anyhow::anyhow!("Failed to decode hex string: {}", e))?;
    
    // Validate EDID size
    validate_edid_size(&edid_data)?;
    
    Ok(edid_data)
}
