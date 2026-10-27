use std::env;
use std::str::FromStr;

use anyhow::Result;
use reqwest::Url;
use tracing::{info, warn};

use crate::cloud::manager::get_cloud_manager;
use crate::config::get_config_manager;
use crate::config::types::{IpV4Mod, IpV6Mod, NetworkConfig, VlanSettings};
use crate::network::network_conf::{self, NETWORK_CONF};
use crate::network::nwu_bridge;
use crate::network::{check_valid_ipv4, check_valid_ipv6, legacy_migrate, mdns, vlan};

const VAR_KEY_PROXY_HTTP: &str = "http_proxy";
const VAR_KEY_PROXY_HTTPS: &str = "https_proxy";

macro_rules! has_changed {
    ($old:expr, $default:expr, |$cfg:ident| $expr:expr) => {
        match $old {
            Some($cfg) => $expr,
            None => $default,
        }
    };
}

pub async fn init_network_settings() {
    if let Err(e) = legacy_migrate::maybe_migrate_legacy_network_config().await {
        warn!("Legacy network config migration: {e}");
    }

    let config = get_network_settings().await;
    // L3 network is applied by network-config-unified at boot; only proxy/mDNS here.
    if let Err(e) = vlan::init_vlan_settings(&config).await {
        warn!("Failed to initialize VLAN settings: {e}");
    }
    let _ = apply_non_l3_settings(None, &config).await;
}

pub async fn get_network_settings() -> NetworkConfig {
    let manager = get_config_manager();
    let mut config = manager.get().await.network_config.clone();
    if let Ok(from_file) = network_conf::read_network_conf(NETWORK_CONF).await {
        network_conf::merge_l3_from_file(&mut config, from_file);
    }
    config
}

pub async fn set_network_settings(settings: NetworkConfig) -> Result<()> {
    let mut settings = settings;
    if let Some(proxy) = settings.http_proxy.as_ref() {
        if proxy.is_empty() {
            settings.http_proxy = None;
        }
    }

    let manager = get_config_manager();
    let old_config = manager.get().await.network_config.clone();
    // Clients often omit vlan_settings (serde default = disabled). VLAN changes go through
    // setVlanSettings; preserve committed VLAN so hostname/domain edits do not tear it down.
    if settings.vlan_settings == VlanSettings::default()
        && old_config.vlan_settings != VlanSettings::default()
    {
        settings.vlan_settings = old_config.vlan_settings.clone();
    }
    if old_config != settings {
        on_config_changed(Some(&old_config), &settings).await?;

        manager
            .update(|config| {
                config.network_config = settings.clone();
            })
            .await?;
    }
    Ok(())
}

/// Restore factory network defaults: VLAN off, DHCP, clear static / proxy.
pub async fn reset_network_to_factory() -> Result<()> {
    let defaults = NetworkConfig::default();
    let old = get_config_manager().get().await.network_config.clone();

    // Suspend cloud reconnects until the refresh below has actually applied
    // the factory networking; reconnecting mid-refresh would bind the new WS
    // to the address that is about to be replaced (zombie connection).
    let transition = get_cloud_manager().begin_network_transition().await;
    let apply_result = (async {
        apply_non_l3_settings(Some(&old), &defaults).await?;
        network_conf::write_network_conf(NETWORK_CONF, &defaults, "committed").await?;
        network_conf::remove_pending().await?;
        nwu_bridge::vlan_cleanup_pending().await?;
        nwu_bridge::refresh_committed(None).await
    })
    .await;
    // Always release the gate, even on failure: the pre-existing network is
    // still in place then, and the cloud loop should reconnect onto it.
    get_cloud_manager().end_network_transition(transition).await;
    apply_result?;

    get_config_manager()
        .update(|config| {
            config.network_config = defaults;
        })
        .await?;

    mdns::schedule_refresh_for_network_channels();

    info!("Network restored to factory defaults (VLAN off, DHCP)");
    Ok(())
}

async fn on_config_changed(
    old_config: Option<&NetworkConfig>,
    new_config: &NetworkConfig,
) -> Result<()> {
    apply_non_l3_settings(old_config, new_config).await?;

    let hostname_domain_changed = has_changed!(old_config, true, |old| {
        old.hostname != new_config.hostname || old.domain != new_config.domain
    });

    // VLAN L3 is owned by setVlanSettings / boot refresh — do not re-apply flat
    // IPv4/IPv6 here. Hostname/domain must still update network.conf + /etc/hostname
    // so getNetworkSettings (merge_l3_from_file) and the UI do not snap back.
    if vlan::is_vlan_enabled_in_config(new_config) {
        if hostname_domain_changed {
            apply_hostname_domain_only(new_config).await?;
        }
        return Ok(());
    }

    let l3_changed = has_changed!(old_config, true, |old| {
        old.hostname != new_config.hostname
            || old.domain != new_config.domain
            || old.ipv4_mode != new_config.ipv4_mode
            || old.static_ipv4 != new_config.static_ipv4
            || old.ipv6_mode != new_config.ipv6_mode
            || old.static_ipv6 != new_config.static_ipv6
    });

    if l3_changed {
        apply_flat_network(new_config).await?;
    }

    Ok(())
}

