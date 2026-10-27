//! VLAN settings — apply immediately as committed (no staging/confirm window).

use anyhow::{Result, bail};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

use crate::cloud::manager::get_cloud_manager;
use crate::cloud::types::CloudConnectionState;
use crate::config::get_config_manager;
use crate::config::types::{
    IpV4Mod, IpV6Mod, NetworkConfig, VlanEndpointConfig, VlanSettings, VlanStaticIpConfig,
};
use crate::module::rtc_response_params::{
    GetVlanSettingsResponse, SetVlanSettingsResponse, VlanEndpointResponse, VlanRedirectEndpoint,
    VlanRedirectInfo, VlanSettingsResponse,
};
use crate::network::network_conf::{self, NETWORK_CONF, NETWORK_CONF_PENDING};
use crate::network::nwu_bridge;
use crate::network::{
    check_valid_ipv4, get_interface_network_state, NetworkInterfaceState, RpcIPv6Address,
};

const PARENT_IFACE: &str = "eth0";
/// Delay before tearing down cloud WS so setVlanSettings RPC can flush success/redirect.
const CLOUD_RECONNECT_AFTER_APPLY_MS: u64 = 500;
/// Bumped on every schedule so older reconnect tasks exit.
static CLOUD_WAN_RECONNECT_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlanRole {
    Primary,
    Secondary,
}

impl VlanRole {
    fn as_str(self) -> &'static str {
        match self {
            VlanRole::Primary => "primary",
            VlanRole::Secondary => "secondary",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "primary" => Ok(VlanRole::Primary),
            "secondary" => Ok(VlanRole::Secondary),
            _ => bail!("Invalid VLAN role: {s} (expected primary or secondary)"),
        }
    }
}

pub fn vlan_iface_name(vlan_id: u16) -> String {
    format!("{PARENT_IFACE}.{vlan_id}")
}

/// Open L3 interfaces derived from committed VLAN settings (for mDNS and similar services).
///
/// - VLAN off → `eth0`
/// - VLAN on → `eth0.{primary}` and optional `eth0.{secondary}`
/// - Falls back to `eth0` if VLAN is enabled but no vlan ids are present
pub async fn open_l3_ifaces() -> Vec<String> {
    let report = get_effective_vlan_config_report().await;
    if !report.vlan_enabled {
        return vec![PARENT_IFACE.to_string()];
    }

    let mut ifaces = Vec::new();
    if let Some(id) = report.primary_vlan_id {
        ifaces.push(vlan_iface_name(id));
    }
    if let Some(id) = report.secondary_vlan_id {
        ifaces.push(vlan_iface_name(id));
    }
    if ifaces.is_empty() {
        vec![PARENT_IFACE.to_string()]
    } else {
        ifaces
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VlanConfigReport {
    pub vlan_enabled: bool,
    #[serde(rename = "primaryVlanId", skip_serializing_if = "Option::is_none")]
    pub primary_vlan_id: Option<u16>,
    #[serde(rename = "secondaryVlanId", skip_serializing_if = "Option::is_none")]
    pub secondary_vlan_id: Option<u16>,
}

/// VLAN summary from committed network.conf (or encrypted config fallback).
pub async fn get_effective_vlan_config_report() -> VlanConfigReport {
    let settings = read_vlan_settings_from_conf().await;
    VlanConfigReport {
        vlan_enabled: settings.vlan_enabled,
        primary_vlan_id: settings.primary_vlan.as_ref().map(|v| v.vlan_id),
        secondary_vlan_id: settings.secondary_vlan.as_ref().map(|v| v.vlan_id),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VlanIpv6AddressesEntry {
    #[serde(rename = "vlanId")]
    pub vlan_id: u16,
    pub ipv6: Vec<RpcIPv6Address>,
}

pub async fn get_effective_vlan_ipv6_addresses_report() -> Vec<VlanIpv6AddressesEntry> {
    let settings = read_vlan_settings_from_conf().await;
    if !settings.vlan_enabled {
        return Vec::new();
    }

    let mut entries = Vec::new();
    if let Some(primary) = &settings.primary_vlan {
        entries.push(collect_vlan_ipv6_entry(primary.vlan_id).await);
    }
    if let Some(secondary) = &settings.secondary_vlan {
        entries.push(collect_vlan_ipv6_entry(secondary.vlan_id).await);
    }
    entries
}

async fn read_vlan_settings_from_conf() -> VlanSettings {
    if let Ok(cfg) = network_conf::read_network_conf(NETWORK_CONF).await {
        return cfg.vlan_settings;
    }
    get_committed_vlan_settings().await
}

async fn collect_vlan_ipv6_entry(vlan_id: u16) -> VlanIpv6AddressesEntry {
    let iface = vlan_iface_name(vlan_id);
    let ipv6 = NetworkInterfaceState::new(&iface)
        .get_ipv6_addresses()
        .await;
    VlanIpv6AddressesEntry { vlan_id, ipv6 }
}

pub async fn get_vlan_settings_response() -> Result<GetVlanSettingsResponse> {
    // Prefer on-disk network.conf (serial `ctl vlan disable` may have updated it).
    sync_encrypted_vlan_from_conf_if_needed().await?;

    let committed = get_committed_vlan_settings().await;
    let settings = enrich_vlan_settings(&committed).await;
    let redirect = if committed.vlan_enabled {
        build_vlan_redirect(&committed).await.ok()
    } else {
        None
    };

    Ok(GetVlanSettingsResponse {
        settings,
        redirect,
    })
}

/// Apply VLAN settings immediately as committed (no confirm window).
pub async fn set_vlan_settings(settings: VlanSettings) -> Result<SetVlanSettingsResponse> {
    validate_vlan_settings(&settings)?;

    if let Err(e) = apply_vlan_committed(&settings).await {
        tracing::error!(error = %e, "set_vlan_settings: apply failed");
        bail!("Failed to apply VLAN settings");
    }

    if let Err(e) = get_config_manager()
        .update(|config| {
            config.network_config.vlan_settings = settings.clone();
        })
        .await
    {
        tracing::error!(error = %e, "set_vlan_settings: save config failed");
        bail!("Failed to save VLAN settings");
    }

    let redirect = match build_vlan_redirect(&settings).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "set_vlan_settings: redirect build failed");
            bail!("VLAN applied, but redirect info is unavailable");
        }
    };

    schedule_cloud_reconnect_after_apply();
    crate::network::mdns::schedule_refresh_for_network_channels();

    Ok(SetVlanSettingsResponse { redirect })
}

fn schedule_cloud_reconnect_after_apply() {
    let poll_gen = CLOUD_WAN_RECONNECT_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(
            CLOUD_RECONNECT_AFTER_APPLY_MS,
        ))
        .await;
        if CLOUD_WAN_RECONNECT_GENERATION.load(Ordering::SeqCst) != poll_gen {
            return;
        }

        let state0 = get_cloud_manager().get_state();
        if matches!(state0, CloudConnectionState::NotConfigured) {
            info!("Skip cloud reconnect after VLAN apply — NotConfigured (unpaired)");
            return;
        }
        info!(
            ?state0,
            "VLAN/network applied — forcing cloud reconnect; connection loop retries until WAN ready"
        );
        // `force_reconnect` also releases the CloudManager reconnect gate, so
        // this stays correct even if a flat-network transition is in flight.
        get_cloud_manager().force_reconnect().await;
    });
}

