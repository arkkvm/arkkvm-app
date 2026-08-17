//! Multicast DNS (mDNS) service discovery module
//!
//! Provides mDNS service discovery functionality, allowing devices to automatically
//! discover each other on the local network.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use config::{MdnsListenOptions, MdnsOptions};
use server::Mdns;
use tokio::sync::OnceCell;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{config as config_module, network};

pub mod config;
pub mod server;
pub mod utils;

/// Delay before rebinding mDNS after L3/VLAN changes (DHCP / addresses may lag refresh).
const REFRESH_AFTER_NETWORK_MS: u64 = 500;
/// Bumped on every schedule so older refresh tasks exit.
static NETWORK_CHANNEL_REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);

lazy_static::lazy_static! {
    static ref MDNS: OnceCell<Mdns> = OnceCell::new();
    static ref MDNS_INIT_TASK_RUNNING: AtomicBool = AtomicBool::new(false);
}

/// Initialize mDNS service
pub async fn init_mdns() -> anyhow::Result<()> {
    if MDNS.get().is_some() {
        info!("mDNS service already initialized, skip init");
        return Ok(());
    }

    // Get hostname from network configuration, fallback to "arkkvm"
    let config = config_module::get_config_manager();
    let network_config = config.get().await.network_config;
    let hostname = network_config.hostname.unwrap_or_default();
    let domain = network_config.domain.clone().unwrap_or_default();

    // Only use the .local FQDN since normalize_hostname will add .local to hostname anyway
    let (hostname, fqdn) = match network::fqdn::build_hostname_and_fqdn(hostname.as_str(), domain.as_str()).await {
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
        local_names: vec![hostname],
        domain: domain.clone(),
        listen_options: MdnsListenOptions { ipv4: ipv4_enabled, ipv6: ipv6_enabled },
    })?;

    let has_running_retry_task = MDNS_INIT_TASK_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err();

    if has_running_retry_task {
        info!("mDNS init task already running, skip duplicate init");
        return Ok(());
    }

    tokio::spawn(async move {
        loop {
            if MDNS.get().is_some() {
                info!("mDNS service initialized by another task, stop retry loop");
                break;
            }

            match mdns.start().await {
                Ok(()) => {
                    if !requested_ip_family_ready(ipv4_enabled, ipv6_enabled).await {
                        warn!(
                            "mDNS started before required IP families were ready (ipv4_enabled={}, ipv6_enabled={}), retry in 1s",
                            ipv4_enabled, ipv6_enabled
                        );
                        if let Err(e) = mdns.stop().await {
                            warn!("Failed to stop early mDNS instance before retry: {:?}", e);
                        }
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }

                    if MDNS.set(mdns).is_err() {
                        warn!("mDNS service already initialized when retry task tried to set instance");
                    } else {
                        info!("mDNS service started with hostname: {}", fqdn);
                    }
                    break;
                }
                Err(e) => {
                    warn!(
                        "Failed to start mDNS service: {:?}; network may not be ready, retry in 1s",
                        e
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }

        MDNS_INIT_TASK_RUNNING.store(false, Ordering::SeqCst);
    });

    info!("mDNS initialization scheduled in background");

    Ok(())
}

pub async fn update_mdns_options(hostname: &str, domain: &str, mdns_mode: &str) {
    info!("update_mdns_options: hostname: {}, domain: {}, mdns_mode: {}", hostname, domain, mdns_mode);

    let Some(mdns) = MDNS.get() else {
        warn!("mDNS service not initialized yet, skip options update this round");
        return;
    };

    let Ok((hostname, fqdn)) = network::fqdn::build_hostname_and_fqdn(hostname, domain).await else {
        warn!("failed to get hostname and fqnd, will not update mDNS options");
        return;
    };

    let (ipv4_enabled, ipv6_enabled) = format_mdns_mode(mdns_mode);

    if let Err(e) = mdns
        .update_option(MdnsOptions {
            local_names: vec![hostname],
            domain: domain.to_owned(),
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

/// Restart mDNS so it rebinds to current open L3 channels (VLAN / flat).
pub async fn refresh_for_network_channels() {
    let Some(mdns) = MDNS.get() else {
        warn!("mDNS service not initialized yet, skip network-channel refresh");
        return;
    };

    let ifaces = network::vlan::open_l3_ifaces().await;
    match mdns.restart().await {
        Ok(()) => info!("mDNS refreshed for network channels: {:?}", ifaces),
        Err(e) => warn!("Failed to refresh mDNS for network channels: {:?}", e),
    }
}

/// Delayed mDNS rebind after VLAN/flat L3 changes (generation-cancelled like cloud reconnect).
pub fn schedule_refresh_for_network_channels() {
    let poll_gen = NETWORK_CHANNEL_REFRESH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::spawn(async move {
        sleep(Duration::from_millis(REFRESH_AFTER_NETWORK_MS)).await;
        if NETWORK_CHANNEL_REFRESH_GENERATION.load(Ordering::SeqCst) != poll_gen {
            return;
        }
        refresh_for_network_channels().await;
    });
}

fn format_mdns_mode(mdns_mode: &str) -> (bool, bool) {
    match mdns_mode {
        "auto" => (true, true),
        "ipv4_only" => (true, false),
        "ipv6_only" => (false, true),
        _ => (false, false),
    }
}

async fn requested_ip_family_ready(ipv4_enabled: bool, ipv6_enabled: bool) -> bool {
    let ifaces = network::vlan::open_l3_ifaces().await;
    let ipv4_ready = open_ifaces_have_ip_family(&ifaces, true);
    let ipv6_ready = open_ifaces_have_ip_family(&ifaces, false);

    match (ipv4_enabled, ipv6_enabled) {
        // auto mode: prefer IPv4 readiness and do not block on IPv6
        (true, true) => ipv4_ready,
        (true, false) => ipv4_ready,
        (false, true) => ipv6_ready,
        (false, false) => true,
    }
}

fn open_ifaces_have_ip_family(ifaces: &[String], want_v4: bool) -> bool {
    let Ok(all) = if_addrs::get_if_addrs() else {
        return false;
    };
    all.iter().any(|iface| {
        if iface.is_loopback() || !ifaces.iter().any(|n| n == &iface.name) {
            return false;
        }
        match &iface.addr {
            if_addrs::IfAddr::V4(addr) => want_v4 && !addr.ip.is_loopback(),
            if_addrs::IfAddr::V6(addr) => !want_v4 && !addr.ip.is_loopback(),
        }
    })
}