/// Patch `[system] hostname/domain` in the on-disk conf and apply runtime
/// hostname without a full NWU refresh (preserves VLAN/DHCP).
async fn apply_hostname_domain_only(config: &NetworkConfig) -> Result<()> {
    let mut conf = match network_conf::read_network_conf(NETWORK_CONF).await {
        Ok(c) => c,
        Err(e) => {
            warn!("read {NETWORK_CONF} for hostname patch failed ({e}); writing from request");
            config.clone()
        }
    };
    conf.hostname = config.hostname.clone();
    conf.domain = config.domain.clone();

    network_conf::write_network_conf(NETWORK_CONF, &conf, "committed").await?;
    nwu_bridge::apply_hostname(
        config.hostname.as_deref().unwrap_or("arkkvm"),
        config.domain.as_deref().unwrap_or("dhcp"),
    )
    .await?;
    info!("Applied hostname/domain without disrupting VLAN L3");
    Ok(())
}

async fn apply_non_l3_settings(
    old_config: Option<&NetworkConfig>,
    new_config: &NetworkConfig,
) -> Result<()> {
    if has_changed!(old_config, true, |old| old.http_proxy != new_config.http_proxy) {
        update_proxy(&new_config.http_proxy)?;
    }

    if has_changed!(old_config, true, |old| {
        old.hostname != new_config.hostname
            || old.domain != new_config.domain
            || old.mdns_mode != new_config.mdns_mode
    }) {
        mdns::update_mdns_options(
            new_config.hostname.clone().unwrap_or_default().as_str(),
            new_config.domain.clone().unwrap_or_default().as_str(),
            new_config.mdns_mode.as_str(),
        )
        .await;

        if has_changed!(old_config, false, |old| old.hostname != new_config.hostname
            || old.domain != new_config.domain)
        {
            if let Err(e) = crate::web::reload_tls_config().await {
                warn!("Failed to reload TLS config after hostname change: {:?}", e);
            }
        }
    }
    Ok(())
}

async fn apply_flat_network(config: &NetworkConfig) -> Result<()> {
    validate_flat_ipv4(config)?;
    validate_flat_ipv6(config)?;

    // Suspend cloud reconnects until the refresh below has actually applied
    // the new L3 config; reconnecting mid-refresh would bind the new WS to
    // the address that is about to be replaced (zombie connection).
    let transition = get_cloud_manager().begin_network_transition().await;
    let apply_result = (async {
        network_conf::write_network_conf(NETWORK_CONF, config, "committed").await?;
        nwu_bridge::refresh_committed(None).await
    })
    .await;
    // Always release the gate, even on failure: the pre-existing network is
    // still in place then, and the cloud loop should reconnect onto it.
    get_cloud_manager().end_network_transition(transition).await;
    apply_result?;

    mdns::schedule_refresh_for_network_channels();
    Ok(())
}

fn validate_flat_ipv4(config: &NetworkConfig) -> Result<()> {
    if config.ipv4_mode != IpV4Mod::Static {
        return Ok(());
    }
    let Some(static_ipv4) = &config.static_ipv4 else {
        return Err(anyhow::anyhow!("Static IPv4 mode missing static IP configuration"));
    };
    if !check_valid_ipv4(static_ipv4.ip_address.as_str()) {
        return Err(anyhow::anyhow!("Invalid IPv4 address: {}", static_ipv4.ip_address));
    }
    if !check_valid_ipv4(static_ipv4.subnet_mask.as_str()) {
        return Err(anyhow::anyhow!("Invalid subnet mask: {}", static_ipv4.subnet_mask));
    }
    if !check_valid_ipv4(static_ipv4.gateway.as_str()) {
        return Err(anyhow::anyhow!("Invalid gateway: {}", static_ipv4.gateway));
    }
    for dns in &static_ipv4.dns_servers {
        if !check_valid_ipv4(dns.as_str()) {
            return Err(anyhow::anyhow!("Invalid DNS server: {dns}"));
        }
    }
    Ok(())
}

fn validate_flat_ipv6(config: &NetworkConfig) -> Result<()> {
    if config.ipv6_mode != IpV6Mod::Static {
        return Ok(());
    }
    let Some(static_ipv6) = &config.static_ipv6 else {
        return Err(anyhow::anyhow!("Static IPv6 mode missing static IP configuration"));
    };
    if !check_valid_ipv6(static_ipv6.ip_address.as_str()) {
        return Err(anyhow::anyhow!("Invalid IPv6 address: {}", static_ipv6.ip_address));
    }
    for dns in &static_ipv6.dns_servers {
        if !check_valid_ipv6(dns.as_str()) {
            return Err(anyhow::anyhow!("Invalid DNS server: {dns}"));
        }
    }
    Ok(())
}

fn update_proxy(proxy: &Option<String>) -> Result<()> {
    if let Some(proxy) = proxy.as_ref() {
        if Url::from_str(proxy).is_ok() {
            unsafe { env::set_var(VAR_KEY_PROXY_HTTP, proxy) };
            unsafe { env::set_var(VAR_KEY_PROXY_HTTPS, proxy) };
            info!("Setting proxy to {proxy}");
        } else {
            unsafe { env::remove_var(VAR_KEY_PROXY_HTTP) };
            unsafe { env::remove_var(VAR_KEY_PROXY_HTTPS) };
            warn!("Invalid proxy URL: {proxy}");
            return Err(anyhow::anyhow!("Invalid proxy URL: {proxy}"));
        }
    } else {
        unsafe { env::remove_var(VAR_KEY_PROXY_HTTP) };
        unsafe { env::remove_var(VAR_KEY_PROXY_HTTPS) };
        info!("Removing proxy");
    }
    Ok(())
}