/// Lab/cmd-socket: re-run `set_vlan_settings` with current committed VLAN config (ID unchanged).
pub async fn reapply_current_vlan_settings() -> Result<SetVlanSettingsResponse> {
    let settings = read_vlan_settings_from_conf().await;
    if !settings.vlan_enabled {
        bail!("VLAN mode is not enabled");
    }
    info!("REAPPLY_VLAN: re-applying committed VLAN settings via set_vlan_settings");
    set_vlan_settings(settings).await
}

/// Lab/cmd-socket: change primary VLAN id then apply (may break L2 reachability).
pub async fn set_primary_vlan_id_and_apply(vlan_id: u16) -> Result<SetVlanSettingsResponse> {
    if vlan_id == 0 || vlan_id > 4094 {
        bail!("VLAN ID must be between 1 and 4094");
    }
    let mut settings = read_vlan_settings_from_conf().await;
    if !settings.vlan_enabled {
        bail!("VLAN mode is not enabled");
    }
    let Some(primary) = settings.primary_vlan.as_mut() else {
        bail!("Primary VLAN is not configured");
    };
    if let Some(secondary) = &settings.secondary_vlan {
        if secondary.vlan_id == vlan_id {
            bail!("Primary and Secondary VLAN IDs must be different");
        }
    }
    info!(
        "SET_VLAN_PRIMARY_ID: {} -> {vlan_id}",
        primary.vlan_id
    );
    primary.vlan_id = vlan_id;
    set_vlan_settings(settings).await
}

