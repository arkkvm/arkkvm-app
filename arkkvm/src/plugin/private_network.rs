//! Mutually exclusive private-network overlay (none / Tailscale / NetBird).
//!
//! The device has little RAM (~180 MB) and both overlays default to the
//! `100.64.0.0/10` CGNAT range, so the UI offers a single choice instead of two
//! independent switches.
//!
//! The selection is *derived* from the plugins' own enable flags, so there is no
//! extra persisted state that could drift, and devices that already run both
//! overlays are reported as [`PrivateNetworkSelection::Both`] until the user picks one.
//!
//! Switching is "make before break": the new overlay is enabled first and the
//! previous one is only stopped once the new one is connected. If the new overlay
//! never connects (e.g. login not completed), the previous one keeps running, so
//! a user who is connected through it does not lock themselves out. Stopping an
//! overlay uses `disable`, not `reset`, so its login survives and switching back
//! does not require a new login.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::time::Instant;
use tracing::{info, warn};

use super::{netbird, tailscale};

/// How long to wait for the new overlay to connect (covers an SSO login).
const SWITCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Poll interval while waiting for the new overlay to connect.
const SWITCH_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Bumped on every request; a waiting switch task stops when it changes.
static SWITCH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Target of the switch that is currently waiting for its overlay to connect.
static PENDING_TARGET: Mutex<Option<Overlay>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Overlay {
    Tailscale,
    Netbird,
}

impl Overlay {
    fn other(self) -> Self {
        match self {
            Self::Tailscale => Self::Netbird,
            Self::Netbird => Self::Tailscale,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tailscale => "Tailscale",
            Self::Netbird => "NetBird",
        }
    }
}

/// What the user can choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrivateNetworkTarget {
    None,
    Tailscale,
    Netbird,
}

impl PrivateNetworkTarget {
    fn overlay(self) -> Option<Overlay> {
        match self {
            Self::None => None,
            Self::Tailscale => Some(Overlay::Tailscale),
            Self::Netbird => Some(Overlay::Netbird),
        }
    }
}

/// What is currently active (derived from the enable flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrivateNetworkSelection {
    None,
    Tailscale,
    Netbird,
    /// Both enabled: legacy state from independent switches, or a switch in progress.
    Both,
}

impl PrivateNetworkSelection {
    pub fn from_flags(tailscale_enabled: bool, netbird_enabled: bool) -> Self {
        match (tailscale_enabled, netbird_enabled) {
            (false, false) => Self::None,
            (true, false) => Self::Tailscale,
            (false, true) => Self::Netbird,
            (true, true) => Self::Both,
        }
    }
}

/// Per-overlay summary for the selector.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayState {
    /// The system image ships this overlay (init script present).
    pub available: bool,
    pub enabled: bool,
    pub connected: bool,
    /// Overlay IPs and DNS names of this device. The UI compares them with the
    /// address it was opened on to detect "this session runs over the overlay".
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateNetworkState {
    pub selection: PrivateNetworkSelection,
    /// Set while a switch waits for this overlay to connect before stopping the other one.
    pub pending_target: Option<Overlay>,
    pub tailscale: OverlayState,
    pub netbird: OverlayState,
}

// ---------------------------------------------------------------------------
// Overlay adapters
// ---------------------------------------------------------------------------

fn is_available(overlay: Overlay) -> bool {
    match overlay {
        Overlay::Tailscale => Path::new(tailscale::INIT_SCRIPT_PATH).exists(),
        Overlay::Netbird => netbird::is_available(),
    }
}

async fn is_enabled(overlay: Overlay) -> Result<bool> {
    Ok(match overlay {
        Overlay::Tailscale => tailscale::get_enable().await? == tailscale::EnableState::Enabled,
        Overlay::Netbird => netbird::get_enable().await? == netbird::EnableState::Enabled,
    })
}

async fn enable(overlay: Overlay) -> Result<()> {
    match overlay {
        Overlay::Tailscale => tailscale::enable().await,
        Overlay::Netbird => netbird::enable().await,
    }
}

/// Stop the overlay but keep its login (no reset).
async fn disable(overlay: Overlay) -> Result<()> {
    match overlay {
        Overlay::Tailscale => tailscale::disable().await,
        Overlay::Netbird => netbird::disable().await,
    }
}

fn push_unique(addresses: &mut Vec<String>, value: &str) {
    let value = value.trim().trim_end_matches('.');
    if !value.is_empty() && !addresses.iter().any(|a| a == value) {
        addresses.push(value.to_string());
    }
}

/// Connected flag and addresses; status errors (daemon starting) count as "not connected".
async fn connection(overlay: Overlay) -> (bool, Vec<String>) {
    let mut addresses = Vec::new();
    match overlay {
        Overlay::Tailscale => match tailscale::get_status().await {
            Ok((_, status)) => {
                let self_node = status.self_node.as_ref();
                for ip in status
                    .tailscale_ips
                    .iter()
                    .flatten()
                    .chain(self_node.and_then(|n| n.tailscale_ips.as_ref()).into_iter().flatten())
                {
                    push_unique(&mut addresses, ip);
                }
                if let Some(node) = self_node {
                    push_unique(&mut addresses, &node.dns_name);
                }
                (status.is_connected(), addresses)
            }
            Err(e) => {
                warn!("private network: tailscale status unavailable: {e:#}");
                (false, addresses)
            }
        },
        Overlay::Netbird => match netbird::get_status().await {
            Ok((_, status)) => {
                if let Some(ip) = status.ip() {
                    push_unique(&mut addresses, &ip);
                }
                if let Some(ipv6) = status.netbird_ipv6.split('/').next() {
                    push_unique(&mut addresses, ipv6);
                }
                if let Some(fqdn) = status.hostname() {
                    push_unique(&mut addresses, &fqdn);
                }
                (status.is_connected(), addresses)
            }
            Err(e) => {
                warn!("private network: netbird status unavailable: {e:#}");
                (false, addresses)
            }
        },
    }
}

