//! Read/write `/userdata/arkkvm/network.conf` (INI) — single L3 config source.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::fs;
use tracing::info;

use crate::config::types::{
    IpV4Mod, IpV6Mod, NetworkConfig, StaticIpConfig, VlanEndpointConfig, VlanSettings,
};

pub const NETWORK_CONF: &str = "/userdata/arkkvm/network.conf";
pub const NETWORK_CONF_PENDING: &str = "/userdata/arkkvm/network.conf.pending";

type Section = HashMap<String, String>;

/// Atomically write network.conf (tmp + rename).
pub async fn write_network_conf(
    path: &str,
    config: &NetworkConfig,
    vlan_apply_mode: &str,
) -> Result<()> {
    let content = network_config_to_ini(config, vlan_apply_mode);
    write_atomic(path, &content).await
}

pub async fn read_network_conf(path: &str) -> Result<NetworkConfig> {
    let content = fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {path}"))?;
    Ok(ini_to_network_config(&content))
}

/// Promote pending VLAN config to committed `network.conf`.
///
/// Rewrites with `apply_mode=committed` (do not bare-rename: staging pending
/// files contain `apply_mode=staging`, which would break boot refresh).
pub async fn promote_pending_to_committed() -> Result<()> {
    if !Path::new(NETWORK_CONF_PENDING).exists() {
        bail!("No pending network.conf to promote");
    }
    let config = read_network_conf(NETWORK_CONF_PENDING).await?;
    write_network_conf(NETWORK_CONF, &config, "committed").await?;
    remove_pending().await?;
    info!("Promoted {NETWORK_CONF_PENDING} -> {NETWORK_CONF} (apply_mode=committed)");
    Ok(())
}

pub async fn remove_pending() -> Result<()> {
    if Path::new(NETWORK_CONF_PENDING).exists() {
        fs::remove_file(NETWORK_CONF_PENDING).await.ok();
        info!("Removed {NETWORK_CONF_PENDING}");
    }
    Ok(())
}

/// Merge L3 fields from network.conf into an existing NetworkConfig (keeps proxy/mDNS/etc.).
pub fn merge_l3_from_file(base: &mut NetworkConfig, from_file: NetworkConfig) {
    if from_file.hostname.is_some() {
        base.hostname = from_file.hostname;
    }
    if from_file.domain.is_some() {
        base.domain = from_file.domain;
    }
    base.ipv4_mode = from_file.ipv4_mode;
    base.static_ipv4 = from_file.static_ipv4;
    base.ipv6_mode = from_file.ipv6_mode;
    base.static_ipv6 = from_file.static_ipv6;
    base.vlan_settings = from_file.vlan_settings;
}

async fn write_atomic(path: &str, content: &str) -> Result<()> {
    let parent = Path::new(path)
        .parent()
        .context("network.conf path has no parent")?;
    fs::create_dir_all(parent).await.ok();

    let tmp = format!("{path}.tmp.{}", std::process::id());
    fs::write(&tmp, content)
        .await
        .with_context(|| format!("Failed to write {tmp}"))?;
    fs::rename(&tmp, path)
        .await
        .with_context(|| format!("Failed to rename {tmp} -> {path}"))?;
    info!("Wrote {path}");
    Ok(())
}

fn parse_ini(content: &str) -> HashMap<String, Section> {
    let mut sections: HashMap<String, Section> = HashMap::new();
    let mut current = String::from("default");

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim().to_string();
            sections.entry(current.clone()).or_default();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            sections
                .entry(current.clone())
                .or_default()
                .insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    sections
}

fn section<'a>(sections: &'a HashMap<String, Section>, name: &str) -> &'a Section {
    static EMPTY: once_cell::sync::Lazy<Section> = once_cell::sync::Lazy::new(Section::new);
    sections.get(name).unwrap_or(&EMPTY)
}

fn get_str(sec: &Section, key: &str) -> Option<String> {
    sec.get(key)
        .filter(|v| !v.is_empty())
        .cloned()
}

fn get_bool(sec: &Section, key: &str, default: bool) -> bool {
    sec.get(key)
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes"))
        .unwrap_or(default)
}

fn parse_ipv4_mode(s: &str) -> IpV4Mod {
    match s {
        "static" => IpV4Mod::Static,
        _ => IpV4Mod::Dhcp,
    }
}

fn parse_ipv6_mode(s: &str) -> IpV6Mod {
    match s {
        "static" => IpV6Mod::Static,
        _ => IpV6Mod::Slaac,
    }
}