/// Lab/cmd-socket: enable VLAN with primary DHCP (optional secondary DHCP).
/// Format consumed by cmd socket: `APPLY_VLAN_DHCP=6` or `APPLY_VLAN_DHCP=6,5`.
pub async fn apply_vlan_dhcp_lab(
    primary_id: u16,
    secondary_id: Option<u16>,
) -> Result<SetVlanSettingsResponse> {
    if primary_id == 0 || primary_id > 4094 {
        bail!("Primary VLAN ID must be between 1 and 4094");
    }
    if let Some(sid) = secondary_id {
        if sid == 0 || sid > 4094 {
            bail!("Secondary VLAN ID must be between 1 and 4094");
        }
        if sid == primary_id {
            bail!("Primary and Secondary VLAN IDs must be different");
        }
    }
    let dhcp_ep = |vlan_id: u16| VlanEndpointConfig {
        vlan_id,
        ipv4_mode: IpV4Mod::Dhcp,
        ipv6_mode: Some(IpV6Mod::Slaac),
        static_ipv4: None,
        static_ipv6: None,
    };
    let settings = VlanSettings {
        vlan_enabled: true,
        primary_vlan: Some(dhcp_ep(primary_id)),
        secondary_vlan: secondary_id.map(dhcp_ep),
    };
    info!(
        primary_id,
        secondary_id = ?secondary_id,
        "APPLY_VLAN_DHCP: enabling VLAN via set_vlan_settings"
    );
    set_vlan_settings(settings).await
}

/// Lab/cmd-socket: disable VLAN (flat eth0) via set_vlan_settings.
pub async fn apply_vlan_off_lab() -> Result<SetVlanSettingsResponse> {
    info!("APPLY_VLAN_OFF: disabling VLAN via set_vlan_settings");
    set_vlan_settings(VlanSettings {
        vlan_enabled: false,
        primary_vlan: None,
        secondary_vlan: None,
    })
    .await
}

pub async fn renew_vlan_dhcp_lease(role_str: &str) -> Result<()> {
    let role = VlanRole::from_str(role_str)?;
    let effective = read_vlan_settings_from_conf().await;

    if !effective.vlan_enabled {
        bail!("VLAN mode is not enabled");
    }

    let endpoint = match role {
        VlanRole::Primary => effective
            .primary_vlan
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Primary VLAN is not configured"))?,
        VlanRole::Secondary => effective
            .secondary_vlan
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Secondary VLAN is not configured"))?,
    };

    if endpoint.ipv4_mode != IpV4Mod::Dhcp {
        bail!(
            "{} VLAN uses static IPv4; DHCP lease renewal is not applicable",
            role.as_str()
        );
    }

    let iface = vlan_iface_name(endpoint.vlan_id);
    nwu_bridge::dhcp_renew(Some(&iface)).await
}

pub async fn init_vlan_settings(network_config: &NetworkConfig) -> Result<()> {
    if std::path::Path::new(NETWORK_CONF_PENDING).exists() {
        info!("Removing leftover {NETWORK_CONF_PENDING} from older firmware");
        network_conf::remove_pending().await?;
        nwu_bridge::vlan_cleanup_pending().await?;
    }

    sync_encrypted_vlan_from_conf_if_needed().await?;

    if network_config.vlan_settings.vlan_enabled {
        info!("VLAN enabled; network applied by network-config-unified at boot");
    }
    Ok(())
}

pub fn is_vlan_enabled_in_config(config: &NetworkConfig) -> bool {
    config.vlan_settings.vlan_enabled
}

pub fn validate_vlan_settings(settings: &VlanSettings) -> Result<()> {
    if !settings.vlan_enabled {
        return Ok(());
    }

    let primary = settings
        .primary_vlan
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Primary VLAN is required"))?;

    validate_vlan_endpoint(primary, "primary")?;

    if let Some(secondary) = &settings.secondary_vlan {
        validate_vlan_endpoint(secondary, "secondary")?;
        if secondary.vlan_id == primary.vlan_id {
            bail!("Primary and secondary VLAN IDs must differ");
        }
    }

    Ok(())
}

async fn apply_vlan_committed(settings: &VlanSettings) -> Result<()> {
    let mut config = get_config_manager().get().await.network_config.clone();
    config.vlan_settings = settings.clone();
    network_conf::write_network_conf(NETWORK_CONF, &config, "committed").await?;
    network_conf::remove_pending().await?;
    nwu_bridge::vlan_cleanup_pending().await?;
    nwu_bridge::refresh_committed(None).await?;
    Ok(())
}

/// If network.conf VLAN differs from encrypted config (e.g. serial ctl vlan disable), sync enc.
async fn sync_encrypted_vlan_from_conf_if_needed() -> Result<()> {
    let Ok(from_file) = network_conf::read_network_conf(NETWORK_CONF).await else {
        return Ok(());
    };
    let enc = get_config_manager()
        .get()
        .await
        .network_config
        .vlan_settings
        .clone();
    if from_file.vlan_settings == enc {
        return Ok(());
    }
    info!("Syncing encrypted vlan_settings from network.conf");
    get_config_manager()
        .update(|config| {
            config.network_config.vlan_settings = from_file.vlan_settings.clone();
            network_conf::merge_l3_from_file(&mut config.network_config, from_file.clone());
        })
        .await?;
    Ok(())
}

