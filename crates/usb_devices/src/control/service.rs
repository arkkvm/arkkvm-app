use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::config::UsbBaseInfo;
use crate::devices::ums::UmsMode;
use crate::error::UsbError;
use crate::manager::UsbDeviceManager;
use crate::proto::v1::*;

pub enum ControlCmd {
    Apply { usb_info: UsbDeviceInfo, reply: oneshot::Sender<Result<DeviceSwitches, UsbError>> },
    ApplyRuntime {
        req: ApplyRuntimeConfigRequest,
        reply: oneshot::Sender<Result<ApplyRuntimeConfigResponse, UsbError>>,
    },
    Get { reply: oneshot::Sender<DeviceSwitches> },
    GetUdcStatus { reply: oneshot::Sender<String> },
    GetUsbEmulationState { reply: oneshot::Sender<bool> },
    SetUsbEmulationState {
        enabled: bool,
        reply: oneshot::Sender<Result<bool, UsbError>>,
    },
    SetMicProcess {
        enabled: bool,
        reply: oneshot::Sender<Result<(), UsbError>>,
    },
    GetMicProcessState { reply: oneshot::Sender<bool> },
    UmsControl {
        req: UmsControlRequest,
        reply: oneshot::Sender<UmsControlResponse>,
    },
    Putkeyboard { event: KeyboardReportParams },
    Putabsmouse { event: AbsMouseReportParams },
    Putrelmouse { event: RelMouseReportParams },
    Putwheel { event: WheelReportParams },
}

#[derive(Clone)]
pub struct ControlHandle {
    tx: Arc<mpsc::Sender<ControlCmd>>,
}

impl ControlHandle {
    pub async fn apply(&self, usb_info: UsbDeviceInfo) -> Result<DeviceSwitches, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::Apply { usb_info, reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control reply dropped".into()))?
    }

    pub async fn get(&self) -> Result<DeviceSwitches, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::Get { reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control reply dropped".into()))
    }

    pub async fn get_udc_status(&self) -> Result<String, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::GetUdcStatus { reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control udc status reply dropped".into()))
    }

    pub async fn get_usb_emulation_state(&self) -> Result<bool, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::GetUsbEmulationState { reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control emulation state reply dropped".into()))
    }

    pub async fn get_mic_process_state(&self) -> Result<bool, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::GetMicProcessState { reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control mic process state reply dropped".into()))
    }

    pub async fn set_mic_process(&self, enabled: bool) -> Result<(), UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::SetMicProcess {
                enabled,
                reply: reply_tx,
            })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control mic process reply dropped".into()))?
    }

    pub async fn set_usb_emulation_state(&self, enabled: bool) -> Result<bool, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::SetUsbEmulationState {
                enabled,
                reply: reply_tx,
            })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control emulation set reply dropped".into()))?
    }

    pub async fn apply_runtime(
        &self,
        req: ApplyRuntimeConfigRequest,
    ) -> Result<ApplyRuntimeConfigResponse, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::ApplyRuntime { req, reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control reply dropped".into()))?
    }

    pub async fn ums_control(&self, req: UmsControlRequest) -> Result<UmsControlResponse, UsbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ControlCmd::UmsControl { req, reply: reply_tx })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| UsbError::InvalidArgument("control ums reply dropped".into()))
    }

    pub async fn key_put_keyboard(&self, event: KeyboardReportParams) -> Result<(), UsbError> {
        self.tx
            .send(ControlCmd::Putkeyboard { event })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        Ok(())
    }

    pub async fn key_put_absmouse(&self, event: AbsMouseReportParams) -> Result<(), UsbError> {
        self.tx
            .send(ControlCmd::Putabsmouse { event })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        Ok(())
    }

    pub async fn key_put_relmouse(&self, event: RelMouseReportParams) -> Result<(), UsbError> {
        self.tx
            .send(ControlCmd::Putrelmouse { event })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        Ok(())
    }

    pub async fn key_put_wheel(&self, event: WheelReportParams) -> Result<(), UsbError> {
        self.tx
            .send(ControlCmd::Putwheel { event })
            .await
            .map_err(|_| UsbError::InvalidArgument("control service stopped".into()))?;
        Ok(())
    }
}

