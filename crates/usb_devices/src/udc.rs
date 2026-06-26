use std::path::{Path, PathBuf};

use crate::events::UdcState;
use usb_gadget::UdcState as RawUdcState;

const UDC_SYSFS_ROOT: &str = "/sys/class/udc";

/// Path to `/sys/class/udc/<name>/state`.
pub fn udc_state_sysfs_path(udc_name: &str) -> PathBuf {
    PathBuf::from(UDC_SYSFS_ROOT).join(udc_name).join("state")
}

/// Read and parse UDC state from sysfs asynchronously.
pub async fn read_udc_state_sysfs(path: &Path) -> Result<RawUdcState, String> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("read UDC state failed: {}", e))?;
    Ok(raw.trim().parse().unwrap_or(RawUdcState::Unknown))
}

/// Map internal UDC enum to sysfs `/sys/class/udc/*/state` lowercase strings.
pub fn udc_state_to_sysfs(state: UdcState) -> &'static str {
    match state {
        UdcState::NotAttached => "not attached",
        UdcState::Attached => "attached",
        UdcState::Powered => "powered",
        UdcState::Default => "default",
        UdcState::Address => "addressed",
        UdcState::Configured => "configured",
        UdcState::Suspended => "suspended",
    }
}
