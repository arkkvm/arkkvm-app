use std::str::FromStr;

use anyhow::{Result, anyhow};
use macaddr::MacAddr6;
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub async fn send_magic_packet(mac_address: &str) -> Result<()> {
    let mac = MacAddr6::from_str(mac_address).map_err(|e| anyhow!("invalid MAC address: {}", e))?;

    // Build magic packet: 6x 0xFF + 16 * MAC
    let mut packet: Vec<u8> = Vec::with_capacity(6 + 16 * 6);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(mac.as_bytes());
    }

    // Use UDP broadcast on port 9 (discard). Bind ephemeral local port.
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
    if let Err(e) = socket.set_broadcast(true) {
        return Err(anyhow!("enable broadcast failed: {}", e));
    }

    let target = ("255.255.255.255", 9);
    let sent = socket.send_to(&packet, target).await?;
    if sent != packet.len() {
        warn!("partial WOL packet sent");
    }

    info!(mac = mac_address, "WOL magic packet sent");
    Ok(())
}
