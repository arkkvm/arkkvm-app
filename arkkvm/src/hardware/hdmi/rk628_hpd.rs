//! RK628 HDMI Hot Plug Detect (HPD) Detection Module
//!
//! Uses RK628 debugfs interface to detect HDMI cable insertion/removal status

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

/// RK628 debugfs status file path
const RK628_DEBUGFS_STATUS: &str = "/sys/kernel/debug/rk628/3-0050/hdmirx/status";

/// RK628 sysfs audio_rate file path (fallback)
const RK628_SYSFS_AUDIO_RATE: &str = "/sys/bus/i2c/devices/3-0050/hdmirx/rk628/audio_rate";

/// HDMI connection status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdmiConnectionStatus {
    /// HDMI cable plugged in
    Connected,
    /// HDMI cable unplugged
    Disconnected,
    /// HDMI cable plugged in but no signal output (resolution is 0)
    NoSignal,
    /// Status unknown
    Unknown,
}

impl HdmiConnectionStatus {
    /// Parse status from string
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "plugin" => HdmiConnectionStatus::Connected,
            "plugout" => HdmiConnectionStatus::Disconnected,
            _ => HdmiConnectionStatus::Unknown,
        }
    }

    /// Convert to boolean value
    pub fn is_connected(self) -> bool {
        matches!(self, HdmiConnectionStatus::Connected)
    }
    
    /// Check if signal is present (Connected and has signal)
    pub fn has_signal(self) -> bool {
        matches!(self, HdmiConnectionStatus::Connected)
    }
}

/// RK628 HPD detector
pub struct Rk628HpdDetector {
    /// Whether it is running
    running: Arc<AtomicBool>,
    /// Status change notification channel
    status_tx: mpsc::Sender<HdmiConnectionStatus>,
}

