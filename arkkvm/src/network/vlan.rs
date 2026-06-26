use std::sync::Arc;

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tracing::{info, warn};

use crate::config::get_config_manager;
use crate::config::types::{
    IpV4Mod, IpV6Mod, NetworkConfig, VlanEndpointConfig, VlanSettings, VlanStaticIpConfig,
};
use crate::module::rtc_response_params::{
    GetVlanSettingsResponse, PendingVlanSettings, SetVlanSettingsResponse, VlanEndpointResponse,
    VlanRedirectEndpoint, VlanRedirectInfo, VlanSettingsResponse,
};
use crate::network::static_ip_config::restart_network;
use chrono::Utc;

use crate::network::{
    check_valid_ipv4, get_interface_network_state, read_dhcp_lease_from_udhcpc_info,
    DhcpLease, NetworkInterfaceState, RpcIPv6Address, RpcNetworkState,
};

const PARENT_IFACE: &str = "eth0";
const CONFIRM_TIMEOUT_SECS: u64 = 90;
const DEFAULT_PRIMARY_VLAN_ID: u16 = 10;
const PRIMARY_ROUTE_METRIC: u32 = 100;
const INSMOD_PATH: &str = "/oem/usr/ko/insmod_ko.sh";
const UDHCPC_SCRIPT: &str = "/usr/share/udhcpc/default.script";
const DHCP_WAIT_TIMEOUT_SECS: u64 = 30;
const DHCP_POLL_INTERVAL_MS: u64 = 500;
const DHCP_RENEW_POLL_INTERVAL_SECS: u64 = 30;
/// Renew when remaining lease time is at or below lease/2 (DHCP T1).
const DHCP_RENEW_LEASE_FRACTION: u64 = 2;
const STAGING_UDHCPC_SCRIPT: &str = "/bin/true";

/// Saved eth0 L3 state so Staging can restore the original management path after VLAN ops.
#[derive(Clone, Debug)]
struct Eth0ConnectivitySnapshot {
    ipv4_cidrs: Vec<String>,
    default_route_args: Option<Vec<String>>,
    resolv_conf: Option<String>,
}

static VLAN_MANAGER: Lazy<Arc<Mutex<VlanManagerState>>> =
    Lazy::new(|| Arc::new(Mutex::new(VlanManagerState::default())));

static DHCP_RENEWAL: Lazy<Arc<Mutex<DhcpRenewalManager>>> =
    Lazy::new(|| Arc::new(Mutex::new(DhcpRenewalManager::default())));

#[derive(Debug, Default)]
struct DhcpRenewalManager {
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct VlanManagerState {
    pending: Option<VlanSettings>,
    rollback: Option<VlanSettings>,
    deadline: Option<Instant>,
    timeout_handle: Option<JoinHandle<()>>,
}

/// Staging: apply VLAN runtime config (VLAN interfaces + keep eth0 management path).
/// Used for setVlanSettings, init after reboot, and rollback.
/// Committed: only when VLAN is disabled — remove VLAN interfaces and restore untagged eth0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    Staging,
    Committed,
}

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

/// Minimal VLAN summary for GUI IPC: effective settings (pending when present).
#[derive(Debug, Clone, Serialize)]
pub struct VlanConfigReport {
    pub vlan_enabled: bool,
    #[serde(rename = "primaryVlanId", skip_serializing_if = "Option::is_none")]
    pub primary_vlan_id: Option<u16>,
    #[serde(rename = "secondaryVlanId", skip_serializing_if = "Option::is_none")]
    pub secondary_vlan_id: Option<u16>,
}

pub async fn get_effective_vlan_config_report() -> VlanConfigReport {
    let settings = get_effective_vlan_settings().await;
    VlanConfigReport {
        vlan_enabled: settings.vlan_enabled,
        primary_vlan_id: settings.primary_vlan.as_ref().map(|vlan| vlan.vlan_id),
        secondary_vlan_id: settings.secondary_vlan.as_ref().map(|vlan| vlan.vlan_id),
    }
}

/// Per-VLAN IPv6 addresses for GUI IPC (effective settings: pending when present).
#[derive(Debug, Clone, Serialize)]
pub struct VlanIpv6AddressesEntry {
    #[serde(rename = "vlanId")]
    pub vlan_id: u16,
    pub ipv6: Vec<RpcIPv6Address>,
}