fn validate_vlan_endpoint(endpoint: &VlanEndpointConfig, label: &str) -> Result<()> {
    if endpoint.vlan_id == 0 || endpoint.vlan_id > 4094 {
        bail!("Invalid {label} VLAN ID (1-4094)");
    }

    match endpoint.ipv4_mode {
        IpV4Mod::Static => validate_static_ipv4(endpoint.static_ipv4.as_ref(), label)?,
        IpV4Mod::Dhcp => {}
    }

    Ok(())
}

fn validate_static_ipv4(static_ipv4: Option<&VlanStaticIpConfig>, label: &str) -> Result<()> {
    let Some(static_ipv4) = static_ipv4 else {
        bail!("Missing {label} static IP settings");
    };
    if !check_valid_ipv4(static_ipv4.ip_address.as_str()) {
        bail!("Invalid {label} IP address");
    }
    if !check_valid_ipv4(static_ipv4.subnet_mask.as_str()) {
        bail!("Invalid {label} subnet mask");
    }
    if !check_valid_ipv4(static_ipv4.gateway.as_str()) {
        bail!("Invalid {label} gateway");
    }
    for dns in &static_ipv4.dns_servers {
        if !check_valid_ipv4(dns.as_str()) {
            bail!("Invalid {label} DNS server");
        }
    }
    Ok(())
}

async fn enrich_vlan_settings(settings: &VlanSettings) -> VlanSettingsResponse {
    VlanSettingsResponse {
        vlan_enabled: settings.vlan_enabled,
        primary_vlan: match &settings.primary_vlan {
            Some(ep) => Some(enrich_endpoint(ep).await),
            None => None,
        },
        secondary_vlan: match &settings.secondary_vlan {
            Some(ep) => Some(enrich_endpoint(ep).await),
            None => None,
        },
    }
}

async fn enrich_endpoint(config: &VlanEndpointConfig) -> VlanEndpointResponse {
    let iface = vlan_iface_name(config.vlan_id);
    let state = get_interface_network_state(&iface).await;
    let dhcp_lease = if config.ipv4_mode == IpV4Mod::Dhcp {
        NetworkInterfaceState::new(&iface).get_dhcp_lease().await
    } else {
        None
    };

    VlanEndpointResponse {
        vlan_id: config.vlan_id,
        ipv4_mode: config.ipv4_mode.clone(),
        ipv6_mode: config.ipv6_mode.clone(),
        static_ipv4: config.static_ipv4.clone(),
        static_ipv6: config.static_ipv6.clone(),
        interface_name: Some(iface),
        ipv4: state.ipv4,
        ipv6: state.ipv6,
        ipv6_link_local: state.ipv6_link_local,
        ipv4_addresses: state.ipv4_addresses,
        ipv6_addresses: state.ipv6_addresses,
        dhcp_lease,
    }
}

async fn get_committed_vlan_settings() -> VlanSettings {
    get_config_manager()
        .get()
        .await
        .network_config
        .vlan_settings
        .clone()
}

async fn build_vlan_redirect(settings: &VlanSettings) -> Result<VlanRedirectInfo> {
    if !settings.vlan_enabled {
        return Ok(VlanRedirectInfo {
            primary_vlan: None,
            secondary_vlan: None,
        });
    }

    let primary = if let Some(ep) = &settings.primary_vlan {
        let iface = vlan_iface_name(ep.vlan_id);
        let state = get_interface_network_state(&iface).await;
        let ipv4 = state.ipv4.clone();
        // Config already committed; ipv4 may still be null until DHCP finishes.
        if ipv4.is_none() {
            info!(
                "Primary VLAN {iface} has no IPv4 yet (mode={:?}); redirect ipv4=null",
                ep.ipv4_mode
            );
        }
        Some(VlanRedirectEndpoint {
            vlan_id: ep.vlan_id,
            interface_name: iface,
            ipv4,
        })
    } else {
        None
    };

    let secondary = if let Some(ep) = &settings.secondary_vlan {
        let iface = vlan_iface_name(ep.vlan_id);
        let state = get_interface_network_state(&iface).await;
        Some(VlanRedirectEndpoint {
            vlan_id: ep.vlan_id,
            interface_name: iface,
            ipv4: state.ipv4,
        })
    } else {
        None
    };

    Ok(VlanRedirectInfo {
        primary_vlan: primary,
        secondary_vlan: secondary,
    })
}
