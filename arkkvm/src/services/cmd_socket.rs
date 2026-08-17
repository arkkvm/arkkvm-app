//! Unix command socket for display-app → arkkvm-app
//! (network factory restore / full factory reset).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use crate::jsonrpc::handlers;
use crate::network::settings;
use crate::network::vlan;

const CMD_SOCKET_PATH: &str = "/tmp/arkkvm_cmd.sock";

static RUNNING: AtomicBool = AtomicBool::new(false);
static RESET_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub fn init() {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async {
        if let Err(e) = run_server().await {
            error!("cmd_socket server stopped: {e}");
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

pub fn uninit() {
    RUNNING.store(false, Ordering::SeqCst);
    let _ = std::fs::remove_file(CMD_SOCKET_PATH);
}

async fn run_server() -> anyhow::Result<()> {
    if Path::new(CMD_SOCKET_PATH).exists() {
        let _ = std::fs::remove_file(CMD_SOCKET_PATH);
    }
    let listener = UnixListener::bind(CMD_SOCKET_PATH)?;
    info!("cmd_socket listening on {CMD_SOCKET_PATH}");

    while RUNNING.load(Ordering::SeqCst) {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                if !RUNNING.load(Ordering::SeqCst) {
                    break;
                }
                warn!("cmd_socket accept: {e}");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream).await {
                warn!("cmd_socket client: {e}");
            }
        });
    }
    Ok(())
}

async fn handle_client(mut stream: UnixStream) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    let resp = dispatch(&req).await;
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

