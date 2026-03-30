use if_addrs::IfAddr;
use mac_address::mac_address_by_name;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::process::Command;
use chrono::{DateTime, Utc};
use tracing::{warn, info, error};
use std::collections::HashMap;
use chrono::Duration;
use anyhow::{Result, Context};

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::copy_nonoverlapping;
use std::collections::HashMap as StdHashMap;
use libc::{
    sockaddr_nl, nlmsghdr, nlattr,
    AF_NETLINK, SOCK_RAW, NETLINK_ROUTE,
    RTM_GETADDR,
    NLM_F_REQUEST, NLM_F_DUMP,
    NLMSG_NOOP, NLMSG_ERROR, NLMSG_DONE,
    AF_INET6,
};

pub mod settings;
pub mod ssh;
pub mod mdns;

static LINE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$").expect("invalid udhcpc info regex")
});

fn split_space(s: &str) -> Vec<String> {
    s.split_whitespace().map(|v| v.to_string()).collect()
}

fn de_space_vec_opt<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => {
            let vec = split_space(&s);
            if vec.is_empty() {
                Ok(None)
            } else {
                Ok(Some(vec))
            }
        }
        None => Ok(None),
    }
}

fn ser_space_vec_opt<S>(value: &Option<Vec<String>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(vec) => {
            if vec.is_empty() {
                serializer.serialize_str("")
            } else {
                serializer.serialize_str(&vec.join(" "))
            }
        }
        _ => serializer.serialize_none(),
    }
}

// Deserializer function that converts empty strings to None
fn de_opt_i32_from_str<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => s.trim().parse::<i32>().map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

fn de_opt_u64_from_str<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => s.trim().parse::<u64>().map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

fn de_opt_string_from_str<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => Ok(Some(s)),
        None => Ok(None),
    }
}

/// IPv6 address type
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum IPv6AddressType {
    /// Global unicast address - 2000::/3
    Global,
    /// Unique local address (ULA) - fc00::/7 (typically fd00::/8)
    UniqueLocal,
    /// Link-local address - fe80::/10
    LinkLocal,
    /// Loopback address - ::1
    Loopback,
    /// Unspecified address - ::
    Unspecified,
    /// Multicast address - ff00::/8
    Multicast,
    /// Unknown type
    Unknown,
}

/// Determine if a ULA address is stable
///
/// Criteria for stable addresses (in priority order):
/// 1. IFA_F_TEMPORARY flag (0x80) indicates temporary address, definitely not stable
/// 2. IFA_F_PERMANENT flag (0x02) indicates permanent address, definitely stable
/// 3. flags == 0 usually indicates default generated stable address (no special flags)
/// 4. Long lifetime (valid_lifetime > 7 days) usually indicates stable address
/// 5. Address format: simplified addresses (e.g., ::581) are usually primary addresses, more stable
pub fn is_stable_ula_address(
    ip_str: &str,
    flags: Option<u8>,
    valid_lifetime: Option<DateTime<Utc>>,
) -> bool {
    // Linux kernel flag definitions
    const IFA_F_PERMANENT: u8 = 0x02;  // Permanent address
    const IFA_F_TEMPORARY: u8 = 0x80;  // Temporary address (privacy address)

    // First check flags (most reliable method)
    if let Some(flags_val) = flags {
        // If IFA_F_TEMPORARY is set, definitely not stable
        if (flags_val & IFA_F_TEMPORARY) != 0 {
            return false;
        }
        // If IFA_F_PERMANENT is set, definitely stable
        if (flags_val & IFA_F_PERMANENT) != 0 {
            return true;
        }
        // flags == 0 usually indicates default generated stable address (no special flags)
        // This is the most common case for stable addresses
        if flags_val == 0 {
            return true;
        }
    }

    // If flags are not set, use other criteria
    // Check lifetime: if valid_lifetime > 7 days, usually considered stable
    if let Some(valid_until) = valid_lifetime {
        let now = Utc::now();
        if valid_until > now {
            let duration = valid_until - now;
            // 7 days = 604800 seconds
            if duration.num_seconds() > 604800 {
                return true;
            }
        }
    }

    // Check address format
    if let Ok(ip) = ip_str.parse::<std::net::Ipv6Addr>() {
        let octets = ip.octets();

        // Check if it's a simplified address (primary addresses are usually more stable)
        // If the last 64 bits (interface identifier) are small, it might be a primary address
        let interface_id = u64::from_be_bytes([
            octets[8], octets[9], octets[10], octets[11],
            octets[12], octets[13], octets[14], octets[15],
        ]);
        // If interface identifier is small (< 0x10000), might be a simplified primary address
        if interface_id < 0x10000 {
            return true;
        }

        // Check EUI-64 format: MAC address with FF:FE inserted in the middle
        // EUI-64 formatted addresses are usually stable
        if octets[11] == 0xff && octets[12] == 0xfe {
            return true;
        }
    }

    // Default: if flags is None, default to stable
    // Because in most cases, ULA addresses are stable by default
    flags.is_none()
}

