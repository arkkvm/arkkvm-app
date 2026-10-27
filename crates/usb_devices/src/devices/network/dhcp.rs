//! Shared udhcpd control for USB network adapter (NCM / RNDIS).

use std::net::Ipv4Addr;
use std::path::Path;

use tracing::{debug, info, warn};

use super::adapter_l3::AdapterL3Settings;

/// Installed OEM path; source lives in repo `shell/arkkvm-network-adapter-udhcpd.sh`.
pub const UDHCPD_SCRIPT: &str = "/oem/usr/bin/arkkvm-network-adapter-udhcpd.sh";

/// BusyBox udhcpd lease file (see `shell/arkkvm-network-adapter-udhcpd.sh`).
pub const LEASE_FILE: &str = "/var/lib/misc/arkkvm-network-adapter-udhcpd.leases";

/// BusyBox `struct dyn_lease` packed size (see BusyBox `networking/udhcp/dhcpd.h`).
const DYN_LEASE_SIZE: usize = 36;
/// Modern BusyBox writes `written_at` (BE u64) before lease records.
const LEASE_FILE_HEADER_LEN: usize = 8;

/// One active DHCP client lease on the USB network adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpLeaseEntry {
    pub ip: Ipv4Addr,
    pub mac: [u8; 6],
    pub hostname: String,
    /// Remaining lease lifetime in seconds (from on-disk relative `expires`).
    pub expires_in_secs: u32,
}

impl DhcpLeaseEntry {
    pub fn mac_string(&self) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5]
        )
    }
}

#[derive(Debug, Clone)]
pub struct DhcpPool {
    pub start: String,
    pub end: String,
}

/// Derive a host pool `.100–.200` in the board subnet, excluding the board IP.
pub fn derive_dhcp_pool(ip_address: &str, subnet_mask: &str) -> Result<DhcpPool, String> {
    let ip: Ipv4Addr = ip_address
        .parse()
        .map_err(|_| format!("invalid ip_address {ip_address:?}"))?;
    let mask: Ipv4Addr = subnet_mask
        .parse()
        .map_err(|_| format!("invalid subnet_mask {subnet_mask:?}"))?;
    let ip_u = u32::from(ip);
    let mask_u = u32::from(mask);
    let network = ip_u & mask_u;
    let broadcast = network | !mask_u;

    let mut start = network.wrapping_add(100);
    let mut end = network.wrapping_add(200);
    if start <= network {
        start = network.wrapping_add(1);
    }
    if end >= broadcast {
        end = broadcast.saturating_sub(1);
    }
    if start > end {
        return Err("subnet too small for DHCP pool".into());
    }

    if ip_u >= start && ip_u <= end {
        if ip_u == start {
            start = ip_u.wrapping_add(1);
        } else if ip_u == end {
            end = ip_u.wrapping_sub(1);
        }
        if start > end {
            return Err("DHCP pool empty after excluding board IP".into());
        }
    }

    Ok(DhcpPool {
        start: Ipv4Addr::from(start).to_string(),
        end: Ipv4Addr::from(end).to_string(),
    })
}

pub async fn dhcp_stop() {
    if !Path::new(UDHCPD_SCRIPT).is_file() {
        debug!("network adapter DHCP script missing; skip stop");
        return;
    }
    match tokio::process::Command::new(UDHCPD_SCRIPT)
        .arg("stop")
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            debug!("network adapter DHCP stopped");
        }
        Ok(out) => {
            warn!(
                status = ?out.status.code(),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "network adapter DHCP stop returned non-zero"
            );
        }
        Err(e) => warn!(error = %e, "network adapter DHCP stop failed to exec"),
    }
}

pub async fn dhcp_start(
    ifname: &str,
    cfg: &AdapterL3Settings,
    pool: &DhcpPool,
) -> Result<(), String> {
    if ifname.starts_with("eth") {
        return Err(format!("refusing DHCP on eth interface: {ifname}"));
    }
    if !Path::new(UDHCPD_SCRIPT).is_file() {
        return Err(format!("script not found: {UDHCPD_SCRIPT}"));
    }
    let out = tokio::process::Command::new(UDHCPD_SCRIPT)
        .arg("start")
        .env("IFACE", ifname)
        .env("SERVER_IP", &cfg.ip_address)
        .env("NETMASK", &cfg.subnet_mask)
        .env("START", &pool.start)
        .env("END", &pool.end)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "status={:?} stdout={} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    info!(
        ifname = %ifname,
        start = %pool.start,
        end = %pool.end,
        "network adapter DHCP started"
    );
    Ok(())
}

