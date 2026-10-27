//! USB subsystem facade for the main process.
//!
//! USB gadget configuration/binding, UDC state, and emulation control are owned
//! by the `usb_devices` sidecar; this module reconciles runtime USB config/devices
//! with the sidecar and tracks local user-input activity.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::get_config_manager;
use crate::config::types::{UsbConfig, UsbDevices};
use crate::services;

pub mod mcp_ssh;
pub mod mic;
pub mod network_adapter;
pub mod storage;

#[derive(Debug)]
pub enum UsbDeviceType {
    AbsoluteMouse,
    RelativeMouse,
    Keyboard,
    MassStorageVm,
    MassStorageFt,
    Microphone,
    Camera,
}

/// Keyboard LED state reported by the `usb_devices` sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct KeyboardState {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
    pub compose: bool,
    pub kana: bool,
}

lazy_static::lazy_static! {
    /// Monotonic base used to derive relative input timestamps.
    static ref INPUT_BASE: Instant = Instant::now();

    /// Lock for reconciling USB runtime config with the `usb_devices` sidecar.
    pub(crate) static ref USB_RECONCILE_LOCK: Mutex<()> = Mutex::new(());
}

/// Seconds (relative to `INPUT_BASE`) of the last real user input; 0 = none yet.
///
/// `AtomicU32` (not `AtomicU64`) for portability on 32-bit ARM; second resolution
/// is sufficient for the OTA/Jiggler idle checks that consume it.
static LAST_USER_INPUT_SECS: AtomicU32 = AtomicU32::new(0);

/// Record that a real user input was just sent to the `usb_devices` sidecar.
///
/// Lock-free and synchronous; safe to call from any thread/task.
pub fn note_user_input() {
    let secs = INPUT_BASE.elapsed().as_secs().min(u32::MAX as u64) as u32;
    LAST_USER_INPUT_SECS.store(secs.max(1), Ordering::Relaxed);
}

/// Seconds since the last real user input; `u64::MAX` if there has been none.
pub fn seconds_since_last_user_input() -> u64 {
    let last = LAST_USER_INPUT_SECS.load(Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    INPUT_BASE.elapsed().as_secs().saturating_sub(last as u64)
}

pub async fn reboot_usb_manager_by_device(
    device_type: UsbDeviceType,
    enable: bool,
) -> anyhow::Result<()> {
    info!(
        device = ?device_type,
        enable = enable,
        "reboot_usb_manager_by_device request"
    );
    let mut deivces = get_config_manager().get_usb_devices().await;
    info!(
        device = ?device_type,
        enable = enable,
        current_devices = ?deivces,
        "reboot_usb_manager_by_device loaded current config"
    );

    match device_type {
        UsbDeviceType::AbsoluteMouse => deivces.absolute_mouse = enable,
        UsbDeviceType::RelativeMouse => deivces.relative_mouse = enable,
        UsbDeviceType::Keyboard => deivces.keyboard = enable,
        UsbDeviceType::MassStorageVm => deivces.mass_storage_vm = enable,
        UsbDeviceType::MassStorageFt => deivces.mass_storage_ft = enable,
        UsbDeviceType::Microphone => deivces.microphone = enable,
        UsbDeviceType::Camera => deivces.camera = enable,
    }

    info!(
        device = ?device_type,
        enable = enable,
        next_devices = ?deivces,
        "reboot_usb_manager_by_device applying updated config"
    );
    reboot_usb_manager(None, Some(deivces)).await
}

pub async fn reboot_usb_manager(
    mut usb_config: Option<UsbConfig>,
    mut devices: Option<UsbDevices>,
) -> anyhow::Result<()> {
    reboot_usb_manager_with_reason(usb_config.take(), devices.take(), "set").await
}

pub async fn reboot_usb_manager_with_reason(
    mut usb_config: Option<UsbConfig>,
    mut devices: Option<UsbDevices>,
    reason: &str,
) -> anyhow::Result<()> {
    let _guard = USB_RECONCILE_LOCK.lock().await;
    info!("reboot_usb_manager start");
    let config = get_config_manager();

    let usb_config = if let Some(usb_config) = usb_config.take() {
        usb_config
    } else {
        config.get_usb_config().await
    };

    let preference =
        if let Some(devices) = devices.take() { devices } else { config.get_usb_devices().await };

    let ssh = config.get_ssh_forwarding_enabled().await;
    let apply_devices = mcp_ssh::devices_for_apply(&preference, ssh);
    let persist_devices = mcp_ssh::devices_for_persist(&preference, ssh);

    let prev_devices = config.get_usb_devices().await;
    // Compare apply vs previous apply so SSH overlay is visible to early-return logic.
    let prev_apply = mcp_ssh::devices_for_apply(&prev_devices, ssh);

    info!(
        usb_config = ?usb_config,
        preference = ?preference,
        apply_devices = ?apply_devices,
        ssh_forwarding = ssh,
        "reboot_usb_manager apply target devices (config saved only on success)"
    );

    if services::get_usb().is_none() {
        warn!("UsbClient not initialized; skipped sidecar apply");
        return Err(anyhow::anyhow!("UsbClient not initialized"));
    }

    // L3-only: still call sidecar apply (usb_devices applies board L3 without rebuild).
    let gadget_same = network_adapter::gadget_switches_equal(&prev_apply, &apply_devices);
    let usb_config_same = config.get_usb_config().await == usb_config;
    if gadget_same
        && usb_config_same
        && !network_adapter::network_adapter_l3_equal(
            &prev_apply.network_adapter,
            &apply_devices.network_adapter,
        )
    {
        info!("reboot_usb_manager: network adapter L3-only update via sidecar");
        reconcile_usb_runtime(usb_config.clone(), apply_devices.clone(), "network_adapter_l3")
            .await?;
        config.set_usb_devices(&persist_devices).await?;
        info!(devices = ?persist_devices, "reboot_usb_manager persisted L3-only config");
        return Ok(());
    }

    if gadget_same
        && usb_config_same
        && network_adapter::network_adapter_l3_equal(
            &prev_apply.network_adapter,
            &apply_devices.network_adapter,
        )
    {
        // Still persist preference if it diverged (e.g. mouse pref while SSH overlay hides it).
        if prev_devices != persist_devices {
            config.set_usb_devices(&persist_devices).await?;
            info!(devices = ?persist_devices, "reboot_usb_manager persisted preference only");
        } else {
            info!("reboot_usb_manager: no gadget/L3 change");
        }
        return Ok(());
    }

    reconcile_usb_runtime(usb_config.clone(), apply_devices, reason).await?;

    config.set_usb_config(usb_config).await?;
    config.set_usb_devices(&persist_devices).await?;
    info!(devices = ?persist_devices, "reboot_usb_manager persisted config after success");
    info!("reboot_usb_manager done");
    Ok(())
}

fn next_request_id(reason: &str) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("{}-{}", reason, now_ms)
}