pub fn spawn_control_service(
    manager: UsbDeviceManager,
) -> (ControlHandle, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<ControlCmd>(32);
    let handle = ControlHandle { tx: Arc::new(tx) };

    let join = tokio::spawn(async move {
        let mut manager = manager;
        let mut last_usb_info: Option<UsbDeviceInfo> = None;

        let reporter = Arc::new(crate::control::zenoh_udc::ZenohUdcReporter);
        if let Err(e) = manager.add_lifecycle_handler(reporter).await {
            warn!("failed to register zenoh UDC lifecycle handler: {:?}", e);
        }

        while let Some(cmd) = rx.recv().await {
            match cmd {
                ControlCmd::Apply { usb_info, reply } => {
                    let res = apply_usb_info(&mut manager, &mut last_usb_info, usb_info, None).await;
                    let _ = reply.send(res);
                }
                ControlCmd::ApplyRuntime { req, reply } => {
                    let res = apply_runtime_usb_info(&mut manager, &mut last_usb_info, req).await;
                    let _ = reply.send(res);
                }
                ControlCmd::Get { reply } => {
                    let _ = reply.send(manager.get_switches());
                }
                ControlCmd::GetUdcStatus { reply } => {
                    let _ = reply.send(manager.get_udc_status_sysfs().await);
                }
                ControlCmd::GetUsbEmulationState { reply } => {
                    let _ = reply.send(manager.get_emulation_enabled());
                }
                ControlCmd::SetUsbEmulationState { enabled, reply } => {
                    let result = manager
                        .set_emulation_enabled(enabled)
                        .await
                        .map(|_| manager.get_emulation_enabled());
                    let _ = reply.send(result);
                }
                ControlCmd::SetMicProcess { enabled, reply } => {
                    let result = manager.apply_mic_process_enabled(enabled).await;
                    let _ = reply.send(result);
                }
                ControlCmd::GetMicProcessState { reply } => {
                    let _ = reply.send(manager.is_mic_process_running().await);
                }
                ControlCmd::UmsControl { req, reply } => {
                    let resp = manager.handle_ums_control(req).await;
                    let _ = reply.send(resp);
                }
                ControlCmd::Putkeyboard { event } => {
                    if let Some(hid) = manager.get_hid_kb_rel().await {
                        hid.keyboard_report(event.modifier as u8, &event.keys).await;
                    }
                }
                ControlCmd::Putabsmouse { event } => {
                    if let Some(hid) = manager.get_hid_abs().await {
                        hid.abs_mouse_report(event.x, event.y, event.buttons as u8)
                            .await;
                    }
                }
                ControlCmd::Putrelmouse { event } => {
                    if let Some(hid) = manager.get_hid_kb_rel().await {
                        hid.rel_mouse_report(event.dx as i8, event.dy as i8, event.buttons as u8)
                            .await;
                    }
                }
                ControlCmd::Putwheel { event } => {
                    if let Some(hid) = manager.get_hid_abs().await {
                        hid.abs_mouse_wheel(event.wheel_y as i8).await;
                    }
                }
            }
        }
    });

    (handle, join)
}

async fn apply_runtime_usb_info(
    manager: &mut UsbDeviceManager,
    last_usb_info: &mut Option<UsbDeviceInfo>,
    req: ApplyRuntimeConfigRequest,
) -> Result<ApplyRuntimeConfigResponse, UsbError> {
    manager.set_mic_process_enabled(req.mic_process_enabled);
    let runtime_usb_config = req.usb_config.ok_or_else(|| {
        UsbError::InvalidArgument("missing usb_config in ApplyRuntimeConfigRequest".into())
    })?;
    let usb_info = req.usb_info.ok_or_else(|| {
        UsbError::InvalidArgument("missing usb_info in ApplyRuntimeConfigRequest".into())
    })?;
    let requested_switches = usb_info.switches.clone().ok_or_else(|| {
        UsbError::InvalidArgument("missing switches in usb_info".into())
    })?;
    let current_switches = manager.get_switches();
    let switches_changed = is_switches_change(&current_switches, &requested_switches);
    // None means first apply in this sidecar process — not a path change.
    let paths_changed = last_usb_info
        .as_ref()
        .map(|prev| is_paths_change(prev, &usb_info))
        .unwrap_or(false);

    let next_base_info = runtime_usb_config_to_base_info(
        &manager.get_base_info(),
        &runtime_usb_config,
    )?;
    let base_changed = manager.get_base_info() != next_base_info;

    // Descriptor-only updates must call update_base_info; set_base_info alone does not touch sysfs.
    if base_changed && !switches_changed {
        info!(
            reason = %req.reason,
            request_id = %req.request_id,
            paths_changed = paths_changed,
            "apply_runtime: usb_config update via update_base_info"
        );
        apply_base_info(manager, next_base_info).await?;
        if paths_changed {
            if let Err(e) =
                preflight_ums_path_changes(manager, &usb_info, last_usb_info.as_ref()).await
            {
                return Err(e);
            }
            apply_ums_mounts(manager, &usb_info).await?;
        }
        *last_usb_info = Some(usb_info.clone());
        return Ok(runtime_apply_ok(manager, usb_info));
    }

    let base_info_for_apply = base_changed.then_some(next_base_info);
    apply_usb_info(manager, last_usb_info, usb_info.clone(), base_info_for_apply).await?;
    Ok(runtime_apply_ok(manager, usb_info))
}