/// Classify IPv6 address type
pub fn classify_ipv6_address(ip_str: &str) -> IPv6AddressType {
    // Parse IPv6 address
    let ip = match ip_str.parse::<std::net::Ipv6Addr>() {
        Ok(ip) => ip,
        Err(_) => return IPv6AddressType::Unknown,
    };

    // Get address byte array (16 bytes)
    let octets = ip.octets();
    let first_byte = octets[0];
    let second_byte = octets[1];

    // Check special addresses
    if ip.is_unspecified() {
        return IPv6AddressType::Unspecified;
    }
    if ip.is_loopback() {
        return IPv6AddressType::Loopback;
    }
    if ip.is_multicast() {
        return IPv6AddressType::Multicast;
    }

    // Check link-local address (fe80::/10)
    // fe80 = 1111 1110 1000 0000, first 10 bits are 1111 1110 10
    if first_byte == 0xfe && (second_byte & 0xc0) == 0x80 {
        return IPv6AddressType::LinkLocal;
    }

    // Check unique local address (fc00::/7)
    // fc00 = 1111 1100 0000 0000, first 7 bits are 1111 110
    // In practice, typically fd00::/8 (fd00 = 1111 1101 0000 0000)
    if (first_byte & 0xfe) == 0xfc {
        return IPv6AddressType::UniqueLocal;
    }

    // Check global unicast address (2000::/3)
    // 2000 = 0010 0000 0000 0000, first 3 bits are 001
    // Range: 2000:: to 3fff::
    if (first_byte & 0xe0) == 0x20 {
        return IPv6AddressType::Global;
    }

    IPv6AddressType::Unknown
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum IPv6AddressState {
    Preferred,
    Deprecated,
    Invalid,
}

fn ipv6_address_state(
    valid_lifetime: Option<DateTime<Utc>>,
    preferred_lifetime: Option<DateTime<Utc>>,
) -> Option<IPv6AddressState> {
    let (Some(valid), Some(pref)) = (valid_lifetime.as_ref(), preferred_lifetime.as_ref()) else {
        return None;
    };

    let now = Utc::now();

    if &now > valid {
        Some(IPv6AddressState::Invalid)
    }
    else if &now > pref {
        Some(IPv6AddressState::Deprecated)
    }
    else {
        Some(IPv6AddressState::Preferred)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RpcIPv6Address {
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_lifetime: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_lifetime: Option<DateTime<Utc>>,
    scope: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    address_type: Option<IPv6AddressType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_stable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<IPv6AddressState>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DhcpLease {
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub ip: Option<String>,
    #[serde(default, alias = "subnet", deserialize_with = "de_opt_string_from_str")]
    pub netmask: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub broadcast: Option<String>,
    #[serde(default, alias = "ipttl", deserialize_with = "de_opt_i32_from_str")]
    pub ttl: Option<i32>,
    #[serde(default, deserialize_with = "de_opt_i32_from_str")]
    pub mtu: Option<i32>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub hostname: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub domain: Option<String>,
    #[serde(default, alias = "siaddr", deserialize_with = "de_opt_string_from_str")]
    pub bootp_next_server: Option<String>,
    #[serde(default, alias = "sname", deserialize_with = "de_opt_string_from_str")]
    pub bootp_server_name: Option<String>,
    #[serde(default, alias = "boot_file", deserialize_with = "de_opt_string_from_str")]
    pub bootp_file: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub timezone: Option<String>,
    #[serde(default, alias = "router", deserialize_with = "de_space_vec_opt")]
    pub routers: Option<Vec<String>>,
    #[serde(default, alias = "dns", deserialize_with = "de_space_vec_opt")]
    pub dns_servers: Option<Vec<String>>,
    #[serde(default, alias = "ntpsrv", deserialize_with = "de_space_vec_opt")]
    pub ntp_servers: Option<Vec<String>>,
    #[serde(default, alias = "lprsvr", deserialize_with = "de_space_vec_opt")]
    pub lpr_servers: Option<Vec<String>>,
    #[serde(default, alias = "timesvr", deserialize_with = "de_space_vec_opt")]
    pub _time_servers: Option<Vec<String>>, // obsolete
    #[serde(default, alias = "namesvr", deserialize_with = "de_space_vec_opt")]
    pub _name_servers: Option<Vec<String>>, // obsolete
    #[serde(default, alias = "logsvr", deserialize_with = "de_space_vec_opt")]
    pub _log_servers: Option<Vec<String>>, // obsolete
    #[serde(default, alias = "cookiesvr", deserialize_with = "de_space_vec_opt")]
    pub _cookie_servers: Option<Vec<String>>, // obsolete
    #[serde(default, alias = "wins", deserialize_with = "de_space_vec_opt")]
    pub _wins_servers: Option<Vec<String>>,
    #[serde(default, alias = "swapsvr", deserialize_with = "de_opt_string_from_str")]
    pub _swap_server: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i32_from_str")]
    pub bootsize: Option<i32>,
    #[serde(default, alias = "rootpath", deserialize_with = "de_opt_string_from_str")]
    pub root_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_from_str")]
    pub lease: Option<u64>,
    #[serde(default, alias = "dhcptype", deserialize_with = "de_opt_string_from_str")]
    pub dhcp_type: Option<String>,
    #[serde(default, alias = "serverid", deserialize_with = "de_opt_string_from_str")]
    pub server_id: Option<String>,
    #[serde(default, alias = "message", deserialize_with = "de_opt_string_from_str")]
    pub reason: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub tftp: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string_from_str")]
    pub bootfile: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64_from_str")]
    pub uptime: Option<u64>,
    #[serde(default)]
    pub lease_expiry: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_empty: Option<HashMap<String, bool>>,
}

// Remove UdhcpcEnv, directly use DhcpLease to deserialize environment variables via alias
#[derive(Serialize, Deserialize, Debug)]
pub struct RpcNetworkState {
    interface_name: String,
    mac_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv6: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv6_link_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv4_addresses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv6_addresses: Option<Vec<RpcIPv6Address>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dhcp_lease: Option<DhcpLease>,
}

pub struct NetworkInterfaceState {
    interface_name: String,
}

impl Default for NetworkInterfaceState {
    fn default() -> Self {
        Self { interface_name: "eth0".to_owned() }
    }
}

impl NetworkInterfaceState {
    pub fn new(interface_name: &str) -> Self {
        NetworkInterfaceState {
            interface_name: interface_name.to_string(),
        }
    }

    pub async fn rpc_get_network_state(&self) -> RpcNetworkState {
        let ipv4_addresses = self.get_ipv4_addresses();
        let ipv6_addresses = self.get_ipv6_addresses().await;

        let ipv4 = self.get_ipv4_address();
        let ipv6 = self.get_ipv6_address();
        let ipv6_link_local = self.get_ipv6_link_local_address();

        RpcNetworkState {
            interface_name: self.interface_name.clone(),
            mac_address: self.mac_address(),
            ipv4,
            ipv6,
            ipv6_link_local,
            ipv4_addresses: if ipv4_addresses.is_empty() {
                None
            } else {
                Some(ipv4_addresses)
            },
            ipv6_addresses: if ipv6_addresses.is_empty() {
                None
            } else {
                Some(ipv6_addresses)
            },
            dhcp_lease: self.get_dhcp_lease().await,
        }
    }
}

impl NetworkInterfaceState {
    fn mac_address(&self) -> String {
        match mac_address_by_name(&self.interface_name) {
            Ok(Some(mac)) => format!("{}", mac),
            Ok(None) => String::new(),
            Err(_) => String::new(),
        }
    }

    fn get_ipv4_address(&self) -> Option<String> {
        let interfaces = if_addrs::get_if_addrs().ok()?;
        interfaces
            .into_iter()
            .filter(|iface| iface.name == self.interface_name)
            .filter_map(|iface| match iface.addr {
                IfAddr::V4(addr) => Some(addr.ip.to_string()),
                _ => None,
            })
            .next()
    }

    fn get_ipv6_address(&self) -> Option<String> {
        let interfaces = if_addrs::get_if_addrs().ok()?;
        interfaces
            .into_iter()
            .filter(|iface| iface.name == self.interface_name)
            .filter_map(|iface| match iface.addr {
                IfAddr::V6(addr) => {
                    let ip_str = addr.ip.to_string();
                    // Exclude link-local addresses as primary IPv6 address
                    if !ip_str.starts_with("fe80") {
                        Some(ip_str)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .next()
    }

    fn get_ipv6_link_local_address(&self) -> Option<String> {
        let interfaces = if_addrs::get_if_addrs().ok()?;
        interfaces
            .into_iter()
            .filter(|iface| iface.name == self.interface_name)
            .filter_map(|iface| match iface.addr {
                IfAddr::V6(addr) => {
                    let ip_str = addr.ip.to_string();
                    if ip_str.starts_with("fe80") {
                        Some(ip_str)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .next()
    }

    fn get_ipv4_addresses(&self) -> Vec<String> {
        let interfaces = if_addrs::get_if_addrs().unwrap_or(vec![]);
        interfaces
            .into_iter()
            .filter(|iface| iface.name == self.interface_name)
            .filter_map(|iface| match iface.addr {
                IfAddr::V4(addr) => Some(addr.ip.to_string()),
                _ => None,
            })
            .collect()
    }

    pub async fn get_ipv6_addresses(&self) -> Vec<RpcIPv6Address> {
        let interfaces = if_addrs::get_if_addrs().unwrap_or(vec![]);

        let enrich_map = query_ipv6_rtnetlink(&self.interface_name)
            .await
            .unwrap_or_else(|_| HashMap::new());

        interfaces
            .into_iter()
            .filter(|iface| iface.name == self.interface_name)
            .filter_map(|iface| match iface.addr {
                IfAddr::V6(addr) => {
                    let ip_str = addr.ip.to_string();

                    let (scope_opt, valid_opt, preferred_opt, flags_opt) = enrich_map
                        .get(&ip_str)
                        .cloned()
                        .unwrap_or((None, None, None, None));

                    let scope = scope_opt.unwrap_or_else(|| {
                        let is_link_local = ip_str.starts_with("fe80");
                        if is_link_local { 253 } else { 0 }
                    });

                    let address_type = classify_ipv6_address(&ip_str);

                    // Determine if address is stable
                    let is_stable = if address_type == IPv6AddressType::UniqueLocal {
                        Some(is_stable_ula_address(&ip_str, flags_opt, valid_opt))
                    } else {
                        None
                    };

                    let state = ipv6_address_state(valid_opt, preferred_opt);

                    Some(RpcIPv6Address {
                        address: ip_str,
                        valid_lifetime: valid_opt,
                        preferred_lifetime: preferred_opt,
                        scope,
                        address_type: Some(address_type),
                        flags: flags_opt,
                        is_stable,
                        state,
                    })
                }
                _ => None,
            })
            .collect()
    }

    pub async fn get_dhcp_lease(&self) -> Option<DhcpLease> {
        let info_path = format!("/run/udhcpc.{}.info", &self.interface_name);
        let content = match fs::read_to_string(&info_path).await {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    "Failed to read DHCP lease info from {}: {}",
                    info_path, err
                );
                return None;
            }
        };

        let parsed_pairs = parse_udhcpc_info(&content);
        if parsed_pairs.is_empty() {
            return None;
        }

        let mut lease: DhcpLease = match envy::from_iter(parsed_pairs) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Failed to parse DhcpLease from {}: {:?}",
                    info_path, e
                );
                return None
            },
        };

        if let Some(netmask) = lease.netmask.as_mut() {
            if let Ok(prefix) = netmask.parse::<u8>() {
                *netmask = cidr_to_netmask(prefix);
            }
        }

        // Check if at least one field exists
        let has_any = lease.ip.is_some()
            || lease.netmask.is_some()
            || lease.broadcast.is_some()
            || lease.ttl.is_some()
            || lease.mtu.is_some()
            || lease.hostname.is_some()
            || lease.domain.is_some()
            || lease.bootp_next_server.is_some()
            || lease.bootp_server_name.is_some()
            || lease.bootp_file.is_some()
            || lease.timezone.is_some()
            || lease.routers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease.dns_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease.ntp_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease.lpr_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._time_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._name_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._log_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._cookie_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._wins_servers.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || lease._swap_server.is_some()
            || lease.bootsize.is_some()
            || lease.root_path.is_some()
            || lease.lease.is_some()
            || lease.dhcp_type.is_some()
            || lease.server_id.is_some()
            || lease.reason.is_some()
            || lease.tftp.is_some()
            || lease.bootfile.is_some()
            || lease.uptime.is_some();

        if !has_any { return None; }

        // Calculate lease_expiry: prefer using uptime to derive time, then add lease
        if let Some(lease_secs) = lease.lease {
            let now = Utc::now();
            if let Some(obtain_uptime) = lease.uptime {
                if let Some(current_uptime) = get_system_uptime_seconds().await {
                    let elapsed = (current_uptime as i64) - (obtain_uptime as i64);
                    let obtained_time = now - Duration::seconds(elapsed.max(0));
                    lease.lease_expiry = Some(obtained_time + Duration::seconds(lease_secs as i64));
                } else {
                    lease.lease_expiry = Some(now + Duration::seconds(lease_secs as i64));
                }
            } else {
                lease.lease_expiry = Some(now + Duration::seconds(lease_secs as i64));
            }
        }

        // Build is_empty map
        lease.is_empty = Some(build_is_empty_map(&lease));

        Some(lease)
    }

    pub async fn renew_dhcp_lease(&self) -> Result<()> {
        info!("DHCP lease renewal requested for interface: {}", self.interface_name);

        let output = Command::new("udhcpc")
            .arg("-i")
            .arg(&self.interface_name)
            .arg("renew")
            .output()
            .await
            .context("Failed to execute udhcpc renew command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let err_msg = format!(
                "udhcpc renew failed for interface {}: {}",
                self.interface_name, stderr
            );
            error!("{}", err_msg);
            return Err(anyhow::anyhow!(err_msg));
        }

        info!("udhcpc renew command completed successfully for interface: {}", self.interface_name);
        Ok(())
    }

    /// Get global IPv6 address
    pub async fn get_global_ipv6_address(&self) -> Option<String> {
        let addresses = self.get_ipv6_addresses().await;
        addresses
            .into_iter()
            .find(|addr| addr.address_type == Some(IPv6AddressType::Global))
            .map(|addr| addr.address)
    }

    /// Get unique local IPv6 address (ULA)
    pub async fn get_unique_local_ipv6_address(&self) -> Option<String> {
        let addresses = self.get_ipv6_addresses().await;
        addresses
            .into_iter()
            .find(|addr| addr.address_type == Some(IPv6AddressType::UniqueLocal))
            .map(|addr| addr.address)
    }

    /// Get stable unique local IPv6 address (ULA)
    ///
    /// Stable addresses are usually based on EUI-64 format, have long lifetime,
    /// and are suitable as fixed device identifiers
    pub async fn get_stable_unique_local_ipv6_address(&self) -> Option<String> {
        let addresses = self.get_ipv6_addresses().await;
        addresses
            .into_iter()
            .find(|addr| {
                addr.address_type == Some(IPv6AddressType::UniqueLocal)
                    && addr.is_stable == Some(true)
            })
            .map(|addr| addr.address)
    }

    /// Get all stable ULA addresses
    pub async fn get_stable_unique_local_ipv6_addresses(&self) -> Vec<String> {
        let addresses = self.get_ipv6_addresses().await;
        addresses
            .into_iter()
            .filter(|addr| {
                addr.address_type == Some(IPv6AddressType::UniqueLocal)
                    && addr.is_stable == Some(true)
            })
            .map(|addr| addr.address)
            .collect()
    }

    /// Get all IPv6 addresses of specified type
    pub async fn get_ipv6_addresses_by_type(&self, addr_type: IPv6AddressType) -> Vec<String> {
        let addresses = self.get_ipv6_addresses().await;
        addresses
            .into_iter()
            .filter(|addr| addr.address_type == Some(addr_type))
            .map(|addr| addr.address)
            .collect()
    }
}

fn parse_udhcpc_info(content: &str) -> Vec<(String, String)> {
    content.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let captures = LINE_REGEX.captures(line)?;
        let key = captures.get(1)?.as_str();
        let raw_value = captures.get(2).map(|m| m.as_str().trim()).unwrap_or("");

        if key.is_empty() {
            return None;
        }

        let value = if raw_value.len() >= 2
            && ((raw_value.starts_with('"') && raw_value.ends_with('"'))
                || (raw_value.starts_with('\'') && raw_value.ends_with('\'')))
        {
            raw_value[1..raw_value.len() - 1].to_string()
        } else {
            raw_value.to_string()
        };

        Some((key.to_lowercase(), value))
    }).collect()
}

fn cidr_to_netmask(prefix: u8) -> String {
    // CIDR prefix length range is 0-32
    let prefix = prefix.min(32);
    let mask: u32 = if prefix == 0 { 0 } else { (!0u32) << (32 - (prefix as u32)) };
    let b1 = ((mask >> 24) & 0xFF) as u8;
    let b2 = ((mask >> 16) & 0xFF) as u8;
    let b3 = ((mask >> 8) & 0xFF) as u8;
    let b4 = (mask & 0xFF) as u8;
    format!("{}.{}.{}.{}", b1, b2, b3, b4)
}

async fn get_system_uptime_seconds() -> Option<f64> {
    if let Ok(content) = fs::read_to_string("/proc/uptime").await {
        let mut parts = content.split_whitespace();
        if let Some(first) = parts.next() {
            if let Ok(val) = first.parse::<f64>() {
                return Some(val);
            }
        }
    }
    None
}

fn build_is_empty_map(lease: &DhcpLease) -> HashMap<String, bool> {
    let mut m = HashMap::new();
    // Simple rule: None or empty Vec is considered true
    let is_none = |opt: &Option<String>| opt.as_ref().map(|s| s.is_empty()).unwrap_or(true);
    let is_none_u64 = |opt: &Option<u64>| opt.is_none();
    let is_none_i32 = |opt: &Option<i32>| opt.is_none();
    let is_none_vec = |opt: &Option<Vec<String>>| opt.as_ref().map(|v| v.is_empty()).unwrap_or(true);
    let is_none_dt = |opt: &Option<DateTime<Utc>>| opt.is_none();

    m.insert("ip".to_string(), is_none(&lease.ip));
    m.insert("subnet".to_string(), is_none(&lease.netmask));
    m.insert("broadcast".to_string(), is_none(&lease.broadcast));
    m.insert("ttl".to_string(), is_none_i32(&lease.ttl));
    m.insert("mtu".to_string(), is_none_i32(&lease.mtu));
    m.insert("hostname".to_string(), is_none(&lease.hostname));
    m.insert("domain".to_string(), is_none(&lease.domain));
    m.insert("siaddr".to_string(), is_none(&lease.bootp_next_server));
    m.insert("sname".to_string(), is_none(&lease.bootp_server_name));
    m.insert("boot_file".to_string(), is_none(&lease.bootp_file));
    m.insert("timezone".to_string(), is_none(&lease.timezone));
    m.insert("router".to_string(), is_none_vec(&lease.routers));
    m.insert("dns".to_string(), is_none_vec(&lease.dns_servers));
    m.insert("ntpsrv".to_string(), is_none_vec(&lease.ntp_servers));
    m.insert("lprsvr".to_string(), is_none_vec(&lease.lpr_servers));
    m.insert("timesvr".to_string(), is_none_vec(&lease._time_servers));
    m.insert("namesvr".to_string(), is_none_vec(&lease._name_servers));
    m.insert("logsvr".to_string(), is_none_vec(&lease._log_servers));
    m.insert("cookiesvr".to_string(), is_none_vec(&lease._cookie_servers));
    m.insert("wins".to_string(), is_none_vec(&lease._wins_servers));
    m.insert("swapsvr".to_string(), is_none(&lease._swap_server));
    m.insert("bootsize".to_string(), lease.bootsize.is_none());
    m.insert("rootpath".to_string(), is_none(&lease.root_path));
    m.insert("lease".to_string(), is_none_u64(&lease.lease));
    m.insert("dhcptype".to_string(), is_none(&lease.dhcp_type));
    m.insert("serverid".to_string(), is_none(&lease.server_id));
    m.insert("message".to_string(), is_none(&lease.reason));
    m.insert("tftp".to_string(), is_none(&lease.tftp));
    m.insert("bootfile".to_string(), is_none(&lease.bootfile));
    m.insert("uptime".to_string(), is_none_u64(&lease.uptime));
    m.insert("lease_expiry".to_string(), is_none_dt(&lease.lease_expiry));
    m
}

async fn query_ipv6_rtnetlink(interface: &str) -> Result<
    HashMap<String, (Option<u8>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<u8>)>,
    Box<dyn std::error::Error>
> {

    #[repr(C)]
    struct IfAddrMsg {
        ifa_family: u8,
        ifa_prefixlen: u8,
        ifa_flags: u8,
        ifa_scope: u8,
        ifa_index: u32,
    }

    #[repr(C)]
    struct ifa_cacheinfo {
        ifa_prefered: u32,
        ifa_valid: u32,
        cstamp: u32,
        tstamp: u32,
    }

    // helpers
    fn nlmsg_align(len: usize) -> usize { (len + 3) & !3 }
    fn rta_align(len: usize) -> usize { (len + 3) & !3 }

    fn send_getaddr_dump(fd: i32, ifindex: u32) -> io::Result<()> {
        let mut req_hdr: nlmsghdr = unsafe { zeroed() };
        let mut req_body: IfAddrMsg = unsafe { zeroed() };

        req_hdr.nlmsg_len = (size_of::<nlmsghdr>() + size_of::<IfAddrMsg>()) as u32;
        req_hdr.nlmsg_type = RTM_GETADDR as u16;
        req_hdr.nlmsg_flags = (NLM_F_REQUEST | NLM_F_DUMP) as u16;
        req_hdr.nlmsg_seq = 1;
        req_hdr.nlmsg_pid = 0;

        req_body.ifa_family = AF_INET6 as u8;
        req_body.ifa_prefixlen = 0;
        req_body.ifa_flags = 0;
        req_body.ifa_scope = 0;
        req_body.ifa_index = ifindex;

        let mut buf = vec![0u8; req_hdr.nlmsg_len as usize];
        let hdr_ptr = &req_hdr as *const nlmsghdr as *const u8;
        unsafe { copy_nonoverlapping(hdr_ptr, buf.as_mut_ptr(), size_of::<nlmsghdr>()); }
        let body_ptr = &req_body as *const IfAddrMsg as *const u8;
        unsafe { copy_nonoverlapping(body_ptr, buf.as_mut_ptr().add(size_of::<nlmsghdr>()), size_of::<IfAddrMsg>()); }

        let mut addr: sockaddr_nl = unsafe { zeroed() };
        addr.nl_family = AF_NETLINK as u16;
        addr.nl_pid = 0;
        addr.nl_groups = 0;
        let ret = unsafe { libc::sendto(
            fd,
            buf.as_ptr() as *const _,
            buf.len(),
            0,
            &addr as *const sockaddr_nl as *const libc::sockaddr,
            size_of::<sockaddr_nl>() as u32,
        ) };
        if ret < 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }

    fn recv_dump(fd: i32, ifindex: u32) -> io::Result<StdHashMap<String, (Option<u8>, Option<u32>, Option<u32>, Option<u8>)>> {
        let mut out: StdHashMap<String, (Option<u8>, Option<u32>, Option<u32>, Option<u8>)> = StdHashMap::new();
        let mut buf = vec![0u8; 32 * 1024];

        loop {
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
            if n < 0 { return Err(io::Error::last_os_error()); }
            if n == 0 { return Ok(out); }

            let mut offset: usize = 0;
            while offset + size_of::<nlmsghdr>() <= n as usize {
                let hdr: nlmsghdr = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset) as *const nlmsghdr) };
                if hdr.nlmsg_len == 0 { break; }

                match hdr.nlmsg_type as i32 {
                    NLMSG_DONE => return Ok(out),
                    NLMSG_NOOP => {},
                    NLMSG_ERROR => return Err(io::Error::new(io::ErrorKind::Other, "netlink NLMSG_ERROR")),
                    _ => {
                        if (hdr.nlmsg_type as i32) == (libc::RTM_NEWADDR as i32) {
                            // parse ifaddrmsg
                            let base = offset + size_of::<nlmsghdr>();
                            if base + size_of::<IfAddrMsg>() > (offset + hdr.nlmsg_len as usize) { break; }
                            let ifa: IfAddrMsg = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(base) as *const IfAddrMsg) };
                            if ifa.ifa_family != AF_INET6 as u8 || ifa.ifa_index != ifindex { /* skip other fam/if */ }
                            else {
                                let scope: Option<u8> = Some(ifa.ifa_scope);
                                let flags: Option<u8> = Some(ifa.ifa_flags);
                                let mut preferred: Option<u32> = None;
                                let mut valid: Option<u32> = None;
                                let mut ip_addr: Option<String> = None;

                                let mut rta_off = base + size_of::<IfAddrMsg>();
                                let end = offset + hdr.nlmsg_len as usize;
                                while rta_off + size_of::<nlattr>() <= end {
                                    let rta: nlattr = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(rta_off) as *const nlattr) };
                                    if rta.nla_len < size_of::<nlattr>() as u16 { break; }
                                    let payload_len = (rta.nla_len as usize) - size_of::<nlattr>();
                                    let payload_ptr = unsafe { buf.as_ptr().add(rta_off + size_of::<nlattr>()) };

                                    match rta.nla_type as i32 {
                                        x if x == (libc::IFA_CACHEINFO as i32) => {
                                            if payload_len >= size_of::<ifa_cacheinfo>() {
                                                let ci: ifa_cacheinfo = unsafe { std::ptr::read_unaligned(payload_ptr as *const ifa_cacheinfo) };
                                                if ci.ifa_prefered != u32::MAX { preferred = Some(ci.ifa_prefered); }
                                                if ci.ifa_valid != u32::MAX { valid = Some(ci.ifa_valid); }
                                            }
                                        }
                                        x if x == (libc::IFA_ADDRESS as i32) => {
                                            if payload_len == 16 {
                                                let mut octets = [0u8; 16];
                                                unsafe { copy_nonoverlapping(payload_ptr, octets.as_mut_ptr(), 16); }
                                                let ip = std::net::Ipv6Addr::from(octets);
                                                ip_addr = Some(ip.to_string());
                                            }
                                        }
                                        _ => {}
                                    }

                                    rta_off += rta_align(rta.nla_len as usize);
                                }

                                if let Some(ip) = ip_addr {
                                    out.insert(ip, (scope, valid, preferred, flags));
                                }
                            }
                        }
                    }
                }

                offset += nlmsg_align(hdr.nlmsg_len as usize);
            }
        }
    }

    {
        let ifindex = unsafe { libc::if_nametoindex(std::ffi::CString::new(interface).unwrap().as_ptr()) };
        if ifindex == 0 { return Ok(HashMap::new()); }

        let fd = unsafe { libc::socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) };
        if fd < 0 { return Err(Box::new(io::Error::last_os_error())); }

        let mut addr: sockaddr_nl = unsafe { zeroed() };
        addr.nl_family = AF_NETLINK as u16;
        addr.nl_pid = 0;
        addr.nl_groups = 0;
        let ret = unsafe { libc::bind(
            fd,
            &addr as *const sockaddr_nl as *const libc::sockaddr,
            size_of::<sockaddr_nl>() as u32,
        ) };
        if ret < 0 { let _ = unsafe { libc::close(fd) }; return Err(Box::new(io::Error::last_os_error())); }

        let r = send_getaddr_dump(fd, ifindex);
        if r.is_err() { let _ = unsafe { libc::close(fd) }; return Err(Box::new(r.err().unwrap())); }

        let map = recv_dump(fd, ifindex as u32)?;
        let _ = unsafe { libc::close(fd) };

        // Convert seconds to DateTime<Utc>
        let now = Utc::now();
        let mut out: HashMap<String, (Option<u8>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<u8>)> = HashMap::new();
        for (k, (scope, valid_s, pref_s, flags)) in map.into_iter() {
            let valid_dt = valid_s.map(|s| now + chrono::Duration::seconds(s as i64));
            let pref_dt = pref_s.map(|s| now + chrono::Duration::seconds(s as i64));
            out.insert(k, (scope, valid_dt, pref_dt, flags));
        }
        Ok(out)
    }
}