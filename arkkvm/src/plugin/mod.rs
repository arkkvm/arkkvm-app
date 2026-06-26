//! Optional device plugins (Tailscale, etc.).

use anyhow::Result;

use crate::module::{rtc_request_params::TailscaleParams, rtc_response_params::TailscaleStateResponse};
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