async fn dispatch(req: &str) -> String {
    // Accept KEY or KEY=VALUE
    let (key, value) = match req.split_once('=') {
        Some((k, v)) => (k.trim(), Some(v.trim())),
        None => (req.trim(), None),
    };
    match key {
        /*
         * Wait until factory reset finishes, then reply OK / ERROR.
         * Small-UI stays on "Restoring..." until this response arrives
         * (UI reads on a background thread so LVGL keeps painting).
         *
         * Success path in settings::reset_network_to_factory:
         *   VLAN off + DHCP defaults → write network.conf → refresh_committed
         *   → config updated → "Network restored to factory defaults"
         */
        "RESET_NETWORK_FACTORY" => {
            if RESET_IN_PROGRESS.swap(true, Ordering::SeqCst) {
                return "ERROR=busy".to_string();
            }
            let resp = match settings::reset_network_to_factory().await {
                Ok(()) => {
                    info!("RESET_NETWORK_FACTORY completed");
                    "OK".to_string()
                }
                Err(e) => {
                    error!("RESET_NETWORK_FACTORY failed: {e}");
                    format!("ERROR={e}")
                }
            };
            RESET_IN_PROGRESS.store(false, Ordering::SeqCst);
            resp
        }
        /*
         * Same wipe+reboot as JSON-RPC facrotyReset.
         * Small-UI stays on "Resetting..." until OK (device then reboots).
         */
        "FACTORY_RESET" => {
            if RESET_IN_PROGRESS.swap(true, Ordering::SeqCst) {
                return "ERROR=busy".to_string();
            }
            let resp = match handlers::factory_reset().await {
                Ok(_) => {
                    info!("FACTORY_RESET completed (reboot scheduled)");
                    "OK".to_string()
                }
                Err(e) => {
                    error!("FACTORY_RESET failed: {e}");
                    RESET_IN_PROGRESS.store(false, Ordering::SeqCst);
                    format!("ERROR={e}")
                }
            };
            /* On success, reboot is already spawned; leave the busy flag set. */
            resp
        }
        /*
         * Lab / serial: SET_HOSTNAME=<label> goes through the same
         * set_network_settings path as the Web UI (incl. VLAN hostname patch).
         */
        "SET_HOSTNAME" => {
            let Some(hostname) = value.filter(|v| !v.is_empty()) else {
                return "ERROR=missing hostname".to_string();
            };
            if !hostname.is_ascii()
                || hostname.len() > 63
                || hostname.contains('/')
                || hostname.contains('\\')
                || hostname.contains(' ')
            {
                return "ERROR=invalid hostname".to_string();
            }
            let mut cfg = settings::get_network_settings().await;
            cfg.hostname = Some(hostname.to_string());
            match settings::set_network_settings(cfg).await {
                Ok(()) => {
                    info!(hostname, "SET_HOSTNAME completed");
                    format!("OK hostname={hostname}")
                }
                Err(e) => {
                    error!("SET_HOSTNAME failed: {e}");
                    format!("ERROR={e}")
                }
            }
        }
        /*
         * Lab: re-apply current committed VLAN via the same set_vlan_settings
         * path as WebRTC JSON-RPC (cloud reconnect ordering / ARKKVM-577).
         * Does not change VLAN ID — safe for SSH while on a working VLAN.
         */
        "REAPPLY_VLAN" => match vlan::reapply_current_vlan_settings().await {
            Ok(resp) => {
                let ipv4 = resp
                    .redirect
                    .primary_vlan
                    .as_ref()
                    .and_then(|p| p.ipv4.as_deref())
                    .unwrap_or("-");
                let vid = resp
                    .redirect
                    .primary_vlan
                    .as_ref()
                    .map(|p| p.vlan_id.to_string())
                    .unwrap_or_else(|| "-".to_string());
                info!(vid = %vid, ipv4, "REAPPLY_VLAN completed");
                format!("OK vlan_id={vid} ipv4={ipv4}")
            }
            Err(e) => {
                error!("REAPPLY_VLAN failed: {e}");
                format!("ERROR={e}")
            }
        },
        /*
         * Lab: SET_VLAN_PRIMARY_ID=<1-4094> then set_vlan_settings.
         * Changing ID can brick SSH if the switch does not pass that VLAN.
         */
        "SET_VLAN_PRIMARY_ID" => {
            let Some(raw) = value.filter(|v| !v.is_empty()) else {
                return "ERROR=missing vlan_id".to_string();
            };
            let Ok(vlan_id) = raw.parse::<u16>() else {
                return "ERROR=invalid vlan_id".to_string();
            };
            match vlan::set_primary_vlan_id_and_apply(vlan_id).await {
                Ok(resp) => {
                    let ipv4 = resp
                        .redirect
                        .primary_vlan
                        .as_ref()
                        .and_then(|p| p.ipv4.as_deref())
                        .unwrap_or("-");
                    info!(vlan_id, ipv4, "SET_VLAN_PRIMARY_ID completed");
                    format!("OK vlan_id={vlan_id} ipv4={ipv4}")
                }
                Err(e) => {
                    error!("SET_VLAN_PRIMARY_ID failed: {e}");
                    format!("ERROR={e}")
                }
            }
        }
        /*
         * Lab: APPLY_VLAN_DHCP=6 or APPLY_VLAN_DHCP=6,5
         * Enables VLAN primary(+optional secondary) DHCP via set_vlan_settings.
         */
        "APPLY_VLAN_DHCP" => {
            let Some(raw) = value.filter(|v| !v.is_empty()) else {
                return "ERROR=missing vlan ids (e.g. 6 or 6,5)".to_string();
            };
            let parts: Vec<&str> = raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if parts.is_empty() || parts.len() > 2 {
                return "ERROR=expected APPLY_VLAN_DHCP=<primary>[,<secondary>]".to_string();
            }
            let Ok(primary_id) = parts[0].parse::<u16>() else {
                return "ERROR=invalid primary vlan_id".to_string();
            };
            let secondary_id = if parts.len() == 2 {
                match parts[1].parse::<u16>() {
                    Ok(v) => Some(v),
                    Err(_) => return "ERROR=invalid secondary vlan_id".to_string(),
                }
            } else {
                None
            };
            match vlan::apply_vlan_dhcp_lab(primary_id, secondary_id).await {
                Ok(resp) => {
                    let ipv4 = resp
                        .redirect
                        .primary_vlan
                        .as_ref()
                        .and_then(|p| p.ipv4.as_deref())
                        .unwrap_or("-");
                    info!(primary_id, ?secondary_id, ipv4, "APPLY_VLAN_DHCP completed");
                    format!(
                        "OK primary={primary_id} secondary={} ipv4={ipv4}",
                        secondary_id
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    )
                }
                Err(e) => {
                    error!("APPLY_VLAN_DHCP failed: {e}");
                    format!("ERROR={e}")
                }
            }
        }
        "APPLY_VLAN_OFF" => match vlan::apply_vlan_off_lab().await {
            Ok(_) => {
                info!("APPLY_VLAN_OFF completed");
                "OK vlan_enabled=false".to_string()
            }
            Err(e) => {
                error!("APPLY_VLAN_OFF failed: {e}");
                format!("ERROR={e}")
            }
        },
        _ => format!("ERROR=Unknown command: {key}"),
    }
}