/// Parse BusyBox udhcpd binary lease file and return **active** leases.
///
/// Modern BusyBox layout (`write_leases`):
/// 1. optional 8-byte BE `written_at` unix timestamp
/// 2. repeated `struct dyn_lease` (36 bytes): `expires` (BE u32, **remaining** secs),
///    `lease_nip` (BE), `mac[6]`, `hostname[20]`, `pad[2]`
///
/// File order follows `g_leases[]` index ascending (skipping empty `nip==0`), **not**
/// chronological assignment. On renew, BusyBox `add_lease` clears the MAC/IP slot then
/// reuses `oldest_expired_lease()`, so a lease **can move to another array slot** when
/// multiple slots are in use. Therefore "first record" is **not** a stable identity under
/// multi-lease + renew. See [`select_controlled_host_lease`].
pub fn parse_lease_file(data: &[u8]) -> Vec<DhcpLeaseEntry> {
    let body = lease_records_slice(data);
    let mut out = Vec::new();
    let mut offset = 0;
    while offset + DYN_LEASE_SIZE <= body.len() {
        let rec = &body[offset..offset + DYN_LEASE_SIZE];
        offset += DYN_LEASE_SIZE;

        // BusyBox: `expires = htonl(remaining_seconds)`.
        let expires_raw = u32::from_be_bytes(rec[0..4].try_into().unwrap());
        let nip = u32::from_be_bytes(rec[4..8].try_into().unwrap());
        if nip == 0 {
            continue;
        }
        let ip = Ipv4Addr::from(nip);
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&rec[8..14]);
        if mac.iter().all(|&b| b == 0) {
            continue;
        }

        let hostname_bytes = &rec[14..34];
        let hostname_end = hostname_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(hostname_bytes.len());
        let hostname = String::from_utf8_lossy(&hostname_bytes[..hostname_end])
            .trim()
            .to_string();

        // Remaining == 0 means expired (BusyBox still may keep the slot in memory, but
        // write_leases emits 0). Skip expired for peer selection.
        if expires_raw == 0 {
            continue;
        }

        out.push(DhcpLeaseEntry {
            ip,
            mac,
            hostname,
            expires_in_secs: expires_raw,
        });
    }
    out
}

fn lease_records_slice(data: &[u8]) -> &[u8] {
    if data.len() >= LEASE_FILE_HEADER_LEN {
        let with_header = (data.len() - LEASE_FILE_HEADER_LEN) % DYN_LEASE_SIZE == 0;
        let without_header = data.len() % DYN_LEASE_SIZE == 0;
        if with_header && !without_header {
            return &data[LEASE_FILE_HEADER_LEN..];
        }
        if with_header {
            // Ambiguous size: prefer header form when leading BE u64 looks like a unix time.
            let ts = u64::from_be_bytes(data[0..8].try_into().unwrap());
            if (1_000_000_000..4_102_444_800).contains(&ts) {
                return &data[LEASE_FILE_HEADER_LEN..];
            }
        }
    }
    data
}

/// Choose the HDMI-controlled host lease among active entries.
///
/// Policy (USB gadget normally has exactly one DHCP client):
/// - 0 active → `None`
/// - 1 active → that lease (renew-safe: still the only peer)
/// - >1 active → pick the lease with **largest `expires_in_secs`** (the host that keeps
///   renewing stays near full lease time; abandoned leases drain). Tie-break: earlier
///   file order. Logs should warn when this branch is used.
pub fn select_controlled_host_lease(leases: &[DhcpLeaseEntry]) -> Option<&DhcpLeaseEntry> {
    if leases.is_empty() {
        return None;
    }
    if leases.len() == 1 {
        return leases.first();
    }
    let max_rem = leases.iter().map(|l| l.expires_in_secs).max()?;
    // First among equals preserves file order (tie-break).
    leases.iter().find(|l| l.expires_in_secs == max_rem)
}

