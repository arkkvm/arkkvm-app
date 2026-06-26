use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;
use reqwest::Url;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::cloud::manager::get_cloud_manager;
use crate::config::get_config_manager;
use crate::config::types::{IpV4Mod, IpV6Mod, NetworkConfig, StaticIpConfig};
use crate::network::{check_valid_ipv4, check_valid_ipv6, mdns, vlan};
use crate::network::static_ip_config::{StaticIpConfigInfo, remove_static_ipv4_config, restart_network, update_static_ipv4_config};

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
    let config = get_network_settings().await;
    if config.vlan_settings.vlan_enabled {
        if let Err(e) = vlan::init_vlan_settings(&config).await {
            warn!("Failed to initialize VLAN settings: {e}");
        }
    } else {
        let _ = on_config_changed(None, &config).await;
    }
}

pub async fn get_network_settings() -> NetworkConfig {
    let manager = get_config_manager();
    manager.get().await.network_config.clone()
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
    if old_config != settings {
        on_config_changed(Some(&old_config), &settings).await?;

        manager
            .update(|config| {
                config.network_config = settings;
            })
            .await?;
    }
    Ok(())
}

async fn on_config_changed(
    old_config: Option<&NetworkConfig>,
    new_config: &NetworkConfig,
) -> Result<()> {
    // Set proxy
    if has_changed!(old_config, true, |old| old.http_proxy != new_config.http_proxy) {
        update_proxy(&new_config.http_proxy)?;
    }

    // Set mDNS
    if has_changed!(old_config, true, |old| old.hostname != new_config.hostname
        || old.domain != new_config.domain
        || old.mdns_mode != new_config.mdns_mode)
    {
        mdns::update_mdns_options(
            new_config.hostname.clone().unwrap_or_default().as_str(),
            new_config.domain.clone().unwrap_or_default().as_str(),
            new_config.mdns_mode.as_str(),
        )
        .await;
    }

    // Update IPv4 mode (skipped when VLAN mode manages interfaces)
    if !vlan::is_vlan_enabled_in_config(new_config)
        && has_changed!(old_config, false, |old| old.ipv4_mode != new_config.ipv4_mode
            || old.static_ipv4 != new_config.static_ipv4)
    {
        update_ipv4_mode(&new_config.ipv4_mode, &new_config.static_ipv4).await?;
    }

    // Update IPv6 mode
    if has_changed!(old_config, false, |old| old.ipv6_mode != new_config.ipv6_mode
        || old.static_ipv6 != new_config.static_ipv6)
    {
        update_ipv6_mode(&new_config.ipv6_mode, &new_config.static_ipv6).await?;
    }
    Ok(())
}

fn update_proxy(proxy: &Option<String>) -> Result<()> {
    if let Some(proxy) = proxy.as_ref() {
        if Url::from_str(proxy).is_ok() {
            unsafe { env::set_var(VAR_KEY_PROXY_HTTP, proxy) };
            unsafe { env::set_var(VAR_KEY_PROXY_HTTPS, proxy) };
            info!("Setting proxy to {}", proxy);
        } else {
            unsafe { env::remove_var(VAR_KEY_PROXY_HTTP) };
            unsafe { env::remove_var(VAR_KEY_PROXY_HTTPS) };
            warn!("Invalid proxy URL: {}", proxy);
            return Err(anyhow::anyhow!("Invalid proxy URL: {}", proxy));
        }
    } else {
        unsafe { env::remove_var(VAR_KEY_PROXY_HTTP) };
        unsafe { env::remove_var(VAR_KEY_PROXY_HTTPS) };
        info!("Removing proxy");
    }
    Ok(())
}

async fn update_ipv4_mode(ipv4_mode: &IpV4Mod, static_ipv4: &Option<StaticIpConfig>) -> Result<()> {
    info!("Updating IPv4 mod: {:?}, staticConfig: {:?}", ipv4_mode, static_ipv4);
    get_cloud_manager().reconnect_after_network_change().await;
    match ipv4_mode {
        IpV4Mod::Static => {
            let Some(static_ipv4) = static_ipv4 else {
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
            
            for dns in static_ipv4.dns_servers.iter() {
                if !check_valid_ipv4(dns.as_str()) {
                    return Err(anyhow::anyhow!("Invalid DNS server: {}", dns));
                }
            }
          
          
            // TODO: update static IPv4

            let  mut config = StaticIpConfigInfo::new();

            config.with_ip(static_ipv4.ip_address.as_str());
            config.with_gateway(static_ipv4.gateway.as_str());
            config.with_netmask(static_ipv4.subnet_mask.as_str());
            if(static_ipv4.dns_servers.len() > 0) {
                config.with_dns(static_ipv4.dns_servers.iter().map(|s| s.as_str()).collect()); 
            }

            let _ =   update_static_ipv4_config(&config);
            restart_network();

        }

        IpV4Mod::Dhcp => {
           
            // TODO: update DHCP IPv4
            let _ = remove_static_ipv4_config(); 
            restart_network();
        }
    }

    Ok(())
}

async fn update_ipv6_mode(ipv6_mode: &IpV6Mod, static_ipv6: &Option<StaticIpConfig>) -> Result<()> {
    info!("Updating IPv6 mod: {:?}, staticConfig: {:?}", ipv6_mode, static_ipv6);
    match ipv6_mode {
        IpV6Mod::Static => {
            let Some(static_ipv6) = static_ipv6 else {
                return Err(anyhow::anyhow!("Static IPv6 mode missing static IP configuration"));
            };

            if !check_valid_ipv6(static_ipv6.ip_address.as_str()) {
                return Err(anyhow::anyhow!("Invalid IPv6 address: {}", static_ipv6.ip_address));
            }

            if !check_valid_ipv6(static_ipv6.subnet_mask.as_str()) {
                return Err(anyhow::anyhow!("Invalid subnet mask: {}", static_ipv6.subnet_mask));
            }

            if !check_valid_ipv6(static_ipv6.gateway.as_str()) {
                return Err(anyhow::anyhow!("Invalid gateway: {}", static_ipv6.gateway));
            }

            for dns in static_ipv6.dns_servers.iter() {
                if !check_valid_ipv6(dns.as_str()) {
                    return Err(anyhow::anyhow!("Invalid DNS server: {}", dns));
                }
            }

            // TODO: update static IPv6
        }

        IpV6Mod::Slaac => {
            // TODO: update SLAAC IPv6
        }
    }

    Err(anyhow::anyhow!("cannot support to update IPv6 mode yet"))
}
