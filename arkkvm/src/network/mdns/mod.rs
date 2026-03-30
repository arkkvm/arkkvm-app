//! Multicast DNS (mDNS) service discovery module
//!
//! Provides mDNS service discovery functionality, allowing devices to automatically
//! discover each other on the local network.
use anyhow::{Result, anyhow};
use config::{MdnsListenOptions, MdnsOptions};
use server::Mdns;
use tokio::sync::OnceCell;
use tracing::{info, warn};

use crate::{config as config_module, network};

pub mod config;
pub mod server;
pub mod utils;

lazy_static::lazy_static! {
    static ref MDNS: OnceCell<Mdns> = OnceCell::new();
}

/// Initialize mDNS service
pub async fn init_mdns() -> anyhow::Result<()> {
    // Get hostname from network configuration, fallback to "arkkvm"
    let config = config_module::get_config_manager();
    let network_config = config.get().await.network_config;
    let hostname = network_config.hostname.unwrap_or_default();
    let domain = network_config.domain.unwrap_or_default();

    // Only use the .local FQDN since normalize_hostname will add .local to hostname anyway
    let (hostname, fqdn) = match format_homename_fqdn(hostname.as_str(), domain.as_str()).await {
        Ok(fqdn) => fqdn,
        Err(e) => {
            warn!("failed to get hostname and fqdn, will not start mDNS service: {:?}", &e);
            return Err(e);
        }
    };

    // Determine IPv4/IPv6 availability based on network configuration
    // For now, enable both by default, but this could be improved by checking actual network state
    let (ipv4_enabled, ipv6_enabled) = format_mdns_mode(network_config.mdns_mode.as_str());

    let mdns = Mdns::new(MdnsOptions {
        // Only use FQDN to avoid duplicate registration
        // normalize_hostname will ensure .local suffix is present
        local_names: vec![hostname, fqdn.clone()],
        listen_options: MdnsListenOptions { ipv4: ipv4_enabled, ipv6: ipv6_enabled },
    })?;

    if let Err(e) = mdns.start().await {
        warn!("Failed to start mDNS service: {:?}, will retry when network is ready", e);
        // Don't fail initialization if mDNS can't start yet
    } else {
        // Store mDNS instance
        let _ = MDNS.set(mdns).map_err(|_| anyhow::anyhow!("Failed to initialize mDNS"));
        info!("mDNS service started with hostname: {}", fqdn.clone());
    }

    Ok(())
}

pub async fn update_mdns_options(hostname: &str, domain: &str, mdns_mode: &str) {
    info!("update_mdns_options: hostname: {}, domain: {}, mdns_mode: {}", hostname, domain, mdns_mode);

    let Some(mdns) = MDNS.get() else {
        warn!("mDNS service not initialized, will retry when network is ready");
        return;
    };

    let Ok((hostname, fqdn)) = format_homename_fqdn(hostname, domain).await else {
        warn!("failed to get hostname and fqnd, will not update mDNS options");
        return;
    };

    let (ipv4_enabled, ipv6_enabled) = format_mdns_mode(mdns_mode);

    if let Err(e) = mdns
        .update_option(MdnsOptions {
            local_names: vec![hostname, fqdn.clone()],
            listen_options: MdnsListenOptions { ipv4: ipv4_enabled, ipv6: ipv6_enabled },
        })
        .await
    {
        warn!("Failed to update mDNS options: {:?}", e);
    } else {
        info!(
            "mDNS options updated successfully for hostname: {}, mdns_mode: {}",
            &fqdn, mdns_mode
        );
    }
}

fn format_mdns_mode(mdns_mode: &str) -> (bool, bool) {
    match mdns_mode {
        "auto" => (true, true),
        "ipv4_only" => (true, false),
        "ipv6_only" => (false, true),
        _ => (false, false),
    }
}

async fn format_homename_fqdn(hostname: &str, domain: &str) -> Result<(String, String)> {
    let mut hostname = hostname.trim();
    if hostname.is_empty() {
        hostname = "arkkvm";
    }

    if !hostname.is_ascii() {
        return Err(anyhow!("the hostname is not ascii"));
    }

    let mut domain = domain.trim().to_owned();
    if domain.is_empty() {
        if let Some(dhcp_lease) = network::NetworkInterfaceState::default().get_dhcp_lease().await {
            domain = dhcp_lease.domain.unwrap_or_default();
        }
    }

    if domain.is_empty() {
        domain = "local".to_owned();
    }

    let fqdn = format!("{}.{}.", hostname, domain);
    if fqdn.is_ascii() {
        Ok((hostname.to_owned(), fqdn))
    } else {
        Err(anyhow!("the fqdn is not ascii"))
    }
}