impl Rk628HpdDetector {
    /// Create new detector
    pub fn new(tx: mpsc::Sender<HdmiConnectionStatus>) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            status_tx: tx,
        }
    }

    /// Read current HDMI connection status (using debugfs)
    pub fn read_status() -> Result<HdmiConnectionStatus> {
        if !Path::new(RK628_DEBUGFS_STATUS).exists() {
            return Err(anyhow::anyhow!(
                "RK628 debugfs status file does not exist: {}",
                RK628_DEBUGFS_STATUS
            ));
        }

        let content = fs::read_to_string(RK628_DEBUGFS_STATUS)
            .with_context(|| format!("Failed to read RK628 debugfs status file"))?;

        let mut plugin_status = None;
        let mut timing_resolution = None;

        // Parse status and timing from content
        for line in content.lines() {
            // Extract status: plugin or status: plugout
            if let Some(status_str) = line.strip_prefix("status:") {
                let status = HdmiConnectionStatus::from_str(status_str.trim());
                plugin_status = Some(status);
            }
            
            // Extract Timing information: "Timing: 0x0p0 (0x0)" or similar
            if let Some(timing_str) = line.strip_prefix("Timing:") {
                // Parse timing line like: "Timing: 0x0p0 (0x0)		hfp:0  hs:0  hbp:0  vfp:0  vs:0  vbp:0"
                // Extract resolution from format like "0x0p0" (width x height)
                // or from parentheses like "(0x0)"
                let resolution = Self::parse_timing_resolution(timing_str.trim());
                timing_resolution = Some(resolution);
                debug!("Parsed timing resolution: {:?}", resolution);
            }
        }

        // If status line not found, try fallback
        let status = match plugin_status {
            Some(HdmiConnectionStatus::Connected) => {
                // If plugged in, check if resolution is 0 (no signal)
                match timing_resolution {
                    Some((0, 0)) => {
                        debug!("HDMI plugged in but resolution is 0x0, indicating no signal");
                        HdmiConnectionStatus::NoSignal
                    }
                    Some((w, h)) => {
                        debug!("HDMI plugged in with resolution: {}x{}", w, h);
                        HdmiConnectionStatus::Connected
                    }
                    None => {
                        // If timing not found but status is plugin, assume connected
                        warn!("Timing line not found in debugfs, assuming connected");
                        HdmiConnectionStatus::Connected
                    }
                }
            }
            Some(s) => s,
            None => {
                warn!("The status line is not found in debugfs, try to use audio_rate");
                return Rk628HpdDetector::read_status_fallback();
            }
        };

        debug!("Final HDMI status from debugfs: {:?}", status);
        Ok(status)
    }

    /// Parse timing resolution from timing line
    /// Format examples:
    /// - "0x0p0 (0x0)" -> (0, 0)
    /// - "1920x1080p60 (0x78)" -> (1920, 1080)
    /// - "3840x2160p30 (0x1e0)" -> (3840, 2160)
    fn parse_timing_resolution(timing_str: &str) -> (u32, u32) {
        // Try to parse format like "1920x1080p60" or "0x0p0"
        if let Some(pos) = timing_str.find('x') {
            let before_x = &timing_str[..pos];
            let after_x = &timing_str[pos + 1..];
            
            // Find the next 'p' character for progressive scan indicator
            if let Some(p_pos) = after_x.find('p') {
                let width_str = before_x.trim();
                let height_str = &after_x[..p_pos].trim();
                
                // Handle hex format like "0x0"
                let width = if width_str.starts_with("0x") {
                    u32::from_str_radix(&width_str[2..], 16).unwrap_or(0)
                } else {
                    width_str.parse().unwrap_or(0)
                };
                
                let height = if height_str.starts_with("0x") {
                    u32::from_str_radix(&height_str[2..], 16).unwrap_or(0)
                } else {
                    height_str.parse().unwrap_or(0)
                };
                
                return (width, height);
            }
        }
        
        // If parsing failed, return (0, 0) as no signal indicator
        (0, 0)
    }

    /// Read current HDMI connection status (fallback: using audio_rate)
    fn read_status_fallback() -> Result<HdmiConnectionStatus> {
        if !Path::new(RK628_SYSFS_AUDIO_RATE).exists() {
            return Ok(HdmiConnectionStatus::Unknown);
        }

        let content = fs::read_to_string(RK628_SYSFS_AUDIO_RATE)
            .with_context(|| format!("Failed to read RK628 sysfs audio_rate file"))?;

        let rate: u32 = content.trim().parse().unwrap_or(0);
        
        // audio_rate of 0 means disconnected, non-zero means connected
        let status = if rate == 0 {
            HdmiConnectionStatus::Disconnected
        } else {
            HdmiConnectionStatus::Connected
        };

        debug!("Read status from audio_rate: {:?} (rate={})", status, rate);
        Ok(status)
    }

    /// Start polling detection (use this method if poll() is not supported)
    /// 
    /// # Arguments
    /// * `interval` - Polling interval, recommended 1 second
    pub async fn start_polling(
        &self,
        interval: Duration,
    ) -> Result<tokio::task::JoinHandle<()>> {
        if self.running.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!("Detector is already running"));
        }

        self.running.store(true, Ordering::Release);
        let running = self.running.clone();
        let status_tx = self.status_tx.clone();

        let handle = tokio::spawn(async move {
            let mut last_status = HdmiConnectionStatus::Unknown;
            info!("RK628 HPD polling detection started, interval: {:?}", interval);
            while running.load(Ordering::Acquire) {
                match Rk628HpdDetector::read_status() {
                    Ok(status) => {
                        if status != last_status {
                            info!("HDMI connection status changed: {:?} -> {:?}", last_status, status);
                            last_status = status;

                            // Send status change notification
                            if let Err(e) = status_tx.send(status).await {
                                error!("Failed to send status change notification: {}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to read HDMI connection status: {}", e);
                    }
                }
                sleep(interval).await;
            }
            info!("RK628 HPD polling detection stopped");
        });

        Ok(handle)
    }

    /// Stop detection
    pub async fn stop(&self) {
        self.running.store(false, Ordering::Release);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Read the actual audio sample rate from RK628
/// 
/// # Returns
/// 
/// Returns the detected sample rate in Hz, or None if unable to read or device is disconnected
pub fn get_rk628_audio_rate() -> Option<u32> {
    if !Path::new(RK628_SYSFS_AUDIO_RATE).exists() {
        debug!("RK628 audio_rate file does not exist");
        return None;
    }

    match fs::read_to_string(RK628_SYSFS_AUDIO_RATE) {
        Ok(content) => {
            match content.trim().parse::<u32>() {
                Ok(rate) => {
                    if rate > 0 {
                        debug!("Detected RK628 audio rate: {} Hz", rate);
                        Some(rate)
                    } else {
                        debug!("RK628 audio_rate is 0 (disconnected)");
                        None
                    }
                }
                Err(e) => {
                    warn!("Failed to parse RK628 audio_rate: {}", e);
                    None
                }
            }
        }
        Err(e) => {
            warn!("Failed to read RK628 audio_rate: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_from_str() {
        assert_eq!(
            HdmiConnectionStatus::from_str("plugin"),
            HdmiConnectionStatus::Connected
        );
        assert_eq!(
            HdmiConnectionStatus::from_str("plugout"),
            HdmiConnectionStatus::Disconnected
        );
        assert_eq!(
            HdmiConnectionStatus::from_str("unknown"),
            HdmiConnectionStatus::Unknown
        );
    }

    #[test]
    fn test_status_is_connected() {
        assert!(HdmiConnectionStatus::Connected.is_connected());
        assert!(!HdmiConnectionStatus::Disconnected.is_connected());
        assert!(!HdmiConnectionStatus::NoSignal.is_connected());
        assert!(!HdmiConnectionStatus::Unknown.is_connected());
    }

    #[test]
    fn test_parse_timing_resolution() {
        // Test standard format
        assert_eq!(
            Rk628HpdDetector::parse_timing_resolution("1920x1080p60 (0x78)"),
            (1920, 1080)
        );
        assert_eq!(
            Rk628HpdDetector::parse_timing_resolution("3840x2160p30 (0x1e0)"),
            (3840, 2160)
        );
        
        // Test hex format (no signal case)
        assert_eq!(
            Rk628HpdDetector::parse_timing_resolution("0x0p0 (0x0)"),
            (0, 0)
        );
        
        // Test invalid format
        assert_eq!(
            Rk628HpdDetector::parse_timing_resolution("invalid"),
            (0, 0)
        );
    }
}

