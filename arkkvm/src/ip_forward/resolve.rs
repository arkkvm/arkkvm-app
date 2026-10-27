//! Resolve forward destination: USB DHCP peer or explicit host.

use anyhow::{Result, bail};
use tracing::warn;

use crate::config::get_config_manager;
use crate::services::get_usb;

/// Stable error codes embedded in anyhow messages for MCP/agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpForwardError {
    UsbNetworkDisabled,
    UsbPeerNotLeased,
    ConnectFailed,
    TicketInvalid,
    ForwardBridgeFailed,
}

impl IpForwardError {
    pub fn code(self) -> &'static str {
        match self {
            Self::UsbNetworkDisabled => "usb_network_disabled",
            Self::UsbPeerNotLeased => "usb_peer_not_leased",
            Self::ConnectFailed => "connect_failed",
            Self::TicketInvalid => "ticket_invalid",
            Self::ForwardBridgeFailed => "forward_bridge_failed",
        }
    }
}

pub fn err_msg(code: IpForwardError, detail: impl AsRef<str>) -> String {
    format!("[{}] {}", code.code(), detail.as_ref())
}

/// Resolve destination host for `startIpForward`.
///
/// - `target == "usb"`: require USB network adapter enabled; pick controlled-host lease
///   (sole active, or longest-remaining among actives).
/// - otherwise: use non-empty `host`.
pub async fn resolve_forward_host(target: Option<&str>, host: Option<&str>) -> Result<String> {
    let target_usb = target.map(|t| t.eq_ignore_ascii_case("usb")).unwrap_or(false);
    let host = host.map(str::trim).filter(|h| !h.is_empty());

    if target_usb {
        if host.is_some() {
            bail!("[invalid_argument] target=usb must not include host; peer IP comes from USB DHCP");
        }
        return resolve_usb_peer().await;
    }

    match host {
        Some(h) => Ok(h.to_string()),
        None => bail!(
            "[invalid_argument] either target=usb or a non-empty host is required"
        ),
    }
}

async fn resolve_usb_peer() -> Result<String> {
    let config = get_config_manager();
    if !config.get_ssh_forwarding_enabled().await {
        bail!(
            "{}",
            err_msg(
                IpForwardError::UsbNetworkDisabled,
                "SSH Forwarding is not enabled; enable MCP + SSH Forwarding to use USB peer forwarding"
            )
        );
    }
    let devices = config.get_usb_devices().await;
    if !devices.network_adapter.enable {
        bail!(
            "{}",
            err_msg(
                IpForwardError::UsbNetworkDisabled,
                "USB network adapter is not enabled; enable SSH Forwarding to reach the HDMI-controlled host \
                 (or use startIpForward with an explicit host after user confirmation)"
            )
        );
    }

    let usb = get_usb().ok_or_else(|| {
        anyhow::anyhow!(err_msg(
            IpForwardError::ForwardBridgeFailed,
            "USB sidecar not available"
        ))
    })?;

    let leases = usb.get_network_adapter_leases().await.map_err(|e| {
        anyhow::anyhow!(err_msg(
            IpForwardError::ForwardBridgeFailed,
            format!("failed to query USB DHCP leases: {e}")
        ))
    })?;

    if leases.is_empty() {
        bail!(
            "{}",
            err_msg(
                IpForwardError::UsbPeerNotLeased,
                "no active USB DHCP lease; wait for the controlled host to obtain an address \
                 or enable/check USB network"
            )
        );
    }

    if leases.len() > 1 {
        warn!(
            count = leases.len(),
            "multiple active USB DHCP leases; selecting longest-remaining as controlled host"
        );
    }

    let selected = leases
        .iter()
        .max_by_key(|l| l.expires_in_secs)
        .and_then(|best| {
            // Tie-break: first in lease list order among max remaining.
            let max_rem = best.expires_in_secs;
            leases.iter().find(|l| l.expires_in_secs == max_rem)
        })
        .expect("leases non-empty");

    if selected.ip.trim().is_empty() {
        bail!(
            "{}",
            err_msg(
                IpForwardError::UsbPeerNotLeased,
                "USB DHCP lease has empty IP"
            )
        );
    }

    Ok(selected.ip.trim().to_string())
}