fn runtime_apply_ok(
    manager: &UsbDeviceManager,
    usb_info: UsbDeviceInfo,
) -> ApplyRuntimeConfigResponse {
    ApplyRuntimeConfigResponse {
        ok: true,
        applied: Some(manager.get_switches()),
        error: None,
        error_code: ApplyErrorCode::Unspecified as i32,
        retryable: false,
        applied_usb_info: Some(usb_info),
    }
}

fn parse_hex_u16(name: &str, value: &str) -> Result<u16, UsbError> {
    let raw = value.trim();
    let trimmed = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    u16::from_str_radix(trimmed, 16).map_err(|_| {
        UsbError::InvalidArgument(format!("invalid {} (expected hex u16): {}", name, value))
    })
}

fn runtime_usb_config_to_base_info(
    current: &UsbBaseInfo,
    runtime: &RuntimeUsbConfig,
) -> Result<UsbBaseInfo, UsbError> {
    Ok(UsbBaseInfo {
        vendor_id: parse_hex_u16("vendor_id", &runtime.vendor_id)?,
        product_id: parse_hex_u16("product_id", &runtime.product_id)?,
        bcd_device: current.bcd_device,
        bcd_usb: current.bcd_usb,
        serial_number: runtime.serial_number.clone(),
        manufacturer: runtime.manufacturer.clone(),
        product: runtime.product.clone(),
        strict_mode: current.strict_mode,
    })
}

/// Apply flow:
/// 1. Return immediately if nothing changed
/// 2. Switch change: preflight gadget (UMS mount busy, etc.) -> rebuild gadget (HID via ChangeHandler lifecycle)
/// 3. Path-only change: preflight whether UMS paths may change while mounted
/// 4. Apply UMS mounts
async fn apply_usb_info(
    manager: &mut UsbDeviceManager,
    last_usb_info: &mut Option<UsbDeviceInfo>,
    usb_info: UsbDeviceInfo,
    base_info: Option<UsbBaseInfo>,
) -> Result<DeviceSwitches, UsbError> {
    let switches = usb_info.switches.clone().ok_or_else(|| {
        UsbError::InvalidArgument("missing switches in usb_info".into())
    })?;

    let current = manager.get_switches();
    let switches_changed = is_switches_change(&current, &switches);
    let paths_changed = last_usb_info
        .as_ref()
        .map(|prev| is_paths_change(prev, &usb_info))
        .unwrap_or(false);
    let needs_initial_bind = manager.needs_initial_bind();
    info!(
        current = ?current,
        requested = ?switches,
        switches_changed = switches_changed,
        paths_changed = paths_changed,
        needs_initial_bind = needs_initial_bind,
        "control apply_usb_info request"
    );

    if !switches_changed && !paths_changed && !needs_initial_bind {
        info!("USB apply: no switch or path changes");
        return Ok(current);
    }

    if switches_changed || needs_initial_bind {
        if let Err(e) = manager.preflight_gadget_change().await {
            warn!(error = ?e, "control apply preflight_gadget_change rejected");
            return Err(e);
        }
        apply_switches(manager, base_info, &switches).await?;
    } else if paths_changed {
        if let Err(e) = preflight_ums_path_changes(manager, &usb_info, last_usb_info.as_ref()).await
        {
            warn!(error = ?e, "control apply preflight_ums_path_changes rejected");
            return Err(e);
        }
    }

    apply_ums_mounts(manager, &usb_info).await?;
    *last_usb_info = Some(usb_info);

    Ok(manager.get_switches())
}

