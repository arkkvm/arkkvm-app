//! Migrate legacy network config files into `network.conf`, apply refresh, then archive legacy.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::fs;
use tracing::{info, warn};

use crate::config::get_config_manager;
use crate::config::types::{IpV4Mod, IpV6Mod, NetworkConfig, StaticIpConfig};
use crate::network::network_conf::{self, NETWORK_CONF};
use crate::network::nwu_bridge;

pub const STATIC_IP_CONF: &str = "/userdata/arkkvm/static_ip.conf";
const LEGACY_ARCHIVE_DIR: &str = "/userdata/arkkvm/legacy-network";

/// Boot-time migration: encrypted `network_config` and/or `static_ip.conf` → `network.conf`.
///
/// Coalesces Enc + `static_ip.conf` into a single write and a single `refresh_committed`.
pub async fn maybe_migrate_legacy_network_config() -> Result<()> {
    let start = Instant::now();
    let enc_config = get_config_manager().get().await.network_config.clone();
    let conf_exists = Path::new(NETWORK_CONF).exists();
    let static_exists = Path::new(STATIC_IP_CONF).exists();
    let has_l3 = network_config_has_l3(&enc_config);

    let provisional_or_template = if conf_exists {
        is_provisional_or_default_template(NETWORK_CONF).await?
    } else {
        false
    };

    info!(
        conf_exists,
        provisional_or_template,
        static_exists,
        has_l3,
        "legacy network migrate start"
    );

    // Managed conf with nothing to merge — fast path (typical second boot after OTA).
    if conf_exists && !provisional_or_template && !static_exists {
        info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "legacy network migrate skipped (conf already managed)"
        );
        return Ok(());
    }

    // Missing / provisional / DHCP template → take Enc (L3 or defaults) as base.
    // Managed conf + leftover static_ip → read file then merge static.
    let mut config = if !conf_exists || provisional_or_template {
        enc_config
    } else {
        network_conf::read_network_conf(NETWORK_CONF)
            .await
            .unwrap_or(enc_config)
    };

    let mut need_write = !conf_exists || provisional_or_template;
    let mut archive_static = false;

    if static_exists {
        match parse_static_ip_conf(STATIC_IP_CONF).await {
            Ok(Some(static_cfg)) => {
                info!(
                    ip = %static_cfg.ip_address,
                    "merging static_ip.conf into network.conf"
                );
                config.ipv4_mode = IpV4Mod::Static;
                config.static_ipv4 = Some(static_cfg);
                need_write = true;
                archive_static = true;
            }
            Ok(None) => {
                warn!("static_ip.conf has no ip=; leaving file in place");
            }
            Err(e) => {
                warn!("static_ip.conf parse failed (kept legacy file): {e}");
            }
        }
    }

    if !need_write {
        info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "legacy network migrate skipped (no conf rewrite needed)"
        );
        return Ok(());
    }

    info!(
        has_l3,
        provisional_or_template,
        archive_static,
        "writing migrated network.conf"
    );
    network_conf::write_network_conf(NETWORK_CONF, &config, "committed").await?;

    let refresh_start = Instant::now();
    nwu_bridge::refresh_committed(None).await?;
    info!(
        elapsed_ms = refresh_start.elapsed().as_millis() as u64,
        "refresh_committed after migrate"
    );
    // Give link/DHCP a moment before WebRTC clients reconnect (#7).
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    if archive_static {
        archive_legacy_file(STATIC_IP_CONF, "static_ip.conf").await?;
    }

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        "Legacy network config migration completed"
    );
    Ok(())
}

/// Provisional (S16 deferred) or unmigrated DHCP example template.
async fn is_provisional_or_default_template(path: &str) -> Result<bool> {
    let content = fs::read_to_string(path).await?;
    if content.contains("provisional-template") {
        return Ok(true);
    }
    let has_dhcp = content.lines().any(|l| {
        let l = l.trim();
        l == "mode=dhcp" || l.starts_with("mode=dhcp ")
    });
    let migrated = content.contains("Migrated from") || content.contains("managed by arkkvm_app");
    Ok(has_dhcp && !migrated)
}

/// Real L3 that must override a DHCP placeholder — hostname alone is not enough
/// (defaults always set hostname=arkkvm).
fn network_config_has_l3(config: &NetworkConfig) -> bool {
    config.ipv4_mode == IpV4Mod::Static
        || config.vlan_settings.vlan_enabled
        || config.ipv6_mode != IpV6Mod::Slaac
}

async fn parse_static_ip_conf(path: &str) -> Result<Option<StaticIpConfig>> {
    let content = fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {path}"))?;

    let mut ip = None;
    let mut netmask = None;
    let mut gateway = None;
    let mut dns_servers = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.as_str() {
            "ip" => ip = Some(value.to_string()),
            "netmask" => netmask = Some(value.to_string()),
            "gateway" => gateway = Some(value.to_string()),
            "dns" => {
                dns_servers = value
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
            _ => {}
        }
    }

    let Some(ip_address) = ip else {
        return Ok(None);
    };

    Ok(Some(StaticIpConfig {
        ip_address,
        subnet_mask: netmask.unwrap_or_else(|| "255.255.255.0".to_string()),
        gateway: gateway.unwrap_or_default(),
        dns_servers,
    }))
}

async fn archive_legacy_file(src: &str, label: &str) -> Result<()> {
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    fs::create_dir_all(LEGACY_ARCHIVE_DIR).await.ok();
    let dest = format!("{LEGACY_ARCHIVE_DIR}/{label}.migrated.{ts}");
    fs::rename(src, &dest)
        .await
        .with_context(|| format!("Failed to archive {src} -> {dest}"))?;
    info!("Archived legacy config: {src} -> {dest}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_static_ip_content() {
        let dir = std::env::temp_dir().join("arkkvm_legacy_test");
        let path = dir.join("static_ip.conf");
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            &path,
            "ip=192.168.1.10\nnetmask=255.255.255.0\ngateway=192.168.1.1\ndns=8.8.8.8\n",
        )
        .await
        .unwrap();

        let cfg = parse_static_ip_conf(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cfg.ip_address, "192.168.1.10");
        assert_eq!(cfg.gateway, "192.168.1.1");
        assert_eq!(cfg.dns_servers, vec!["8.8.8.8"]);

        fs::remove_dir_all(&dir).await.ok();
    }

    #[test]
    fn has_l3_ignores_hostname_only() {
        let mut cfg = NetworkConfig::default();
        assert!(!network_config_has_l3(&cfg));
        cfg.hostname = Some("arkkvm".into());
        assert!(!network_config_has_l3(&cfg));
        cfg.ipv4_mode = IpV4Mod::Static;
        assert!(network_config_has_l3(&cfg));
    }

    #[tokio::test]
    async fn detects_provisional_marker() {
        let dir = std::env::temp_dir().join("arkkvm_legacy_provisional");
        let path = dir.join("network.conf");
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            &path,
            "# provisional-template\n[ipv4]\nmode=dhcp\n",
        )
        .await
        .unwrap();
        assert!(is_provisional_or_default_template(path.to_str().unwrap())
            .await
            .unwrap());
        fs::remove_dir_all(&dir).await.ok();
    }
}