pub async fn get_effective_vlan_ipv6_addresses_report() -> Vec<VlanIpv6AddressesEntry> {
    let settings = get_effective_vlan_settings().await;
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

async fn collect_vlan_ipv6_entry(vlan_id: u16) -> VlanIpv6AddressesEntry {
    let iface = vlan_iface_name(vlan_id);
    let ipv6 = NetworkInterfaceState::new(&iface)
        .get_ipv6_addresses()
        .await;
    VlanIpv6AddressesEntry { vlan_id, ipv6 }
}

pub async fn get_vlan_settings_response() -> Result<GetVlanSettingsResponse> {
    let committed = get_committed_vlan_settings().await;
    let settings = enrich_vlan_settings(&committed).await;

    let manager = VLAN_MANAGER.lock().await;
    let pending_settings = if let Some(pending) = manager.pending.clone() {
        Some(PendingVlanSettings {
            settings: enrich_vlan_settings(&pending).await,
            confirm_seconds_remaining: remaining_confirm_seconds(manager.deadline),
        })
    } else {
        None
    };
    let redirect = if let Some(pending) = &manager.pending {
        build_vlan_redirect(pending).await.ok()
    } else {
        None
    };

    Ok(GetVlanSettingsResponse {
        settings,
        pending_settings,
        redirect,
    })
}

pub async fn set_vlan_settings(mut settings: VlanSettings) -> Result<SetVlanSettingsResponse> {
    normalize_vlan_settings(&mut settings);
    validate_vlan_settings(&settings)?;

    let mut manager = VLAN_MANAGER.lock().await;
    if manager.pending.is_some() {
        bail!("VLAN settings change already pending confirmation");
    }

    let committed = get_committed_vlan_settings().await;
    manager.rollback = Some(committed.clone());

    let redirect = match async {
        apply_vlan_settings(&settings, ApplyMode::Staging).await?;
        build_vlan_redirect(&settings).await
    }
    .await
    {
        Ok(redirect) => redirect,
        Err(e) => {
            if let Err(revert_err) = revert_staged_vlan_settings(&committed).await {
                warn!("Failed to revert VLAN staging after error: {revert_err}");
            }
            manager.rollback = None;
            return Err(e);
        }
    };

    manager.pending = Some(settings);
    manager.deadline = Some(Instant::now() + Duration::from_secs(CONFIRM_TIMEOUT_SECS));
    start_timeout_task(&mut manager).await;

    Ok(SetVlanSettingsResponse {
        confirm_within_seconds: CONFIRM_TIMEOUT_SECS,
        redirect,
    })
}

pub async fn confirm_vlan_settings() -> Result<()> {
    let mut manager = VLAN_MANAGER.lock().await;
    let pending = manager
        .pending
        .take()
        .ok_or_else(|| anyhow::anyhow!("No pending VLAN settings to confirm"))?;

    cancel_timeout(&mut manager);
    manager.rollback = None;
    drop(manager);

    // Network was already applied at setVlanSettings; confirm only persists config.
    get_config_manager()
        .update(|config| {
            config.network_config.vlan_settings = pending;
        })
        .await?;

    Ok(())
}

pub async fn revert_vlan_settings() -> Result<()> {
    let mut manager = VLAN_MANAGER.lock().await;
    if manager.pending.is_none() {
        bail!("No pending VLAN settings to revert");
    }

    let rollback = manager
        .rollback
        .take()
        .unwrap_or_else(VlanSettings::default);

    cancel_timeout(&mut manager);
    manager.pending = None;

    drop(manager);

    revert_staged_vlan_settings(&rollback).await?;

    Ok(())
}

pub async fn renew_vlan_dhcp_lease(role_str: &str) -> Result<()> {
    let role = VlanRole::from_str(role_str)?;
    let effective = get_effective_vlan_settings().await;

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
    let is_primary = role == VlanRole::Primary;
    renew_vlan_dhcp_on_interface(&iface, is_primary, ApplyMode::Staging).await
}

pub async fn apply_vlan_settings(settings: &VlanSettings, mode: ApplyMode) -> Result<()> {
    let eth0_snap = if mode == ApplyMode::Staging {
        Some(capture_eth0_connectivity().await?)
    } else {
        None
    };

    let result = apply_vlan_settings_inner(settings, mode).await;

    if let Some(snap) = eth0_snap {
        if let Err(e) = restore_eth0_connectivity(&snap).await {
            warn!("Failed to restore {PARENT_IFACE} connectivity after staging: {e}");
        }
    }

    result
}

async fn apply_vlan_settings_inner(settings: &VlanSettings, mode: ApplyMode) -> Result<()> {
    if settings.vlan_enabled {
        ensure_kernel_modules().await?;
        prepare_vlan_interfaces_for_settings(settings).await?;

        let primary = settings
            .primary_vlan
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Primary VLAN is required when VLAN mode is enabled"))?;

        apply_vlan_endpoint(primary, true, mode).await?;

        let primary_iface = vlan_iface_name(primary.vlan_id);
        if primary.ipv4_mode == IpV4Mod::Dhcp {
            wait_for_vlan_ipv4(&primary_iface, mode).await?;
        }

        if let Some(secondary) = &settings.secondary_vlan {
            apply_vlan_endpoint(secondary, false, mode).await?;
            if secondary.ipv4_mode == IpV4Mod::Dhcp {
                let secondary_iface = vlan_iface_name(secondary.vlan_id);
                let _ = wait_for_vlan_ipv4(&secondary_iface, mode).await;
            }
        }

        if mode == ApplyMode::Staging {
            if let Some(snap) = capture_eth0_connectivity().await.ok() {
                let _ = restore_eth0_connectivity(&snap).await;
            }
        }
    } else if mode == ApplyMode::Committed {
        clear_all_vlans().await?;
        let network_config = get_config_manager().get().await.network_config.clone();
        restore_untagged_eth0(&network_config).await?;
    } else {
        clear_all_vlans().await?;
    }

    sync_dhcp_renewal_task(settings).await;

    Ok(())
}

pub async fn init_vlan_settings(network_config: &NetworkConfig) -> Result<()> {
    let mode = if network_config.vlan_settings.vlan_enabled {
        ApplyMode::Staging
    } else {
        ApplyMode::Committed
    };
    apply_vlan_settings(&network_config.vlan_settings, mode).await
}

pub fn is_vlan_enabled_in_config(config: &NetworkConfig) -> bool {
    config.vlan_settings.vlan_enabled
}

pub(crate) async fn restore_untagged_eth0(network_config: &NetworkConfig) -> Result<()> {
    use crate::network::static_ip_config::{remove_static_ipv4_config, update_static_ipv4_config, StaticIpConfigInfo};

    crate::cloud::manager::get_cloud_manager().reconnect_after_network_change().await;

    match network_config.ipv4_mode {
        IpV4Mod::Static => {
            let Some(static_ipv4) = &network_config.static_ipv4 else {
                bail!("Static IPv4 mode missing static IP configuration");
            };
            let mut config = StaticIpConfigInfo::new();
            config.with_ip(static_ipv4.ip_address.as_str());
            config.with_gateway(static_ipv4.gateway.as_str());
            config.with_netmask(static_ipv4.subnet_mask.as_str());
            if !static_ipv4.dns_servers.is_empty() {
                config.with_dns(static_ipv4.dns_servers.iter().map(|s| s.as_str()).collect());
            }
            update_static_ipv4_config(&config)
                .map_err(|e| anyhow::anyhow!("Failed to write static IP config: {e}"))?;
            restart_network();
        }
        IpV4Mod::Dhcp => {
            remove_static_ipv4_config();
            restart_network();
        }
    }
    Ok(())
}

fn normalize_vlan_settings(settings: &mut VlanSettings) {
    if !settings.vlan_enabled {
        return;
    }
    if let Some(primary) = settings.primary_vlan.as_mut() {
        if primary.vlan_id == 0 {
            primary.vlan_id = DEFAULT_PRIMARY_VLAN_ID;
        }
    }
}

pub fn validate_vlan_settings(settings: &VlanSettings) -> Result<()> {
    if !settings.vlan_enabled {
        return Ok(());
    }

    let primary = settings
        .primary_vlan
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Primary VLAN is required when VLAN mode is enabled"))?;

    validate_vlan_endpoint(primary, "Primary")?;

    if let Some(secondary) = &settings.secondary_vlan {
        validate_vlan_endpoint(secondary, "Secondary")?;
        if secondary.vlan_id == primary.vlan_id {
            bail!("Primary and Secondary VLAN IDs must be different");
        }
    }

    Ok(())
}

fn validate_vlan_endpoint(endpoint: &VlanEndpointConfig, label: &str) -> Result<()> {
    if endpoint.vlan_id == 0 || endpoint.vlan_id > 4094 {
        bail!("{label} VLAN ID must be between 1 and 4094");
    }

    match endpoint.ipv4_mode {
        IpV4Mod::Static => validate_static_ipv4(endpoint.static_ipv4.as_ref(), label)?,
        IpV4Mod::Dhcp => {}
    }

    if endpoint.ipv6_mode == Some(IpV6Mod::Static) {
        if endpoint.static_ipv6.is_none() {
            bail!("{label} static IPv6 mode requires static_ipv6 configuration");
        }
        warn!(
            "{label} VLAN static IPv6 is stored but not applied at runtime (SLAAC only)"
        );
    }

    Ok(())
}

fn validate_static_ipv4(static_ipv4: Option<&VlanStaticIpConfig>, label: &str) -> Result<()> {
    let Some(static_ipv4) = static_ipv4 else {
        bail!("{label} static IPv4 mode requires static_ipv4 configuration");
    };
    if !check_valid_ipv4(static_ipv4.ip_address.as_str()) {
        bail!("Invalid {label} IPv4 address: {}", static_ipv4.ip_address);
    }
    if !check_valid_ipv4(static_ipv4.subnet_mask.as_str()) {
        bail!("Invalid {label} subnet mask: {}", static_ipv4.subnet_mask);
    }
    if !check_valid_ipv4(static_ipv4.gateway.as_str()) {
        bail!("Invalid {label} gateway: {}", static_ipv4.gateway);
    }
    for dns in &static_ipv4.dns_servers {
        if !check_valid_ipv4(dns.as_str()) {
            bail!("Invalid {label} DNS server: {dns}");
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
        NetworkInterfaceState::new(&iface)
            .get_dhcp_lease_from_interface()
            .await
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
    get_config_manager().get().await.network_config.vlan_settings.clone()
}

async fn get_effective_vlan_settings() -> VlanSettings {
    let manager = VLAN_MANAGER.lock().await;
    if let Some(pending) = &manager.pending {
        return pending.clone();
    }
    drop(manager);
    get_committed_vlan_settings().await
}

async fn start_timeout_task(manager: &mut VlanManagerState) {
    abort_timeout_task(manager);
    let manager_arc = VLAN_MANAGER.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CONFIRM_TIMEOUT_SECS)).await;
        let mut state = manager_arc.lock().await;
        if state.pending.is_some() {
            info!("VLAN settings confirmation timed out; reverting");
            let rollback = state.rollback.take().unwrap_or_else(VlanSettings::default);
            state.pending = None;
            state.deadline = None;
            state.timeout_handle = None;
            drop(state);

            if let Err(e) = revert_staged_vlan_settings(&rollback).await {
                warn!("Failed to apply VLAN rollback after timeout: {e}");
            }
        }
    });
    manager.timeout_handle = Some(handle);
}

