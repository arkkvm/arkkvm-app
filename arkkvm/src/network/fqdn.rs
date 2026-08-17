use anyhow::{Result, anyhow};

use super::vlan;
use super::NetworkInterfaceState;

/// Resolve domain: explicit value, else DHCP lease (primary VLAN when enabled), else `local`.
pub async fn resolve_domain(domain: &str) -> String {
    let domain = if domain == "dhcp" { "" } else { domain };
    let mut domain = domain.trim().to_owned();
    if domain.is_empty() {
        if let Some(dhcp_lease) = dhcp_lease_for_domain_lookup().await {
            domain = dhcp_lease.domain.unwrap_or_default();
        }
    }
    if domain.is_empty() {
        domain = "local".to_owned();
    }
    domain
}

async fn dhcp_lease_for_domain_lookup() -> Option<super::DhcpLease> {
    let report = vlan::get_effective_vlan_config_report().await;
    if report.vlan_enabled {
        if let Some(vlan_id) = report.primary_vlan_id {
            let iface = vlan::vlan_iface_name(vlan_id);
            return NetworkInterfaceState::new(&iface).get_dhcp_lease().await;
        }
    }
    NetworkInterfaceState::get_instance().get_dhcp_lease().await
}

fn normalize_hostname_label(hostname: &str) -> Result<String> {
    let mut hostname = hostname.trim().trim_end_matches('.');
    if hostname.is_empty() {
        hostname = "arkkvm";
    }
    if !hostname.is_ascii() {
        return Err(anyhow!("the hostname is not ascii"));
    }
    Ok(hostname.to_lowercase())
}

fn format_fqdn(hostname: &str, domain: &str) -> Result<String> {
    let domain = domain.trim().trim_end_matches('.');
    let fqdn =
        if domain == "local" { format!("{hostname}.local.") } else { format!("{hostname}.{domain}.local.") };
    if fqdn.is_ascii() { Ok(fqdn) } else { Err(anyhow!("the fqdn is not ascii")) }
}

/// Build FQDN `{hostname}.{domain}.` with domain resolution.
pub async fn build_network_fqdn(hostname: &str, domain: &str) -> Result<String> {
    let hostname = normalize_hostname_label(hostname)?;
    let domain = resolve_domain(domain).await;
    format_fqdn(&hostname, &domain)
}

/// Build bare hostname and FQDN.
pub async fn build_hostname_and_fqdn(hostname: &str, domain: &str) -> Result<(String, String)> {
    let hostname = normalize_hostname_label(hostname)?;
    let domain = resolve_domain(domain).await;
    let fqdn = format_fqdn(&hostname, &domain)?;
    Ok((hostname, fqdn))
}

/// Build FQDN when domain is already resolved.
pub fn format_fqdn_from_parts(hostname: &str, domain: &str) -> Result<String> {
    let hostname = normalize_hostname_label(hostname)?;
    format_fqdn(&hostname, domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_fqdn_local_domain() {
        assert_eq!(format_fqdn_from_parts("arkkvm", "local").unwrap(), "arkkvm.local.");
        assert_eq!(format_fqdn_from_parts("host", "corp").unwrap(), "host.corp.local.");
    }

    #[test]
    fn empty_hostname_defaults_to_arkkvm() {
        assert_eq!(format_fqdn_from_parts("", "local").unwrap(), "arkkvm.local.");
    }
}