/// Read and parse the USB network adapter lease file (empty if missing).
pub fn read_active_leases() -> Result<Vec<DhcpLeaseEntry>, String> {
    let path = Path::new(LEASE_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let data = std::fs::read(path).map_err(|e| format!("read {LEASE_FILE}: {e}"))?;
    Ok(parse_lease_file(&data))
}

/// Resolve the controlled-host IPv4 from the USB DHCP lease file.
pub fn resolve_controlled_host_ip() -> Result<Option<Ipv4Addr>, String> {
    let leases = read_active_leases()?;
    if leases.len() > 1 {
        warn!(
            count = leases.len(),
            "multiple active USB DHCP leases; selecting longest-remaining as controlled host"
        );
    }
    Ok(select_controlled_host_lease(&leases).map(|l| l.ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease_record(remaining: u32, ip: [u8; 4], mac: [u8; 6], host: &[u8]) -> [u8; DYN_LEASE_SIZE] {
        let mut rec = [0u8; DYN_LEASE_SIZE];
        rec[0..4].copy_from_slice(&remaining.to_be_bytes());
        rec[4..8].copy_from_slice(&ip);
        rec[8..14].copy_from_slice(&mac);
        let n = host.len().min(20);
        rec[14..14 + n].copy_from_slice(&host[..n]);
        rec
    }

    #[test]
    fn dhcp_pool_default_24() {
        let pool = derive_dhcp_pool("172.16.0.1", "255.255.255.0").unwrap();
        assert_eq!(pool.start, "172.16.0.100");
        assert_eq!(pool.end, "172.16.0.200");
    }

    #[test]
    fn dhcp_pool_excludes_board_at_100() {
        let pool = derive_dhcp_pool("172.16.0.100", "255.255.255.0").unwrap();
        assert_eq!(pool.start, "172.16.0.101");
        assert_eq!(pool.end, "172.16.0.200");
    }

    #[test]
    fn parse_with_written_at_header() {
        let mut data = Vec::new();
        data.extend_from_slice(&1_700_000_000u64.to_be_bytes());
        data.extend_from_slice(&lease_record(
            3600,
            [172, 16, 0, 100],
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            b"pc01",
        ));
        let leases = parse_lease_file(&data);
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].ip, Ipv4Addr::new(172, 16, 0, 100));
        assert_eq!(leases[0].mac_string(), "11:22:33:44:55:66");
        assert_eq!(leases[0].hostname, "pc01");
        assert_eq!(leases[0].expires_in_secs, 3600);
    }

    #[test]
    fn parse_skips_expired() {
        let mut data = Vec::new();
        data.extend_from_slice(&1_700_000_000u64.to_be_bytes());
        data.extend_from_slice(&lease_record(
            0,
            [172, 16, 0, 100],
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            b"old",
        ));
        assert!(parse_lease_file(&data).is_empty());
    }

    #[test]
    fn select_prefers_longest_remaining() {
        let a = DhcpLeaseEntry {
            ip: Ipv4Addr::new(172, 16, 0, 100),
            mac: [1, 0, 0, 0, 0, 1],
            hostname: "a".into(),
            expires_in_secs: 100,
        };
        let b = DhcpLeaseEntry {
            ip: Ipv4Addr::new(172, 16, 0, 101),
            mac: [1, 0, 0, 0, 0, 2],
            hostname: "b".into(),
            expires_in_secs: 80000,
        };
        let leases = vec![a, b.clone()];
        let selected = select_controlled_host_lease(&leases).unwrap();
        assert_eq!(selected.ip, b.ip);
    }

    #[test]
    fn select_tie_breaks_to_first() {
        let a = DhcpLeaseEntry {
            ip: Ipv4Addr::new(172, 16, 0, 100),
            mac: [1, 0, 0, 0, 0, 1],
            hostname: "a".into(),
            expires_in_secs: 500,
        };
        let b = DhcpLeaseEntry {
            ip: Ipv4Addr::new(172, 16, 0, 101),
            mac: [1, 0, 0, 0, 0, 2],
            hostname: "b".into(),
            expires_in_secs: 500,
        };
        let leases = vec![a.clone(), b];
        assert_eq!(select_controlled_host_lease(&leases).unwrap().ip, a.ip);
    }
}