fn ipv4_mode_str(mode: &IpV4Mod) -> &'static str {
    match mode {
        IpV4Mod::Dhcp => "dhcp",
        IpV4Mod::Static => "static",
    }
}

fn ipv6_mode_str(mode: &IpV6Mod) -> &'static str {
    match mode {
        IpV6Mod::Slaac => "slaac",
        IpV6Mod::Static => "static",
    }
}

fn parse_static_ipv4(sec: &Section) -> Option<StaticIpConfig> {
    let ip = get_str(sec, "address")?;
    Some(StaticIpConfig {
        ip_address: ip,
        subnet_mask: get_str(sec, "netmask").unwrap_or_else(|| "255.255.255.0".to_string()),
        gateway: get_str(sec, "gateway").unwrap_or_default(),
        dns_servers: get_str(sec, "dns")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
    })
}

fn parse_vlan_endpoint(sec: &Section) -> Option<VlanEndpointConfig> {
    let id: u16 = sec.get("id")?.parse().ok()?;
    if id == 0 {
        return None;
    }
    let ipv4_mode = parse_ipv4_mode(sec.get("ipv4_mode").map(String::as_str).unwrap_or("dhcp"));
    let ipv6_mode = sec
        .get("ipv6_mode")
        .map(|s| parse_ipv6_mode(s.as_str()))
        .filter(|m| *m != IpV6Mod::Slaac || sec.get("ipv6_mode").is_some_and(|v| v != "disabled"));

    let static_ipv4 = if ipv4_mode == IpV4Mod::Static {
        Some(StaticIpConfig {
            ip_address: get_str(sec, "ipv4_address").unwrap_or_default(),
            subnet_mask: get_str(sec, "ipv4_netmask").unwrap_or_else(|| "255.255.255.0".to_string()),
            gateway: get_str(sec, "ipv4_gateway").unwrap_or_default(),
            dns_servers: get_str(sec, "ipv4_dns")
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
        })
    } else {
        None
    };

    Some(VlanEndpointConfig {
        vlan_id: id,
        ipv4_mode,
        ipv6_mode: Some(ipv6_mode.unwrap_or(IpV6Mod::Slaac)),
        static_ipv4,
        static_ipv6: None,
    })
}

pub fn ini_to_network_config(content: &str) -> NetworkConfig {
    let sections = parse_ini(content);
    let sys = section(&sections, "system");
    let ipv4 = section(&sections, "ipv4");
    let ipv6 = section(&sections, "ipv6");
    let vlan = section(&sections, "vlan");

    let ipv4_mode = parse_ipv4_mode(ipv4.get("mode").map(String::as_str).unwrap_or("dhcp"));
    let static_ipv4 = if ipv4_mode == IpV4Mod::Static {
        parse_static_ipv4(ipv4)
    } else {
        None
    };

    let ipv6_mode = parse_ipv6_mode(ipv6.get("mode").map(String::as_str).unwrap_or("slaac"));
    let static_ipv6 = if ipv6_mode == IpV6Mod::Static {
        get_str(ipv6, "prefix").map(|prefix| StaticIpConfig {
            ip_address: prefix,
            subnet_mask: String::new(),
            gateway: get_str(ipv6, "gateway").unwrap_or_default(),
            dns_servers: get_str(ipv6, "dns")
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
        })
    } else {
        None
    };

    let vlan_enabled = get_bool(vlan, "enabled", false);
    let vlan_settings = VlanSettings {
        vlan_enabled,
        primary_vlan: parse_vlan_endpoint(section(&sections, "vlan.primary")),
        secondary_vlan: parse_vlan_endpoint(section(&sections, "vlan.secondary")),
    };

    NetworkConfig {
        hostname: get_str(sys, "hostname"),
        http_proxy: None,
        domain: get_str(sys, "domain"),
        ipv4_mode,
        static_ipv4,
        ipv6_mode,
        static_ipv6,
        lldp_mode: "disabled".to_string(),
        lldp_tx_tlvs: Vec::new(),
        mdns_mode: "disabled".to_string(),
        time_sync_mode: "ntp".to_string(),
        time_sync_ordering: Vec::new(),
        time_sync_disable_fallback: false,
        time_sync_parallel: 1,
        vlan_settings,
    }
}

