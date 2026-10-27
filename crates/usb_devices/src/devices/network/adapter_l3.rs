//! Board-side L3 on the USB gadget netdev (ip/mask/gw/dns).

use std::net::Ipv4Addr;

use tracing::{info, warn};

use crate::error::UsbError;
use crate::proto::v1::NetworkAdapterL3;

/// Board L3 settings applied to the USB netdev after bind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterL3Settings {
    pub ip_address: String,
    pub subnet_mask: String,
    pub gateway: Option<String>,
    pub dns_servers: Vec<String>,
}

impl From<&NetworkAdapterL3> for AdapterL3Settings {
    fn from(c: &NetworkAdapterL3) -> Self {
        Self {
            ip_address: c.ip_address.clone(),
            subnet_mask: c.subnet_mask.clone(),
            gateway: c.gateway.clone().filter(|g| !g.is_empty()),
            dns_servers: c.dns_servers.clone(),
        }
    }
}

pub fn ipv4_mask_to_prefix(mask: &str) -> Result<u8, String> {
    let parts: Vec<&str> = mask.trim().split('.').collect();
    if parts.len() != 4 {
        return Err("expected a.b.c.d".into());
    }
    let mut octets = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        octets[i] = p.parse::<u8>().map_err(|_| "octet".to_string())?;
    }
    let n = u32::from_be_bytes(octets);
    let ones = n.count_ones();
    if ones > 0 {
        let expected = (!0u32) << (32 - ones);
        if n != expected {
            return Err("non-contiguous mask".into());
        }
    }
    Ok(ones as u8)
}

pub async fn flush_and_down(ifname: &str) {
    let _ = flush_addrs(ifname).await;
    let _ = run_ip(&["link", "set", "dev", ifname, "down"]).await;
}

pub async fn flush_addrs(ifname: &str) -> Result<(), String> {
    let _ = run_ip(&["addr", "flush", "dev", ifname]).await;
    Ok(())
}

pub async fn run_ip(args: &[&str]) -> Result<(), String> {
    let out = tokio::process::Command::new("ip")
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "ip {:?} failed: status={:?} stdout={} stderr={}",
            args,
            out.status.code(),
            stdout,
            stderr
        ));
    }
    Ok(())
}

/// Apply board L3 addressing on `ifname` (does not start DHCP).
pub async fn apply_board_l3(ifname: &str, cfg: &AdapterL3Settings) -> Result<(), UsbError> {
    if ifname.starts_with("eth") {
        return Err(UsbError::InvalidArgument(format!(
            "refusing network adapter L3 on eth interface: {ifname}"
        )));
    }

    let prefix = ipv4_mask_to_prefix(&cfg.subnet_mask).map_err(|e| {
        UsbError::InvalidArgument(format!("invalid subnet_mask {:?}: {e}", cfg.subnet_mask))
    })?;
    cfg.ip_address.parse::<Ipv4Addr>().map_err(|_| {
        UsbError::InvalidArgument(format!("invalid ip_address {:?}", cfg.ip_address))
    })?;

    let _ = flush_addrs(ifname).await;
    run_ip(&[
        "addr",
        "add",
        &format!("{}/{}", cfg.ip_address, prefix),
        "dev",
        ifname,
    ])
    .await
    .map_err(|e| UsbError::IoError(format!("ip addr add on {ifname}: {e}")))?;
    run_ip(&["link", "set", "dev", ifname, "up"])
        .await
        .map_err(|e| UsbError::IoError(format!("ip link set {ifname} up: {e}")))?;

    if let Some(gw) = cfg.gateway.as_ref().filter(|g| !g.is_empty()) {
        gw.parse::<Ipv4Addr>().map_err(|_| {
            UsbError::InvalidArgument(format!("invalid gateway {gw:?}"))
        })?;
        if let Err(e) = run_ip(&[
            "route",
            "replace",
            "default",
            "via",
            gw,
            "dev",
            ifname,
            "metric",
            "500",
        ])
        .await
        {
            warn!(
                error = %e,
                gateway = %gw,
                ifname = %ifname,
                "network adapter gateway route failed"
            );
        }
    }

    if !cfg.dns_servers.is_empty() {
        warn!("network adapter dns_servers set but not applied to system resolv (board-side unused)");
    }

    info!(
        ifname = %ifname,
        ip = %cfg.ip_address,
        prefix = prefix,
        "network adapter board L3 applied"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_to_prefix_24() {
        assert_eq!(ipv4_mask_to_prefix("255.255.255.0").unwrap(), 24);
    }
}