pub async fn reconcile_usb_runtime(
    usb_config: UsbConfig,
    devices: UsbDevices,
    reason: &str,
) -> anyhow::Result<()> {
    let Some(usb) = services::get_usb() else {
        return Err(anyhow::anyhow!("UsbClient not initialized"));
    };

    let usb_info = services::usb::usb_info_for_reconcile(&devices).await?;
    let runtime_usb_config = services::usb::runtime_usb_config_from_config(&usb_config);
    let request_id = next_request_id(reason);
    let mic_process_enabled = get_config_manager().get_microphone_emulation().await;

    for attempt in 1..=5 {
        let resp = usb
            .apply_runtime_config(
                runtime_usb_config.clone(),
                usb_info.clone(),
                reason.to_string(),
                request_id.clone(),
                mic_process_enabled,
            )
            .await
            .map_err(|e| anyhow::anyhow!("usb_devices runtime apply failed: {}", e))?;

        if resp.ok {
            info!(
                reason = reason,
                attempt = attempt,
                applied = ?resp.applied,
                "reconcile_usb_runtime sidecar apply ok"
            );
            return Ok(());
        }

        let err = resp.error.unwrap_or_else(|| "unknown runtime apply error".to_string());
        warn!(
            reason = reason,
            attempt = attempt,
            retryable = resp.retryable,
            error_code = resp.error_code,
            error = %err,
            "reconcile_usb_runtime sidecar apply rejected"
        );

        if !resp.retryable || attempt == 5 {
            return Err(anyhow::anyhow!("usb_devices runtime apply rejected: {}", err));
        }

        let backoff_ms = 500_u64 * (1_u64 << (attempt - 1));
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }

    Err(anyhow::anyhow!("usb_devices runtime apply exhausted retries"))
}

/// Read cached UDC state (updated by `usb_devices` via Zenoh).
pub async fn get_current_usb_state() -> String {
    crate::jsonrpc::handlers::get_usb_state().unwrap_or_else(|_| "unknown".to_string())
}
