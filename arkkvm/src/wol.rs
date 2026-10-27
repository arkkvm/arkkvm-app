use std::str::FromStr;

use anyhow::{Result, anyhow};
use macaddr::MacAddr6;
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub const DEFAULT_WOL_BROADCAST_ADDRESS: &str = "255.255.255.255";
pub const DEFAULT_WOL_PORT: u16 = 9;

/// Build a standard Magic Packet: 6x 0xFF + 16 repetitions of the MAC.
pub fn build_magic_packet(mac_address: &str) -> Result<Vec<u8>> {
    let mac = MacAddr6::from_str(mac_address).map_err(|e| anyhow!("invalid MAC address: {}", e))?;

    let mut packet: Vec<u8> = Vec::with_capacity(6 + 16 * 6);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(mac.as_bytes());
    }
    Ok(packet)
}

/// Resolve send target: explicit RPC fields win, else stored device values, else defaults.
/// Mirrors frontend flow that often only passes `macAddress` on send after saving devices.
pub fn resolve_wol_send_target(
    explicit_broadcast: Option<&str>,
    explicit_port: Option<u16>,
    stored_broadcast: Option<&str>,
    stored_port: Option<u16>,
) -> (String, u16) {
    let addr = explicit_broadcast
        .filter(|s| !s.is_empty())
        .or(stored_broadcast.filter(|s| !s.is_empty()))
        .unwrap_or(DEFAULT_WOL_BROADCAST_ADDRESS)
        .to_string();
    let port = explicit_port.or(stored_port).unwrap_or(DEFAULT_WOL_PORT);
    (addr, port)
}

pub async fn send_magic_packet(
    mac_address: &str,
    broadcast_address: &str,
    port: u16,
) -> Result<()> {
    let packet = build_magic_packet(mac_address)?;

    // Use UDP broadcast. Bind ephemeral local port.
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
    if let Err(e) = socket.set_broadcast(true) {
        return Err(anyhow!("enable broadcast failed: {}", e));
    }

    let target = (broadcast_address, port);
    let sent = socket.send_to(&packet, target).await?;
    if sent != packet.len() {
        warn!("partial WOL packet sent");
    }

    info!(
        mac = mac_address,
        broadcast = broadcast_address,
        port,
        "WOL magic packet sent"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    #[test]
    fn build_magic_packet_structure() {
        let mac = "AA:BB:CC:DD:EE:FF";
        let packet = build_magic_packet(mac).unwrap();
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[0..6], &[0xFF; 6]);
        let mac_bytes = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        for i in 0..16 {
            let start = 6 + i * 6;
            assert_eq!(&packet[start..start + 6], &mac_bytes);
        }
    }

    #[test]
    fn build_magic_packet_rejects_invalid_mac() {
        assert!(build_magic_packet("not-a-mac").is_err());
    }

    #[test]
    fn default_constants() {
        assert_eq!(DEFAULT_WOL_BROADCAST_ADDRESS, "255.255.255.255");
        assert_eq!(DEFAULT_WOL_PORT, 9);
    }

    #[test]
    fn resolve_prefers_explicit_over_stored_and_defaults() {
        let (addr, port) = resolve_wol_send_target(
            Some("192.168.4.255"),
            Some(7),
            Some("10.0.0.255"),
            Some(9),
        );
        assert_eq!(addr, "192.168.4.255");
        assert_eq!(port, 7);
    }

    #[test]
    fn resolve_uses_stored_when_send_only_has_mac_like_frontend() {
        // Frontend WakeOnLAN.tsx currently sends only { macAddress }.
        let (addr, port) =
            resolve_wol_send_target(None, None, Some("192.168.4.255"), Some(9));
        assert_eq!(addr, "192.168.4.255");
        assert_eq!(port, 9);
    }

    #[test]
    fn resolve_falls_back_to_defaults() {
        let (addr, port) = resolve_wol_send_target(None, None, None, None);
        assert_eq!(addr, DEFAULT_WOL_BROADCAST_ADDRESS);
        assert_eq!(port, DEFAULT_WOL_PORT);
    }

    /// Local-only acceptance: packet must arrive at the configured address:port.
    #[tokio::test]
    async fn send_magic_packet_localhost_custom_port_params_correct() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected = build_magic_packet("AA:BB:CC:DD:EE:FF").unwrap();

        let recv = tokio::spawn(async move {
            let mut buf = vec![0u8; 256];
            let (n, peer) = listener.recv_from(&mut buf).await.unwrap();
            (buf[..n].to_vec(), peer)
        });

        send_magic_packet("AA:BB:CC:DD:EE:FF", "127.0.0.1", port)
            .await
            .expect("send should succeed");

        let (got, _peer) = tokio::time::timeout(std::time::Duration::from_secs(2), recv)
            .await
            .expect("recv task timed out")
            .expect("recv task failed");

        assert_eq!(got, expected, "magic packet bytes mismatch");
        assert_eq!(got.len(), 102);
    }
}
