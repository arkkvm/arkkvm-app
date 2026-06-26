//! Hardware module test example
//!
//! This example demonstrates how to use the arkkvm hardware module
//! to interact with device hardware features like serial number extraction,
//! OTP entropy reading, and watchdog management.

use anyhow::Result;
use arkkvm::hardware::hw;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();

    info!("Starting hardware module test...");

    // Test 1: Device ID extraction
    test_device_id_extraction().await?;

    // Test 2: OTP entropy reading
    test_otp_entropy_reading().await?;

    // Test 3: Hostname generation
    test_hostname_generation().await?;

    // Test 4: Watchdog operations (commented out for safety)
    test_watchdog_operations().await?;

    info!("Hardware module test completed successfully!");
    Ok(())
}

/// Test device serial number extraction
async fn test_device_id_extraction() -> Result<()> {
    info!("Testing device ID extraction...");

    // Test cached device ID
    let device_id = hw::get_device_id();
    info!("📱 Device ID (cached): {}", device_id);

    Ok(())
}

/// Test OTP entropy reading
async fn test_otp_entropy_reading() -> Result<()> {
    info!("Testing OTP entropy reading...");

    match hw::read_otp_entropy() {
        Ok(entropy) => {
            info!("✅ Successfully read OTP entropy: {:?}", entropy);
            info!("📊 Entropy length: {} bytes", entropy.len());

            // Convert to hex string for display
            let hex_string =
                entropy.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join("");
            info!("🔐 Entropy (hex): {}", hex_string);
        }
        Err(e) => {
            warn!("⚠️  Failed to read OTP entropy: {}", e);
            // Check error message for specific error types
            let error_msg = e.to_string();
            if error_msg.contains("Failed to open OTP entropy file") {
                info!("ℹ️  OTP device not found (expected on non-RV1106 devices)");
            } else if error_msg.contains("Permission denied") {
                warn!("🚫 Permission denied accessing OTP device");
            } else if error_msg.contains("OTP content too short") {
                warn!("⚙️  Invalid OTP configuration");
            } else {
                error!("❌ Unexpected error reading OTP: {}", e);
            }
        }
    }

    Ok(())
}

/// Test hostname generation
async fn test_hostname_generation() -> Result<()> {
    info!("Testing hostname generation...");

    let hostname = hw::get_default_hostname();
    info!("🏠 Generated hostname: {}", hostname);

    // Test different scenarios
    let device_id = hw::get_device_id();
    if device_id == "unknown_device_id" {
        info!("ℹ️  Using default hostname 'arkkvm' (unknown device)");
    } else {
        info!("ℹ️  Using device-specific hostname 'arkkvm-{}'", device_id.to_lowercase());
    }

    Ok(())
}

/// Test watchdog operations (commented out for safety)
#[allow(dead_code)]
async fn test_watchdog_operations() -> Result<()> {
    info!("Testing watchdog operations...");

    // WARNING: These operations can affect system stability
    // Only uncomment if you know what you're doing

    // Test watchdog disarm
    match hw::disarm_watchdog() {
        Ok(()) => {
            info!("✅ Successfully disarmed watchdog");
        }
        Err(e) => {
            warn!("⚠️  Failed to disarm watchdog: {}", e);
            // Check error message for specific error types
            let error_msg = e.to_string();
            if error_msg.contains("Failed to open watchdog device") {
                info!("ℹ️  Watchdog device not found");
            } else if error_msg.contains("Permission denied") {
                warn!("🚫 Permission denied accessing watchdog device");
            } else {
                error!("❌ Unexpected error disarming watchdog: {}", e);
            }
        }
    }

    // Test watchdog reset (runs indefinitely)
    info!("Starting watchdog reset loop (Ctrl+C to stop)...");
    let cancel_token = CancellationToken::new();
    hw::run_watchdog(cancel_token).await?;

    Ok(())
}

/// Additional test: Error handling demonstration
#[allow(dead_code)]
async fn test_error_handling() -> Result<()> {
    info!("Testing error handling...");

    // Demonstrate anyhow error handling
    let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "Device not found");
    let anyhow_error = anyhow::Error::from(io_error);
    info!("🔄 Converted IO error to anyhow::Error: {}", anyhow_error);

    // Test JSON serialization error
    let json_error = serde_json::from_str::<serde_json::Value>("invalid json");
    if let Err(e) = json_error {
        let anyhow_error = anyhow::Error::from(e);
        info!("🔄 Converted JSON error to anyhow::Error: {}", anyhow_error);
    }

    Ok(())
}