async fn overlay_state(overlay: Overlay) -> Result<OverlayState> {
    if !is_available(overlay) {
        return Ok(OverlayState::default());
    }
    let enabled = is_enabled(overlay).await?;
    let (connected, addresses) = if enabled { connection(overlay).await } else { (false, Vec::new()) };
    Ok(OverlayState { available: true, enabled, connected, addresses })
}

async fn is_connected(overlay: Overlay) -> bool {
    connection(overlay).await.0
}

fn set_pending(target: Option<Overlay>) {
    if let Ok(mut pending) = PENDING_TARGET.lock() {
        *pending = target;
    }
}

fn pending() -> Option<Overlay> {
    PENDING_TARGET.lock().ok().and_then(|p| *p)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn get_state() -> Result<PrivateNetworkState> {
    let tailscale = overlay_state(Overlay::Tailscale).await?;
    let netbird = overlay_state(Overlay::Netbird).await?;
    Ok(PrivateNetworkState {
        selection: PrivateNetworkSelection::from_flags(tailscale.enabled, netbird.enabled),
        pending_target: pending(),
        tailscale,
        netbird,
    })
}

/// Select the overlay. Returns once the target is enabled; stopping the previous
/// overlay happens in the background after the target has connected.
pub async fn select(target: PrivateNetworkTarget) -> Result<()> {
    // Supersede any switch that is still waiting.
    let generation = SWITCH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    set_pending(None);

    let Some(target) = target.overlay() else {
        for overlay in [Overlay::Tailscale, Overlay::Netbird] {
            if is_available(overlay) && is_enabled(overlay).await? {
                info!("private network: disabling {}", overlay.name());
                disable(overlay).await?;
            }
        }
        return Ok(());
    };

    if !is_available(target) {
        return Err(anyhow!("{} is not available on this system image", target.name()));
    }

    if !is_enabled(target).await? {
        info!("private network: enabling {}", target.name());
        enable(target).await?;
    }

    let other = target.other();
    if is_available(other) && is_enabled(other).await? {
        info!(
            "private network: keeping {} until {} is connected",
            other.name(),
            target.name()
        );
        set_pending(Some(target));
        tokio::spawn(stop_other_when_connected(generation, target, other));
    }
    Ok(())
}

/// Background part of [`select`]: stop `other` once `target` is connected.
async fn stop_other_when_connected(generation: u64, target: Overlay, other: Overlay) {
    let deadline = Instant::now() + SWITCH_TIMEOUT;
    let superseded = || SWITCH_GENERATION.load(Ordering::SeqCst) != generation;

    while Instant::now() < deadline {
        tokio::time::sleep(SWITCH_POLL_INTERVAL).await;
        if superseded() {
            return;
        }
        if is_connected(target).await {
            if superseded() {
                return;
            }
            info!("private network: {} connected, disabling {}", target.name(), other.name());
            if let Err(e) = disable(other).await {
                warn!("private network: failed to disable {}: {e:#}", other.name());
            }
            if !superseded() {
                set_pending(None);
            }
            return;
        }
    }

    warn!(
        "private network: {} did not connect within {}s, keeping {} running",
        target.name(),
        SWITCH_TIMEOUT.as_secs(),
        other.name()
    );
    if !superseded() {
        set_pending(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_from_flags() {
        assert_eq!(PrivateNetworkSelection::from_flags(false, false), PrivateNetworkSelection::None);
        assert_eq!(PrivateNetworkSelection::from_flags(true, false), PrivateNetworkSelection::Tailscale);
        assert_eq!(PrivateNetworkSelection::from_flags(false, true), PrivateNetworkSelection::Netbird);
        assert_eq!(PrivateNetworkSelection::from_flags(true, true), PrivateNetworkSelection::Both);
    }

    #[test]
    fn target_maps_to_overlay() {
        assert_eq!(PrivateNetworkTarget::None.overlay(), None);
        assert_eq!(PrivateNetworkTarget::Tailscale.overlay(), Some(Overlay::Tailscale));
        assert_eq!(PrivateNetworkTarget::Netbird.overlay(), Some(Overlay::Netbird));
        assert_eq!(Overlay::Tailscale.other(), Overlay::Netbird);
    }

    #[test]
    fn serde_names() {
        assert_eq!(serde_json::to_string(&PrivateNetworkSelection::Both).unwrap(), "\"both\"");
        let t: PrivateNetworkTarget = serde_json::from_str("\"netbird\"").unwrap();
        assert_eq!(t, PrivateNetworkTarget::Netbird);
        assert!(serde_json::from_str::<PrivateNetworkTarget>("\"both\"").is_err());
    }

    #[test]
    fn addresses_are_normalized_and_unique() {
        let mut a = Vec::new();
        push_unique(&mut a, "arkkvm.tail1234.ts.net.");
        push_unique(&mut a, "arkkvm.tail1234.ts.net");
        push_unique(&mut a, " 100.64.0.5 ");
        push_unique(&mut a, "");
        assert_eq!(a, vec!["arkkvm.tail1234.ts.net", "100.64.0.5"]);
    }
}
