//! Optional device plugins (Tailscale, NetBird, etc.).

use anyhow::Result;
use tracing::warn;

use crate::module::{
    rtc_request_params::{NetbirdParams, NetbirdRegisterParams, TailscaleParams},
    rtc_response_params::{NetbirdStateResponse, TailscaleStateResponse},
};
pub mod netbird;
pub mod tailscale;

macro_rules! has_changed {
    ($old:expr, $new:expr) => {
        match ($old, $new) {
            (Some(old), Some(new)) => old != new,
            (None, None) => false,
            _ => true,
        }
    };
}

pub async fn get_tailscale_state() -> Result<TailscaleStateResponse> {
    let enable = tailscale::get_enable().await?;
    let login_server = tailscale::get_login_server().await?;

    if enable == tailscale::EnableState::Enabled {
        let (raw, status) = tailscale::get_status().await?;
        Ok(TailscaleStateResponse {
            enabled: true,
            login_server: login_server.url,
            raw: Some(raw),
            connection_state: Some(status.compute_connection_state()),
            status: Some(status),
        })
    } else {
        Ok(TailscaleStateResponse {
            enabled: false,
            login_server: login_server.url,
            raw: None,
            connection_state: None,
            status: None,
        })
    }
}

pub async fn switch_tailscale(params: TailscaleParams) -> Result<()> {
    if params.enabled {
        let old_login_server = tailscale::get_login_server().await?;
        if has_changed!(old_login_server.url.as_ref(), params.login_server.as_ref()) {
            tailscale::set_login_server(params.login_server).await?;
        }
        tailscale::enable().await?;
    } else {
        tailscale::disable().await?;
    }
    Ok(())
}

pub async fn register_tailscale() -> Result<tailscale::RegisterResult> {
    tailscale::register().await
}

pub async fn register_tailscale_force() -> Result<tailscale::RegisterResult> {
    tailscale::register_force().await
}

pub async fn reset_tailscale() -> Result<()> {
    tailscale::reset().await
}

// ---------------------------------------------------------------------------
// NetBird
// ---------------------------------------------------------------------------

/// Treat empty / whitespace-only strings from the UI as "unset".
fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub async fn get_netbird_state() -> Result<NetbirdStateResponse> {
    if !netbird::is_available() {
        return Ok(NetbirdStateResponse { available: false, ..Default::default() });
    }

    let enable = netbird::get_enable().await?;
    let management_url = netbird::get_management_url().await?;
    let hostname = netbird::get_hostname().await?;

    let mut state = NetbirdStateResponse {
        available: true,
        enabled: enable == netbird::EnableState::Enabled,
        management_url,
        hostname,
        ..Default::default()
    };

    if state.enabled {
        // A daemon that is still starting (or crashed) must not break the settings page.
        match netbird::get_status().await {
            Ok((raw, status)) => {
                state.connection_state = Some(status.compute_connection_state());
                state.ip = status.ip();
                state.fqdn = status.hostname();
                state.raw = Some(raw);
                state.status = Some(status);
            }
            Err(e) => {
                state.connection_state = Some(netbird::ConnectionState::Unknown);
                state.error = Some(e.to_string());
            }
        }
    }

    Ok(state)
}

pub async fn switch_netbird(params: NetbirdParams) -> Result<()> {
    if !params.enabled {
        return netbird::disable().await;
    }

    let management_url = non_empty(params.management_url);
    if let Some(url) = management_url.as_deref() {
        netbird::validate_management_url(url)?;
    }
    let hostname = non_empty(params.hostname);
    if let Some(name) = hostname.as_deref() {
        netbird::validate_hostname(name)?;
    }

    // Enable first so a changed management URL restarts the daemon only once.
    if netbird::get_enable().await? != netbird::EnableState::Enabled {
        netbird::enable().await?;
    }
    if netbird::get_management_url().await? != management_url {
        netbird::set_management_url(management_url.as_deref()).await?;
    }
    if netbird::get_hostname().await? != hostname {
        netbird::set_hostname(hostname.as_deref()).await?;
    }
    Ok(())
}

pub async fn register_netbird(params: NetbirdRegisterParams) -> Result<netbird::RegisterResult> {
    netbird::register(non_empty(params.setup_key).as_deref()).await
}

pub async fn register_netbird_force(params: NetbirdRegisterParams) -> Result<netbird::RegisterResult> {
    netbird::register_force(non_empty(params.setup_key).as_deref()).await
}

/// Called from config reset. Older root filesystems ship without `S41netbird`;
/// that must not make the whole reset fail.
pub async fn reset_netbird() -> Result<()> {
    if !netbird::is_available() {
        return Ok(());
    }
    if let Err(e) = netbird::reset().await {
        warn!("NetBird reset failed: {e:#}");
    }
    Ok(())
}