fn network_config_to_ini(config: &NetworkConfig, vlan_apply_mode: &str) -> String {
    let mut out = String::from(
        "# network.conf — managed by arkkvm_app\n\
         # Apply: refresh-network.sh\n\n",
    );

    out.push_str("[interface]\nname=eth0\n\n");

    out.push_str("[system]\n");
    out.push_str(&format!(
        "hostname={}\n",
        config.hostname.as_deref().unwrap_or("arkkvm")
    ));
    out.push_str(&format!(
        "domain={}\n\n",
        config.domain.as_deref().unwrap_or("dhcp")
    ));

    out.push_str("[dhcp]\nclient=udhcpc\n\n");

    out.push_str("[ipv4]\n");
    out.push_str(&format!("mode={}\n", ipv4_mode_str(&config.ipv4_mode)));
    if let Some(s) = &config.static_ipv4 {
        out.push_str(&format!("address={}\n", s.ip_address));
        out.push_str(&format!("netmask={}\n", s.subnet_mask));
        out.push_str(&format!("gateway={}\n", s.gateway));
        if !s.dns_servers.is_empty() {
            out.push_str(&format!("dns={}\n", s.dns_servers.join(" ")));
        }
    } else {
        out.push_str("address=\nnetmask=\ngateway=\ndns=\n");
    }
    out.push('\n');

    out.push_str("[ipv6]\n");
    out.push_str(&format!("mode={}\n", ipv6_mode_str(&config.ipv6_mode)));
    if let Some(s) = &config.static_ipv6 {
        out.push_str(&format!("prefix={}\n", s.ip_address));
        out.push_str(&format!("gateway={}\n", s.gateway));
        if !s.dns_servers.is_empty() {
            out.push_str(&format!("dns={}\n", s.dns_servers.join(" ")));
        }
    } else {
        out.push_str("prefix=\ngateway=\ndns=\n");
    }
    out.push('\n');

    out.push_str("[dns]\nservers=\n\n");

    out.push_str("[vlan]\n");
    out.push_str(&format!(
        "enabled={}\n",
        if config.vlan_settings.vlan_enabled {
            "true"
        } else {
            "false"
        }
    ));
    out.push_str("parent=eth0\n");
    out.push_str(&format!("apply_mode={vlan_apply_mode}\n\n"));

    write_vlan_endpoint_section(&mut out, "vlan.primary", config.vlan_settings.primary_vlan.as_ref(), 0);
    write_vlan_endpoint_section(
        &mut out,
        "vlan.secondary",
        config.vlan_settings.secondary_vlan.as_ref(),
        200,
    );

    out
}

fn write_vlan_endpoint_section(
    out: &mut String,
    section_name: &str,
    endpoint: Option<&VlanEndpointConfig>,
    default_metric: u32,
) {
    out.push_str(&format!("[{section_name}]\n"));
    let Some(ep) = endpoint else {
        out.push_str("id=0\nipv4_mode=disabled\nipv4_address=\nipv4_netmask=\nipv4_gateway=\nipv4_dns=\n");
        out.push_str("ipv6_mode=disabled\nipv6_prefix=\nipv6_gateway=\nipv6_dns=\n");
        out.push_str(&format!("route_metric={default_metric}\n\n"));
        return;
    };

    out.push_str(&format!("id={}\n", ep.vlan_id));
    out.push_str(&format!("ipv4_mode={}\n", ipv4_mode_str(&ep.ipv4_mode)));
    if let Some(s) = &ep.static_ipv4 {
        out.push_str(&format!("ipv4_address={}\n", s.ip_address));
        out.push_str(&format!("ipv4_netmask={}\n", s.subnet_mask));
        out.push_str(&format!("ipv4_gateway={}\n", s.gateway));
        if !s.dns_servers.is_empty() {
            out.push_str(&format!("ipv4_dns={}\n", s.dns_servers.join(" ")));
        } else {
            out.push_str("ipv4_dns=\n");
        }
    } else {
        out.push_str("ipv4_address=\nipv4_netmask=\nipv4_gateway=\nipv4_dns=\n");
    }
    let ipv6_mode = ep.ipv6_mode.as_ref().unwrap_or(&IpV6Mod::Slaac);
    out.push_str(&format!("ipv6_mode={}\n", ipv6_mode_str(ipv6_mode)));
    out.push_str("ipv6_prefix=\nipv6_gateway=\nipv6_dns=\n");
    out.push_str(&format!("route_metric={default_metric}\n\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_flat_dhcp() {
        let config = NetworkConfig {
            hostname: Some("arkkvm".to_string()),
            domain: Some("local".to_string()),
            ipv4_mode: IpV4Mod::Dhcp,
            ..Default::default()
        };
        let ini = network_config_to_ini(&config, "committed");
        let parsed = ini_to_network_config(&ini);
        assert_eq!(parsed.ipv4_mode, IpV4Mod::Dhcp);
        assert!(!parsed.vlan_settings.vlan_enabled);
    }
}