fn abort_timeout_task(manager: &mut VlanManagerState) {
    if let Some(handle) = manager.timeout_handle.take() {
        handle.abort();
    }
}

fn cancel_timeout(manager: &mut VlanManagerState) {
    abort_timeout_task(manager);
    manager.deadline = None;
}

fn remaining_confirm_seconds(deadline: Option<Instant>) -> u64 {
    let Some(deadline) = deadline else {
        return 0;
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let secs = remaining.as_secs();
    if remaining.subsec_nanos() > 0 {
        secs.saturating_add(1)
    } else {
        secs
    }
}

async fn ensure_kernel_modules() -> Result<()> {
    if vlan_module_loaded().await? {
        return Ok(());
    }
    let output = tokio::process::Command::new(INSMOD_PATH)
        .output()
        .await
        .with_context(|| format!("Failed to run {INSMOD_PATH}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("{INSMOD_PATH} exited with error (continuing): {stderr}");
    }
    Ok(())
}

async fn vlan_module_loaded() -> Result<bool> {
    let modules = tokio::fs::read_to_string("/proc/modules").await?;
    Ok(modules.lines().any(|line| line.starts_with("8021q")))
}

async fn prepare_vlan_interfaces_for_settings(settings: &VlanSettings) -> Result<()> {
    let mut keep = Vec::new();
    if let Some(primary) = &settings.primary_vlan {
        keep.push(primary.vlan_id);
    }
    if let Some(secondary) = &settings.secondary_vlan {
        keep.push(secondary.vlan_id);
    }
    for iface in list_vlan_interfaces().await? {
        if let Some(vid) = parse_vlan_id_from_iface(&iface) {
            if !keep.contains(&vid) {
                let _ = run_ip(&["link", "delete", &iface]).await;
            }
        }
    }
    Ok(())
}

async fn capture_eth0_connectivity() -> Result<Eth0ConnectivitySnapshot> {
    let ipv4_cidrs = parse_interface_ipv4_cidrs(PARENT_IFACE).await?;
    let default_route_args = parse_eth0_default_route_args().await?;
    let resolv_conf = tokio::fs::read_to_string("/etc/resolv.conf").await.ok();
    Ok(Eth0ConnectivitySnapshot {
        ipv4_cidrs,
        default_route_args,
        resolv_conf,
    })
}

async fn restore_eth0_connectivity(snap: &Eth0ConnectivitySnapshot) -> Result<()> {
    let current_cidrs = parse_interface_ipv4_cidrs(PARENT_IFACE).await.unwrap_or_default();
    for cidr in &snap.ipv4_cidrs {
        if !current_cidrs.contains(cidr) {
            let _ = run_ip(&["addr", "add", cidr, "dev", PARENT_IFACE]).await;
        }
    }

    if let Some(route_args) = &snap.default_route_args {
        if !eth0_has_default_route().await? {
            let refs: Vec<&str> = route_args.iter().map(String::as_str).collect();
            let _ = run_ip(&refs).await;
        }
    }

    if eth0_default_route_via_vlan_iface().await? {
        strip_all_vlan_default_routes().await?;
        if let Some(route_args) = &snap.default_route_args {
            let refs: Vec<&str> = route_args.iter().map(String::as_str).collect();
            let _ = run_ip(&refs).await;
        }
    }

    if let Some(expected) = &snap.resolv_conf {
        let current = tokio::fs::read_to_string("/etc/resolv.conf").await.ok();
        if current.as_ref() != Some(expected) {
            let _ = tokio::fs::write("/etc/resolv.conf", expected).await;
        }
    }

    Ok(())
}

async fn parse_interface_ipv4_cidrs(iface: &str) -> Result<Vec<String>> {
    let output = tokio::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .output()
        .await
        .with_context(|| format!("Failed to read IPv4 addresses on {iface}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut cidrs = Vec::new();
    for line in stdout.lines() {
        let Some(inet_idx) = line.split_whitespace().position(|p| p == "inet") else {
            continue;
        };
        let parts: Vec<_> = line.split_whitespace().collect();
        if let Some(cidr) = parts.get(inet_idx + 1) {
            cidrs.push(cidr.to_string());
        }
    }
    Ok(cidrs)
}

async fn parse_eth0_default_route_args() -> Result<Option<Vec<String>>> {
    let output = tokio::process::Command::new("ip")
        .args(["route", "show", "default", "dev", PARENT_IFACE])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(first_line) = stdout.lines().next() else {
        return Ok(None);
    };
    let line = first_line.trim().to_string();
    if line.is_empty() {
        return Ok(None);
    }
    parse_default_route_line_to_ip_args(&line)
}

fn parse_default_route_line_to_ip_args(line: &str) -> Result<Option<Vec<String>>> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.first().copied() != Some("default") {
        return Ok(None);
    }
    let mut args = vec!["route", "replace", "default"];
    let mut i = 1;
    while i < parts.len() {
        match parts[i] {
            "via" | "dev" | "metric" => {
                args.push(parts[i]);
                if let Some(v) = parts.get(i + 1) {
                    args.push(v);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(Some(args.iter().map(|s| s.to_string()).collect()))
}

async fn eth0_has_default_route() -> Result<bool> {
    let output = tokio::process::Command::new("ip")
        .args(["route", "show", "default", "dev", PARENT_IFACE])
        .output()
        .await?;
    Ok(output.status.success() && !output.stdout.is_empty())
}

async fn eth0_default_route_via_vlan_iface() -> Result<bool> {
    let output = tokio::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        line.contains(" dev ") && line.split(" dev ").nth(1).is_some_and(|rest| {
            rest.split_whitespace()
                .next()
                .is_some_and(|dev| dev.starts_with(&format!("{PARENT_IFACE}.")))
        })
    }))
}

async fn strip_all_vlan_default_routes() -> Result<()> {
    for iface in list_vlan_interfaces().await? {
        let _ = tokio::process::Command::new("ip")
            .args(["route", "del", "default", "dev", &iface])
            .output()
            .await;
    }
    Ok(())
}

async fn revert_staged_vlan_settings(rollback: &VlanSettings) -> Result<()> {
    clear_all_vlans().await?;
    if rollback.vlan_enabled {
        apply_vlan_settings(rollback, ApplyMode::Staging).await?;
    }
    Ok(())
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
        if ipv4.is_none() {
            bail!(
                "Primary VLAN {iface} has no IPv4 address; cannot provide redirect target"
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

    Ok(VlanRedirectInfo { primary_vlan: primary, secondary_vlan: secondary })
}

async fn apply_vlan_endpoint(
    endpoint: &VlanEndpointConfig,
    is_primary: bool,
    mode: ApplyMode,
) -> Result<()> {
    let iface = vlan_iface_name(endpoint.vlan_id);
    create_vlan_link(endpoint.vlan_id).await?;
    run_ip(&["link", "set", "dev", &iface, "up"]).await?;

    match endpoint.ipv4_mode {
        IpV4Mod::Dhcp => {
            stop_udhcpc_on_interface(&iface).await?;
            run_ip(&["addr", "flush", "dev", &iface]).await?;
            match mode {
                ApplyMode::Staging => run_udhcpc_staging(&iface).await?,
                ApplyMode::Committed => start_udhcpc_on_interface(&iface).await?,
            }
        }
        IpV4Mod::Static => {
            let static_ipv4 = endpoint
                .static_ipv4
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Static IPv4 configuration missing"))?;
            stop_udhcpc_on_interface(&iface).await?;
            let cidr = static_ipv4_to_cidr(static_ipv4)?;
            run_ip(&["addr", "flush", "dev", &iface]).await?;
            run_ip(&["addr", "add", &cidr, "dev", &iface]).await?;
            if is_primary && mode == ApplyMode::Committed {
                set_primary_default_route(&iface, &static_ipv4.gateway).await?;
                write_resolv_conf(&static_ipv4.dns_servers).await?;
            }
        }
    }

    if mode == ApplyMode::Staging {
        strip_all_vlan_default_routes().await?;
    }

    if endpoint.ipv6_mode == Some(IpV6Mod::Static) {
        warn!(
            "Static IPv6 on {iface} is not applied; interface relies on SLAAC when available"
        );
    }

    Ok(())
}

async fn create_vlan_link(vlan_id: u16) -> Result<()> {
    let iface = vlan_iface_name(vlan_id);
    if interface_exists(&iface).await? {
        return Ok(());
    }
    run_ip(&[
        "link",
        "add",
        "link",
        PARENT_IFACE,
        "name",
        &iface,
        "type",
        "vlan",
        "id",
        &vlan_id.to_string(),
    ])
    .await
}

async fn interface_exists(iface: &str) -> Result<bool> {
    let output = tokio::process::Command::new("ip")
        .args(["link", "show", "dev", iface])
        .output()
        .await
        .context("Failed to check interface existence")?;
    Ok(output.status.success())
}

async fn set_primary_default_route(iface: &str, gateway: &str) -> Result<()> {
    run_ip(&[
        "route",
        "replace",
        "default",
        "via",
        gateway,
        "dev",
        iface,
        "metric",
        &PRIMARY_ROUTE_METRIC.to_string(),
    ])
    .await
}

async fn write_resolv_conf(dns_servers: &[String]) -> Result<()> {
    if dns_servers.is_empty() {
        return Ok(());
    }
    let mut content = String::from("# Static DNS from ArkKVM VLAN primary\n");
    for dns in dns_servers {
        content.push_str(&format!("nameserver {dns}\n"));
    }
    tokio::fs::write("/etc/resolv.conf", content)
        .await
        .context("Failed to write /etc/resolv.conf")?;
    Ok(())
}

async fn start_udhcpc_on_interface(iface: &str) -> Result<()> {
    let output = tokio::process::Command::new("udhcpc")
        .args(["-i", iface, "-b", "-s", UDHCPC_SCRIPT])
        .output()
        .await
        .with_context(|| format!("Failed to start udhcpc on {iface}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("udhcpc failed on {iface}: {stderr}");
    }

    if let Ok(client_id) = eth0_mac_client_id().await {
        let opt = format!("0x3d:01{client_id}");
        let _ = tokio::process::Command::new("udhcpc")
            .args(["-i", iface, "-b", "-s", UDHCPC_SCRIPT, "-x", &opt])
            .output()
            .await;
    }

    Ok(())
}

/// Staging DHCP: obtain a lease without running the default script (no route/DNS side effects).
async fn run_udhcpc_staging(iface: &str) -> Result<()> {
    let output = tokio::process::Command::new("udhcpc")
        .args([
            "-i",
            iface,
            "-n",
            "-q",
            "-t",
            "10",
            "-T",
            "3",
            "-s",
            STAGING_UDHCPC_SCRIPT,
        ])
        .output()
        .await
        .with_context(|| format!("Failed to run staging udhcpc on {iface}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("staging udhcpc failed on {iface}: {stderr}");
    }
    apply_lease_to_interface(iface).await
}

async fn apply_lease_to_interface(iface: &str) -> Result<()> {
    if get_interface_network_state(iface).await.ipv4.is_some() {
        return Ok(());
    }

    let Some(lease) = read_dhcp_lease_from_udhcpc_info(iface).await else {
        return Ok(());
    };
    let Some(ip) = lease.ip else {
        return Ok(());
    };
    let cidr = if let Some(netmask) = lease.netmask {
        if netmask.contains('.') {
            format!("{}/{}", ip, netmask_to_prefix(&netmask)?)
        } else {
            format!("{}/{}", ip, netmask)
        }
    } else {
        bail!("DHCP lease on {iface} missing netmask");
    };
    run_ip(&["addr", "add", &cidr, "dev", iface]).await
}

async fn eth0_mac_client_id() -> Result<String> {
    let mac = tokio::fs::read_to_string(format!("/sys/class/net/{PARENT_IFACE}/address"))
        .await
        .context("Failed to read parent interface MAC")?;
    Ok(mac.trim().replace(':', ""))
}

async fn wait_for_vlan_ipv4(iface: &str, mode: ApplyMode) -> Result<()> {
    if mode == ApplyMode::Staging {
        if get_interface_network_state(iface).await.ipv4.is_some() {
            strip_all_vlan_default_routes().await?;
            info!(
                "Staging VLAN {iface} has IPv4 {:?}",
                get_interface_network_state(iface).await.ipv4
            );
            return Ok(());
        }
    }

    let deadline = Instant::now() + Duration::from_secs(DHCP_WAIT_TIMEOUT_SECS);
    while Instant::now() < deadline {
        let state = get_interface_network_state(iface).await;
        if state.ipv4.is_some() {
            match mode {
                ApplyMode::Committed => apply_primary_dhcp_routing(iface, &state).await?,
                ApplyMode::Staging => strip_all_vlan_default_routes().await?,
            }
            info!("DHCP on {iface} acquired IPv4 {:?}", state.ipv4);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(DHCP_POLL_INTERVAL_MS)).await;
    }
    bail!(
        "Timed out after {DHCP_WAIT_TIMEOUT_SECS}s waiting for DHCP on {iface}; \
         check switch Trunk/Hybrid and VLAN ID"
    );
}

async fn renew_vlan_dhcp_on_interface(iface: &str, is_primary: bool, mode: ApplyMode) -> Result<()> {
    stop_udhcpc_on_interface(iface).await?;
    run_ip(&["addr", "flush", "dev", iface]).await?;
    match mode {
        ApplyMode::Staging => run_udhcpc_staging(iface).await?,
        ApplyMode::Committed => start_udhcpc_on_interface(iface).await?,
    }
    if is_primary {
        wait_for_vlan_ipv4(iface, mode).await
    } else {
        wait_for_interface_ipv4(iface).await
    }
}

async fn apply_primary_dhcp_routing(iface: &str, _state: &RpcNetworkState) -> Result<()> {
    let Some(lease) = NetworkInterfaceState::new(iface)
        .get_dhcp_lease_from_interface()
        .await
    else {
        return Ok(());
    };
    if let Some(gw) = lease.routers.as_ref().and_then(|r| r.first()) {
        set_primary_default_route(iface, gw).await?;
    }
    if let Some(dns) = &lease.dns_servers {
        write_resolv_conf(dns).await?;
    }
    Ok(())
}

async fn stop_udhcpc_on_interface(iface: &str) -> Result<()> {
    let output = tokio::process::Command::new("ps")
        .output()
        .await
        .context("Failed to list processes")?;
    if !output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("udhcpc") && line.contains(iface) {
            if let Some(pid_str) = line.split_whitespace().next() {
                if let Ok(pid) = pid_str.parse::<i32>() {
                    let _ = tokio::process::Command::new("kill")
                        .arg(pid.to_string())
                        .output()
                        .await;
                }
            }
        }
    }
    Ok(())
}

async fn wait_for_interface_ipv4(iface: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(DHCP_WAIT_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if get_interface_network_state(iface).await.ipv4.is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(DHCP_POLL_INTERVAL_MS)).await;
    }
    bail!("Timed out after {DHCP_WAIT_TIMEOUT_SECS}s waiting for IPv4 on {iface}");
}

pub async fn clear_all_vlans() -> Result<()> {
    let interfaces = list_vlan_interfaces().await?;
    for iface in interfaces {
        let _ = run_ip(&["link", "delete", &iface]).await;
    }
    stop_dhcp_renewal_task().await;
    Ok(())
}

fn vlan_settings_use_dhcp(settings: &VlanSettings) -> bool {
    if !settings.vlan_enabled {
        return false;
    }
    let primary_dhcp = settings
        .primary_vlan
        .as_ref()
        .is_some_and(|ep| ep.ipv4_mode == IpV4Mod::Dhcp);
    let secondary_dhcp = settings
        .secondary_vlan
        .as_ref()
        .is_some_and(|ep| ep.ipv4_mode == IpV4Mod::Dhcp);
    primary_dhcp || secondary_dhcp
}

fn should_renew_lease(lease: &DhcpLease) -> bool {
    let Some(expiry) = lease.lease_expiry else {
        return false;
    };
    let Some(lease_secs) = lease.lease else {
        return false;
    };
    if lease_secs == 0 {
        return false;
    }

    let now = Utc::now();
    if now >= expiry {
        return true;
    }

    let renew_before = chrono::Duration::seconds((lease_secs / DHCP_RENEW_LEASE_FRACTION) as i64);
    now >= expiry - renew_before
}

async fn collect_dhcp_vlan_targets(settings: &VlanSettings) -> Vec<(String, bool)> {
    let mut targets = Vec::new();
    if let Some(primary) = &settings.primary_vlan {
        if primary.ipv4_mode == IpV4Mod::Dhcp {
            targets.push((vlan_iface_name(primary.vlan_id), true));
        }
    }
    if let Some(secondary) = &settings.secondary_vlan {
        if secondary.ipv4_mode == IpV4Mod::Dhcp {
            targets.push((vlan_iface_name(secondary.vlan_id), false));
        }
    }
    targets
}

async fn sync_dhcp_renewal_task(settings: &VlanSettings) {
    if vlan_settings_use_dhcp(settings) {
        ensure_dhcp_renewal_task().await;
    } else {
        stop_dhcp_renewal_task().await;
    }
}

async fn ensure_dhcp_renewal_task() {
    let mut manager = DHCP_RENEWAL.lock().await;
    if manager
        .handle
        .as_ref()
        .is_some_and(|handle| !handle.is_finished())
    {
        return;
    }

    let manager_arc = DHCP_RENEWAL.clone();
    manager.handle = Some(tokio::spawn(async move {
        dhcp_renewal_loop().await;
        let mut state = manager_arc.lock().await;
        state.handle = None;
    }));
}

async fn stop_dhcp_renewal_task() {
    let mut manager = DHCP_RENEWAL.lock().await;
    if let Some(handle) = manager.handle.take() {
        handle.abort();
    }
}

async fn dhcp_renewal_loop() {
    info!("VLAN DHCP lease renewal watcher started");
    loop {
        tokio::time::sleep(Duration::from_secs(DHCP_RENEW_POLL_INTERVAL_SECS)).await;

        let settings = get_effective_vlan_settings().await;
        if !vlan_settings_use_dhcp(&settings) {
            info!("VLAN DHCP lease renewal watcher stopping (no DHCP VLAN endpoints)");
            break;
        }

        for (iface, is_primary) in collect_dhcp_vlan_targets(&settings).await {
            if !interface_exists(&iface).await.unwrap_or(false) {
                continue;
            }

            let Some(lease) = read_dhcp_lease_from_udhcpc_info(&iface).await else {
                continue;
            };
            if !should_renew_lease(&lease) {
                continue;
            }

            info!(
                "Renewing DHCP lease on {iface} (expiry {:?})",
                lease.lease_expiry
            );
            if let Err(e) = renew_vlan_dhcp_on_interface(&iface, is_primary, ApplyMode::Staging).await
            {
                warn!("Failed to renew DHCP lease on {iface}: {e}");
            }
        }
    }
}

async fn list_vlan_interfaces() -> Result<Vec<String>> {
    let output = tokio::process::Command::new("ip")
        .args(["link", "show"])
        .output()
        .await
        .context("Failed to list network interfaces")?;
    if !output.status.success() {
        bail!("ip link show failed");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{PARENT_IFACE}.");
    let mut result = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.split(':').nth(1) {
            let name = rest.trim().split('@').next().unwrap_or("").trim();
            if name.starts_with(&prefix) {
                result.push(name.to_string());
            }
        }
    }
    Ok(result)
}

fn parse_vlan_id_from_iface(iface: &str) -> Option<u16> {
    iface
        .strip_prefix(&format!("{PARENT_IFACE}."))
        .and_then(|s| s.parse().ok())
}

fn static_ipv4_to_cidr(static_ipv4: &VlanStaticIpConfig) -> Result<String> {
    let prefix = netmask_to_prefix(&static_ipv4.subnet_mask)?;
    Ok(format!("{}/{}", static_ipv4.ip_address, prefix))
}

fn netmask_to_prefix(netmask: &str) -> Result<u8> {
    let octets: Vec<u8> = netmask
        .split('.')
        .map(|s| s.parse().map_err(|_| anyhow::anyhow!("Invalid netmask")))
        .collect::<Result<Vec<_>, _>>()?;
    if octets.len() != 4 {
        bail!("Invalid netmask: {netmask}");
    }
    let mut mask = 0u32;
    for o in octets {
        mask = (mask << 8) | u32::from(o);
    }
    let prefix = mask.count_ones() as u8;
    if prefix > 32 {
        bail!("Invalid netmask: {netmask}");
    }
    Ok(prefix)
}

async fn run_ip(args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("ip")
        .args(args)
        .output()
        .await
        .with_context(|| format!("Failed to run: ip {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ip {} failed: {stderr}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_confirm_seconds_ceil_subsecond_remainder() {
        let deadline = Instant::now() + Duration::from_millis(1500);
        assert_eq!(remaining_confirm_seconds(Some(deadline)), 2);
    }

    #[test]
    fn remaining_confirm_seconds_returns_zero_without_deadline() {
        assert_eq!(remaining_confirm_seconds(None), 0);
    }

    #[test]
    fn validate_rejects_duplicate_vlan_ids() {
        let settings = VlanSettings {
            vlan_enabled: true,
            primary_vlan: Some(VlanEndpointConfig {
                vlan_id: 10,
                ipv4_mode: IpV4Mod::Dhcp,
                ipv6_mode: None,
                static_ipv4: None,
                static_ipv6: None,
            }),
            secondary_vlan: Some(VlanEndpointConfig {
                vlan_id: 10,
                ipv4_mode: IpV4Mod::Dhcp,
                ipv6_mode: None,
                static_ipv4: None,
                static_ipv6: None,
            }),
        };
        assert!(validate_vlan_settings(&settings).is_err());
    }

    #[test]
    fn normalize_fills_default_primary_vlan_id() {
        let mut settings = VlanSettings {
            vlan_enabled: true,
            primary_vlan: Some(VlanEndpointConfig {
                vlan_id: 0,
                ipv4_mode: IpV4Mod::Dhcp,
                ipv6_mode: None,
                static_ipv4: None,
                static_ipv6: None,
            }),
            secondary_vlan: None,
        };
        normalize_vlan_settings(&mut settings);
        assert_eq!(settings.primary_vlan.unwrap().vlan_id, 10);
    }

    #[test]
    fn netmask_to_prefix_works() {
        assert_eq!(netmask_to_prefix("255.255.255.0").unwrap(), 24);
    }

    #[test]
    fn should_renew_lease_at_t1() {
        let lease = DhcpLease {
            ip: None,
            netmask: None,
            broadcast: None,
            ttl: None,
            mtu: None,
            hostname: None,
            domain: None,
            bootp_next_server: None,
            bootp_server_name: None,
            bootp_file: None,
            timezone: None,
            routers: None,
            dns_servers: None,
            ntp_servers: None,
            lpr_servers: None,
            _time_servers: None,
            _name_servers: None,
            _log_servers: None,
            _cookie_servers: None,
            _wins_servers: None,
            _swap_server: None,
            bootsize: None,
            root_path: None,
            lease: Some(3600),
            dhcp_type: None,
            server_id: None,
            reason: None,
            tftp: None,
            bootfile: None,
            uptime: None,
            lease_expiry: Some(Utc::now() + chrono::Duration::seconds(1700)),
            is_empty: None,
            ipv4_addresses: None,
        };
        assert!(should_renew_lease(&lease));
    }

    #[test]
    fn should_not_renew_lease_before_t1() {
        let lease = DhcpLease {
            ip: None,
            netmask: None,
            broadcast: None,
            ttl: None,
            mtu: None,
            hostname: None,
            domain: None,
            bootp_next_server: None,
            bootp_server_name: None,
            bootp_file: None,
            timezone: None,
            routers: None,
            dns_servers: None,
            ntp_servers: None,
            lpr_servers: None,
            _time_servers: None,
            _name_servers: None,
            _log_servers: None,
            _cookie_servers: None,
            _wins_servers: None,
            _swap_server: None,
            bootsize: None,
            root_path: None,
            lease: Some(3600),
            dhcp_type: None,
            server_id: None,
            reason: None,
            tftp: None,
            bootfile: None,
            uptime: None,
            lease_expiry: Some(Utc::now() + chrono::Duration::seconds(2000)),
            is_empty: None,
            ipv4_addresses: None,
        };
        assert!(!should_renew_lease(&lease));
    }

    #[test]
    fn should_renew_expired_lease() {
        let lease = DhcpLease {
            ip: None,
            netmask: None,
            broadcast: None,
            ttl: None,
            mtu: None,
            hostname: None,
            domain: None,
            bootp_next_server: None,
            bootp_server_name: None,
            bootp_file: None,
            timezone: None,
            routers: None,
            dns_servers: None,
            ntp_servers: None,
            lpr_servers: None,
            _time_servers: None,
            _name_servers: None,
            _log_servers: None,
            _cookie_servers: None,
            _wins_servers: None,
            _swap_server: None,
            bootsize: None,
            root_path: None,
            lease: Some(3600),
            dhcp_type: None,
            server_id: None,
            reason: None,
            tftp: None,
            bootfile: None,
            uptime: None,
            lease_expiry: Some(Utc::now() - chrono::Duration::seconds(10)),
            is_empty: None,
            ipv4_addresses: None,
        };
        assert!(should_renew_lease(&lease));
    }
}