/// Rebuild gadget when only USB descriptors (VID/PID/strings) change.
async fn apply_base_info(
    manager: &mut UsbDeviceManager,
    base_info: UsbBaseInfo,
) -> Result<(), UsbError> {
    manager.update_base_info(base_info).await
}

/// Rebuild gadget after gadget preflight passes.
async fn apply_switches(
    manager: &mut UsbDeviceManager,
    base_info: Option<UsbBaseInfo>,
    switches: &DeviceSwitches,
) -> Result<(), UsbError> {
    manager.apply_switches(base_info, Some(switches.clone())).await
}

/// On UMS image path-only change, reject if the LUN is mounted with a different path.
async fn preflight_ums_path_changes(
    manager: &UsbDeviceManager,
    usb_info: &UsbDeviceInfo,
    last_usb_info: Option<&UsbDeviceInfo>,
) -> Result<(), UsbError> {
    let Some(last) = last_usb_info else {
        return Ok(());
    };
    let switches = usb_info.switches.as_ref();

    if let Some(s) = switches {
        if s.ums_vm_enabled
            && !usb_info.ums_vm_path.is_empty()
            && usb_info.ums_vm_path != last.ums_vm_path
            && manager.is_ums_vm_mounted().await
        {
            return Err(UsbError::ChangeRejected(
                "virtual media is mounted, cannot change image path".into(),
            ));
        }

        if s.ums_ft_enabled
            && !usb_info.ums_ft_path.is_empty()
            && usb_info.ums_ft_path != last.ums_ft_path
            && manager.is_ums_ft_mounted().await
        {
            return Err(UsbError::ChangeRejected(
                "file transfer storage is mounted, cannot change image path".into(),
            ));
        }
    }

    Ok(())
}

async fn apply_ums_mounts(
    manager: &mut UsbDeviceManager,
    usb_info: &UsbDeviceInfo,
) -> Result<(), UsbError> {
    let switches = usb_info.switches.as_ref();

    if let Some(s) = switches {
        if s.ums_vm_enabled {
            if !usb_info.ums_vm_path.is_empty() {
                let mode = proto_vm_type_to_mode(usb_info.ums_vm_type);
                if let Some(ctrl) = manager.get_virtual_meida().await {
                    match ctrl.switch_mode(mode).await {
                        Ok(true) => {
                            manager.apply_switches(None, None).await?;
                        }
                        Ok(false) => {}
                        Err(e) => warn!("UMS VM switch_mode failed: {:?}", e),
                    }
                    if let Err(e) = ctrl.mount(&usb_info.ums_vm_path).await {
                        warn!("UMS VM mount failed: {:?}", e);
                    } else {
                        info!("UMS VM mounted: {}", usb_info.ums_vm_path);
                    }
                }
            }
        } else if let Some(ctrl) = manager.get_virtual_meida().await {
            let _ = ctrl.unmount().await;
        }

        if s.ums_ft_enabled {
            if !usb_info.ums_ft_path.is_empty() {
                if let Some(ctrl) = manager.get_file_transfer().await {
                    if let Err(e) = ctrl.mount(&usb_info.ums_ft_path).await {
                        warn!("UMS FT mount failed: {:?}", e);
                    } else {
                        info!("UMS FT mounted: {}", usb_info.ums_ft_path);
                    }
                }
            }
        } else if let Some(ctrl) = manager.get_file_transfer().await {
            let _ = ctrl.unmount().await;
        }
    }

    Ok(())
}

fn proto_vm_type_to_mode(vm_type: i32) -> UmsMode {
    if vm_type == UmsVmType::VmDisk as i32 {
        UmsMode::Disk
    } else {
        UmsMode::CdRom
    }
}

pub fn is_switches_change(a: &DeviceSwitches, b: &DeviceSwitches) -> bool {
    a.hid_kb_rel_enabled != b.hid_kb_rel_enabled
        || a.hid_abs_enabled != b.hid_abs_enabled
        || a.ums_vm_enabled != b.ums_vm_enabled
        || a.ums_ft_enabled != b.ums_ft_enabled
        || a.uac1_enabled != b.uac1_enabled
        || a.uvc_enabled != b.uvc_enabled
}

fn is_paths_change(a: &UsbDeviceInfo, b: &UsbDeviceInfo) -> bool {
    a.ums_vm_path != b.ums_vm_path
        || a.ums_ft_path != b.ums_ft_path
        || a.ums_vm_type != b.ums_vm_type
}
