use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use super::resolve::resolve_forward_host;

pub const IP_FORWARD_PREFIX: &str = "ip_forward_";
pub const TICKET_TTL: Duration = Duration::from_secs(60);
pub const MAX_PENDING_PER_SESSION: usize = 4;

#[derive(Debug, Clone, Deserialize)]
pub struct StartIpForwardParams {
    /// `"usb"` resolves controlled-host IP from USB DHCP; mutually exclusive with `host`.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    pub port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "tcp".to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIpForwardResponse {
    pub data_channel: String,
    pub resolved_host: String,
    pub port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardProtocol {
    Tcp,
    Udp,
}

impl ForwardProtocol {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tcp" | "" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            other => bail!("[invalid_argument] protocol must be tcp or udp, got {other}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Consumed ticket: destination already resolved at `start` time.
#[derive(Debug, Clone)]
pub struct IpForwardParams {
    pub host: String,
    pub port: u16,
    pub protocol: ForwardProtocol,
}

struct PendingTicket {
    session_id: String,
    params: IpForwardParams,
    expires_at: Instant,
}

static PENDING: Lazy<AsyncMutex<HashMap<String, PendingTicket>>> =
    Lazy::new(|| AsyncMutex::new(HashMap::new()));

fn purge_expired(map: &mut HashMap<String, PendingTicket>) {
    let now = Instant::now();
    map.retain(|_, t| t.expires_at > now);
}

pub async fn start(
    session_id: String,
    params: StartIpForwardParams,
) -> Result<StartIpForwardResponse> {
    if params.port == 0 {
        bail!("[invalid_argument] port must be in 1..=65535");
    }
    let protocol = ForwardProtocol::parse(&params.protocol)?;
    let host = resolve_forward_host(params.target.as_deref(), params.host.as_deref()).await?;

    let label = format!("{}{}", IP_FORWARD_PREFIX, Uuid::new_v4());
    let ticket = PendingTicket {
        session_id: session_id.clone(),
        params: IpForwardParams {
            host: host.clone(),
            port: params.port,
            protocol,
        },
        expires_at: Instant::now() + TICKET_TTL,
    };

    let mut map = PENDING.lock().await;
    purge_expired(&mut map);

    let pending_for_session = map.values().filter(|t| t.session_id == session_id).count();
    if pending_for_session >= MAX_PENDING_PER_SESSION {
        bail!(
            "[invalid_argument] too many pending ip forward tickets for this session (max {})",
            MAX_PENDING_PER_SESSION
        );
    }

    map.insert(label.clone(), ticket);
    Ok(StartIpForwardResponse {
        data_channel: label,
        resolved_host: host,
        port: params.port,
        protocol: protocol.as_str().to_string(),
    })
}

pub async fn consume(label: &str, session_id: &str) -> Result<IpForwardParams> {
    let mut map = PENDING.lock().await;
    purge_expired(&mut map);

    let Some(ticket) = map.remove(label) else {
        bail!("[ticket_invalid] ip forward ticket not found or already used");
    };
    if Instant::now() >= ticket.expires_at {
        bail!("[ticket_invalid] ip forward ticket expired");
    }
    if ticket.session_id != session_id {
        map.insert(label.to_string(), ticket);
        bail!("[ticket_invalid] ip forward ticket session mismatch");
    }
    Ok(ticket.params)
}

pub async fn clear_session(session_id: &str) {
    let mut map = PENDING.lock().await;
    map.retain(|_, t| t.session_id != session_id);
}

pub fn is_ip_forward_label(label: &str) -> bool {
    label.starts_with(IP_FORWARD_PREFIX)
}
