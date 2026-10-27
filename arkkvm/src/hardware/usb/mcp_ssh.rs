//! MCP / SSH Forwarding: USB virtual NIC gate + relative-mouse / mic apply overlay.
//!
//! Preference (`usb_devices.relative_mouse` / `microphone`) is preserved while SSH is on;
//! only the sidecar apply payload is forced off. Closing SSH re-applies preference.

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::config::get_config_manager;
use crate::config::types::UsbDevices;
use crate::hardware::usb::{self, USB_RECONCILE_LOCK};
use crate::services;

/// Build devices sent to the sidecar (overlay when SSH Forwarding is on).
pub fn devices_for_apply(preference: &UsbDevices, ssh_forwarding: bool) -> UsbDevices {
    let mut d = preference.clone();
    d.network_adapter.enable = ssh_forwarding;
    if ssh_forwarding {
        d.relative_mouse = false;
        d.microphone = false;
    }
    d
}

/// Build devices written to encrypted config (preference + NIC owned by SSH).
pub fn devices_for_persist(preference: &UsbDevices, ssh_forwarding: bool) -> UsbDevices {
    let mut d = preference.clone();
    d.network_adapter.enable = ssh_forwarding;
    d
}

async fn stop_mic_process_if_running() {
    let config = get_config_manager();
    if !config.get_microphone_emulation().await {
        return;
    }
    let Some(usb) = services::get_usb() else {
        return;
    };
    if let Err(e) = usb.set_mic_process(false).await {
        warn!(error = %e, "mcp_ssh: set_mic_process(false) failed");
    }
    if let Err(e) = services::uninit_virtual_mic_service().await {
        warn!(error = %e, "mcp_ssh: uninit_virtual_mic failed");
    }
    if let Err(e) = config.set_microphone_emulation(false).await {
        warn!(error = %e, "mcp_ssh: persist microphone_emulation=false failed");
    }
}

/// Apply overlay/reconcile and persist preference-safe devices + SSH/MCP flags.
pub async fn apply_ssh_forwarding_state(
    mcp_enabled: bool,
    ssh_forwarding: bool,
    reason: &str,
) -> Result<()> {
    let config = get_config_manager();
    let preference = config.get_usb_devices().await;
    let apply_devices = devices_for_apply(&preference, ssh_forwarding);
    let persist_devices = devices_for_persist(&preference, ssh_forwarding);
    let usb_config = config.get_usb_config().await;

    info!(
        mcp_enabled,
        ssh_forwarding,
        prefer_rel = preference.relative_mouse,
        prefer_mic = preference.microphone,
        apply_rel = apply_devices.relative_mouse,
        apply_mic = apply_devices.microphone,
        net = apply_devices.network_adapter.enable,
        reason,
        "mcp_ssh apply_ssh_forwarding_state"
    );

    if ssh_forwarding {
        stop_mic_process_if_running().await;
        // FT LUN mount can leave configfs half-torn; best-effort unmount before rebuild.
        if let Err(e) = crate::hardware::usb::storage::unmount_file_img().await {
            warn!(error = %e, "mcp_ssh: FT unmount before SSH on (ignored if not mounted)");
        }
    }

    let mut last_err = None;
    for attempt in 1..=5 {
        let result = {
            let _guard = USB_RECONCILE_LOCK.lock().await;
            usb::reconcile_usb_runtime(usb_config.clone(), apply_devices.clone(), reason).await
        };
        match result {
            Ok(()) => {
                config.set_usb_devices(&persist_devices).await?;
                config.set_mcp_enabled(mcp_enabled).await?;
                config.set_ssh_forwarding_enabled(ssh_forwarding).await?;
                return Ok(());
            }
            Err(e) => {
                warn!(attempt, error = %e, "mcp_ssh apply failed; retrying");
                last_err = Some(e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("mcp_ssh apply failed")))
}

/// Reconcile current preference under the current SSH overlay (boot / setUsbDevices).
pub async fn reconcile_with_ssh_overlay(reason: &str) -> Result<()> {
    let config = get_config_manager();
    let ssh = config.get_ssh_forwarding_enabled().await;
    let preference = config.get_usb_devices().await;
    let apply_devices = devices_for_apply(&preference, ssh);
    let persist_devices = devices_for_persist(&preference, ssh);
    let usb_config = config.get_usb_config().await;

    let _guard = USB_RECONCILE_LOCK.lock().await;
    usb::reconcile_usb_runtime(usb_config, apply_devices, reason).await?;
    if preference.network_adapter.enable != persist_devices.network_adapter.enable {
        config.set_usb_devices(&persist_devices).await?;
    }
    Ok(())
}

pub async fn set_mcp_enabled(enabled: bool) -> Result<()> {
    let config = get_config_manager();
    let prev_mcp = config.get_mcp_enabled().await;
    let prev_ssh = config.get_ssh_forwarding_enabled().await;

    if enabled == prev_mcp {
        return Ok(());
    }

    if !enabled {
        if prev_ssh {
            // Cascade: full SSH-off path (NIC + restore overlay).
            apply_ssh_forwarding_state(false, false, "mcp_off").await?;
        } else {
            // SSH already off: only clear the flag (no gadget rebuild).
            config.set_mcp_enabled(false).await?;
        }
        // ARKKVM-619: MCP OFF must tear down any live agent session / control.
        disconnect_agent_for_mcp_off().await;
        return Ok(());
    }

    config.set_mcp_enabled(true).await?;
    Ok(())
}

/// Close the agent WebRTC slot when MCP is turned off (drops HID / Control lease via remove).
async fn disconnect_agent_for_mcp_off() {
    let app = crate::web::get_global_app_state();
    let Some(session) = app.get_session_for_role(crate::session::SessionRole::Agent).await else {
        return;
    };
    let session_id = session.id.clone();
    info!(%session_id, "mcp off: closing agent session");
    crate::webrtc::remove_and_close_session(&app, &session_id).await;
}

pub async fn set_ssh_forwarding(enabled: bool) -> Result<()> {
    let config = get_config_manager();
    let mcp = config.get_mcp_enabled().await;
    let prev = config.get_ssh_forwarding_enabled().await;

    if enabled && !mcp {
        bail!("mcp_disabled: enable MCP before SSH Forwarding");
    }
    if enabled == prev {
        return Ok(());
    }

    apply_ssh_forwarding_state(
        mcp,
        enabled,
        if enabled {
            "ssh_forwarding_on"
        } else {
            "ssh_forwarding_off"
        },
    )
    .await
}
