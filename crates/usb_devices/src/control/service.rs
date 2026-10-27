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
                    let base_info = manager.get_base_info();
                    let res =
                        apply_full_config(&mut manager, &mut last_usb_info, usb_info, base_info)
                            .await;
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
                    if let Some(hid) = manager.get_hid_kb().await {
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
                    if let Some(hid) = manager.get_hid_rel().await {
                        hid.rel_mouse_report(event.dx as i8, event.dy as i8, event.buttons as u8)
                            .await;
                    }
                }
                ControlCmd::Putwheel { event } => {
                    if let Some(hid) = manager.get_hid_abs().await {
                        hid.abs_mouse_wheel(event.wheel_y as i8, event.wheel_x as i8)
                            .await;
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
    let base_info =
        runtime_usb_config_to_base_info(&manager.get_base_info(), &runtime_usb_config)?;

    info!(
        reason = %req.reason,
        request_id = %req.request_id,
        "apply_runtime_config"
    );

    apply_full_config(manager, last_usb_info, usb_info.clone(), base_info).await?;
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
        force_full_speed: runtime.force_full_speed,
    })
}

/// Full gadget lifecycle + UMS sync, except pure network adapter L3 updates (no unbind).
async fn apply_full_config(
    manager: &mut UsbDeviceManager,
    last_usb_info: &mut Option<UsbDeviceInfo>,
    usb_info: UsbDeviceInfo,
    base_info: UsbBaseInfo,
) -> Result<DeviceSwitches, UsbError> {
    let switches = usb_info.switches.clone().ok_or_else(|| {
        UsbError::InvalidArgument("missing switches in usb_info".into())
    })?;

    manager
        .set_network_adapter_l3_config(usb_info.network_adapter_l3.as_ref())
        .await;

    let current = manager.get_switches();
    let switches_changed = is_switches_change(&current, &switches);
    let paths_changed = last_usb_info
        .as_ref()
        .map(|prev| is_paths_change(prev, &usb_info))
        .unwrap_or(false);
    let base_changed = manager.get_base_info() != base_info;
    let network_l3_changed = last_usb_info
        .as_ref()
        .map(|prev| is_network_adapter_l3_change(prev, &usb_info))
        .unwrap_or(usb_info.network_adapter_l3.is_some());

    info!(
        current = ?current,
        requested = ?switches,
        switches_changed = switches_changed,
        paths_changed = paths_changed,
        base_changed = base_changed,
        network_l3_changed = network_l3_changed,
        "apply_full_config"
    );

    // Switches / paths / base unchanged: only refresh board L3 (no gadget rebuild).
    if last_usb_info.is_some()
        && !switches_changed
        && !paths_changed
        && !base_changed
        && network_l3_changed
    {
        info!("apply_full_config: network adapter L3-only update");
        manager.apply_network_adapter_l3_only().await?;
        *last_usb_info = Some(usb_info);
        return Ok(manager.get_switches());
    }

    apply_switches(manager, Some(base_info), &switches).await?;
    apply_ums_mounts(manager, &usb_info, last_usb_info.as_ref()).await?;

    *last_usb_info = Some(usb_info);
    Ok(manager.get_switches())
}

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
    last_usb_info: Option<&UsbDeviceInfo>,
) -> Result<(), UsbError> {
    preflight_ums_path_changes(manager, usb_info, last_usb_info).await?;

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
    a.hid_kb_enabled != b.hid_kb_enabled
        || a.hid_abs_enabled != b.hid_abs_enabled
        || a.hid_rel_enabled != b.hid_rel_enabled
        || a.ums_vm_enabled != b.ums_vm_enabled
        || a.ums_ft_enabled != b.ums_ft_enabled
        || a.uac1_enabled != b.uac1_enabled
        || a.uvc_enabled != b.uvc_enabled
        || network_adapter_switch_changed(a.network_adapter.as_ref(), b.network_adapter.as_ref())
}

fn network_adapter_switch_changed(
    a: Option<&crate::proto::v1::NetworkAdapterSwitch>,
    b: Option<&crate::proto::v1::NetworkAdapterSwitch>,
) -> bool {
    match (a, b) {
        (None, None) => false,
        (Some(a), Some(b)) => a.enable != b.enable || a.adapter_type != b.adapter_type,
        (None, Some(b)) => b.enable,
        (Some(a), None) => a.enable,
    }
}

fn is_paths_change(a: &UsbDeviceInfo, b: &UsbDeviceInfo) -> bool {
    a.ums_vm_path != b.ums_vm_path
        || a.ums_ft_path != b.ums_ft_path
        || a.ums_vm_type != b.ums_vm_type
}

fn is_network_adapter_l3_change(a: &UsbDeviceInfo, b: &UsbDeviceInfo) -> bool {
    a.network_adapter_l3 != b.network_adapter_l3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_switches(ums_ft: bool) -> DeviceSwitches {
        DeviceSwitches {
            hid_kb_enabled: true,
            hid_abs_enabled: true,
            hid_rel_enabled: true,
            ums_vm_enabled: true,
            ums_ft_enabled: ums_ft,
            uac1_enabled: false,
            uvc_enabled: false,
            network_adapter: None,
        }
    }

    #[test]
    fn is_switches_change_reports_identical_switches() {
        let a = sample_switches(true);
        let b = sample_switches(true);
        assert!(!is_switches_change(&a, &b));
    }

    #[test]
    fn is_switches_change_reports_ft_toggle() {
        let a = sample_switches(true);
        let b = sample_switches(false);
        assert!(is_switches_change(&a, &b));
    }

    #[test]
    fn is_paths_change_reports_ft_path_diff() {
        let a = UsbDeviceInfo {
            switches: Some(sample_switches(true)),
            ums_vm_path: String::new(),
            ums_ft_path: "/a.img".into(),
            ums_vm_type: UmsVmType::VmCdRom as i32,
            network_adapter_l3: None,
        };
        let b = UsbDeviceInfo {
            ums_ft_path: "/b.img".into(),
            ..a.clone()
        };
        assert!(is_paths_change(&a, &b));
    }
}
