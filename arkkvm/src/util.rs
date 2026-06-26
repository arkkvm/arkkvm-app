use std::net::IpAddr;

/// Gets the first non-loopback IPv4 address from the local network interfaces
///
/// # Returns
/// * `Some(IpAddr)` - The first non-loopback IPv4 address found
/// * `None` - If no suitable IP address is found
///
/// # Implementation Details
/// - Uses `if_addrs` crate to get network interfaces
/// - Filters out loopback interfaces
/// - Extracts IPv4 addresses
/// - Returns first non-loopback address found
///
/// # Example
/// ```
/// if let Some(ip) = get_local_ip() {
///     println!("Local IP address: {}", ip);
/// }
/// ```
pub fn local_ip() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(addr) => Some(IpAddr::V4(addr.ip)),
            _ => None,
        })
        .find(|ip| !ip.is_loopback())
}

pub fn local_ip_v6() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V6(addr) => Some(IpAddr::V6(addr.ip)),
            _ => None,
        })
        .find(|ip| !ip.is_loopback())
}

pub fn is_link_local_ip() -> bool {
    if let Some(IpAddr::V4(ip)) = local_ip() {
        ip.is_link_local()
    } else {
        false
    }
}

pub fn is_link_local_ip_v6() -> bool {
    if let Some(IpAddr::V6(ip)) = local_ip_v6() {
        ip.is_unicast_link_local()
    } else {
        false
    }
}