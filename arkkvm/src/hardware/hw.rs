use anyhow::{Context, Result, bail};
use once_cell::sync::OnceCell;
use rustix::fs::{Mode, OFlags, open};
use rustix::io::{read, write};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

static DEVICE_ID: OnceCell<String> = OnceCell::new();

pub fn read_otp_entropy() -> Result<Vec<u8>> {
    let file = open("/sys/bus/nvmem/devices/rockchip-otp0/nvmem", OFlags::RDONLY, Mode::empty())
        .context("Failed to open OTP entropy file")?;

    let mut content = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        match read(&file, &mut buffer) {
            Ok(0) => break,
            Ok(n) => content.extend_from_slice(&buffer[..n]),
            Err(e) => return Err(e).context("Failed to read OTP entropy"),
        }
    }

    if content.len() < 28 {
        bail!("OTP content too short (expected at least 28 bytes, got {})", content.len())
    }

    Ok(content[0x17..0x1C].to_vec())
}

pub fn get_device_id() -> String {
    DEVICE_ID
        .get_or_init(|| match common::device::extract_serial_number() {
            Ok(serial) => {
                debug!("Extracted device serial number: {}", serial);
                serial
            }
            Err(e) => {
                warn!("Unknown serial number, the program likely not running on RV1106: {}", e);
                "unknown_device_id".to_string()
            }
        })
        .clone()
}

pub fn get_default_hostname() -> String {
    let device_id = get_device_id();

    if device_id == "unknown_device_id" {
        "arkkvm".to_string()
    } else {
        format!("arkkvm-{}", device_id.to_lowercase())
    }
}

pub async fn run_watchdog(cancel_token: CancellationToken) -> Result<()> {
    let file = match open("/dev/watchdog", OFlags::WRONLY, Mode::empty()) {
        Ok(file) => file,
        Err(e) => {
            warn!("Unable to open /dev/watchdog, skipping watchdog reset: {}", e);
            return Ok(());
        }
    };

    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = write(&file, &[0]) {
                    warn!("Error writing to /dev/watchdog, system may reboot: {}", e);
                }
            }
            _ = cancel_token.cancelled() => {
                if let Err(e) = write(&file, b"V") {
                    warn!("Failed to disarm watchdog, system may reboot: {}", e);
                }
                return Ok(());
            }
        }
    }
}

pub fn disarm_watchdog() -> Result<()> {
    let file = open("/dev/watchdog", OFlags::WRONLY, Mode::empty())
        .context("Failed to open watchdog device")?;

    write(&file, b"V").context("Failed to disarm watchdog")?;

    Ok(())
}
