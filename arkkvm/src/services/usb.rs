use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use prost::Message;
use tokio::sync::{OnceCell, RwLock};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};
use zenoh::bytes::ZBytes;

use crate::config::types::{UsbConfig, UsbDevices};
use crate::hardware::usb::KeyboardState as HidKeyboardState;
use crate::hardware::usb::storage::{self, VirtualMediaMode};
use crate::proto::v1::*;
use crate::{jsonrpc, zenoh_bus};

pub const KEY_APPLY: &str = "arkkvm/usb_devices/query/apply_switches";
pub const KEY_APPLY_RUNTIME: &str = "arkkvm/usb_devices/query/apply_runtime_config";
pub const KEY_GET: &str = "arkkvm/usb_devices/query/get_switches";
pub const KEY_UMS_CONTROL: &str = "arkkvm/usb_devices/query/ums_control";

pub const KEY_PUT_KEYBOARD: &str = "arkkvm/usb_devices/event/keyboard";
pub const KEY_PUT_ABSMOUSE: &str = "arkkvm/usb_devices/event/absmouse";
pub const KEY_PUT_RELMOUSE: &str = "arkkvm/usb_devices/event/relmouse";
pub const KEY_PUT_WHEEL: &str = "arkkvm/usb_devices/event/wheel";

pub const KEY_EVENT_KEYBOARD_LED: &str = "arkkvm/usb_devices/event/keyboard_led";
pub const KEY_EVENT_UDC_STATE: &str = "arkkvm/usb_devices/event/udc_state";
pub use crate::zenoh_bus::{KEY_EVENT_MIC_PROCESS, KEY_GET_MIC_PROCESS_STATE};
pub const KEY_GET_UDC_STATUS: &str = "arkkvm/usb_devices/query/get_udc_status";
pub const KEY_GET_USB_EMULATION_STATE: &str = "arkkvm/usb_devices/query/get_usb_emulation_state";
pub const KEY_SET_USB_EMULATION_STATE: &str = "arkkvm/usb_devices/query/set_usb_emulation_state";
pub const KEY_SET_MIC_PROCESS: &str = "arkkvm/usb_devices/query/set_mic_process";

/// Sidecar gadget rebuild + UDC recover can exceed the default Zenoh 10s query timeout.
const ZENOH_RUNTIME_QUERY_TIMEOUT: Duration = Duration::from_secs(35);
/// UMS switch/mount may trigger gadget rebuild (same order of magnitude as apply_switches).
const ZENOH_UMS_QUERY_TIMEOUT: Duration = Duration::from_secs(35);
const ZENOH_UDC_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const ZENOH_EMULATION_QUERY_TIMEOUT: Duration = Duration::from_secs(35);

lazy_static::lazy_static! {
    static ref USB: OnceCell<Arc<UsbClient>> = OnceCell::new();
}

pub async fn init() -> Result<()> {
    let client = USB
        .get_or_init(|| async {
            let client = Arc::new(UsbClient::new());
            client.spawn_keyboard_led_subscriber();
            client.spawn_udc_state_subscriber();
            client
        })
        .await;

    if let Ok(state) = client.get_udc_status().await {
        let _ = jsonrpc::handlers::set_usb_state(state);
    }

    Ok(())
}

/// Return cached UDC state; query sidecar when cache is still `"unknown"`.
pub async fn ensure_usb_state() -> String {
    let cached =
        jsonrpc::handlers::get_usb_state().unwrap_or_else(|_| "unknown".to_string());
    if cached != "unknown" {
        return cached;
    }

    let Some(usb) = get_usb() else {
        return "unknown".to_string();
    };

    match usb.get_udc_status().await {
        Ok(state) => state,
        Err(e) => {
            warn!("ensure_usb_state: get_udc_status failed: {}", e);
            "unknown".to_string()
        }
    }
}

pub fn get_usb() -> Option<Arc<UsbClient>> {
    USB.get().cloned()
}

/// Map persisted USB device flags and mount paths to a sidecar apply payload.
pub async fn usb_info_from_devices(devices: &UsbDevices) -> Result<UsbDeviceInfo> {
    let (ums_vm_path, ums_ft_path, ums_vm_type) =
        match storage::resolve_ums_paths_for_apply().await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "resolve_ums_paths_for_apply failed, fallback to empty paths: {}",
                    e
                );
                (String::new(), String::new(), UmsVmType::VmCdRom)
            }
        };
    Ok(usb_info_from_devices_with_paths(devices, ums_vm_path, ums_ft_path, ums_vm_type))
}

/// Build apply payload using cached sidecar UMS paths (avoids slow ums_control during reconcile).
pub async fn usb_info_for_reconcile(devices: &UsbDevices) -> Result<UsbDeviceInfo> {
    if let Some(usb) = get_usb() {
        let cached = usb.cached_usb_info().await;
        let ums_vm_type =
            UmsVmType::try_from(cached.ums_vm_type).unwrap_or(UmsVmType::VmCdRom);
        return Ok(usb_info_from_devices_with_paths(
            devices,
            cached.ums_vm_path,
            cached.ums_ft_path,
            ums_vm_type,
        ));
    }
    usb_info_from_devices(devices).await
}

pub fn usb_info_from_devices_with_paths(
    devices: &UsbDevices,
    ums_vm_path: String,
    ums_ft_path: String,
    ums_vm_type: UmsVmType,
) -> UsbDeviceInfo {
    UsbDeviceInfo {
        switches: Some(DeviceSwitches {
            hid_kb_rel_enabled: devices.keyboard,
            hid_abs_enabled: devices.absolute_mouse,
            ums_vm_enabled: devices.mass_storage_vm,
            ums_ft_enabled: devices.mass_storage_ft,
            uac1_enabled: devices.microphone,
            uvc_enabled: devices.camera,
        }),
        ums_vm_type: ums_vm_type as i32,
        ums_vm_path,
        ums_ft_path,
    }
}

pub fn runtime_usb_config_from_config(config: &UsbConfig) -> RuntimeUsbConfig {
    RuntimeUsbConfig {
        vendor_id: config.vendor_id.clone(),
        product_id: config.product_id.clone(),
        serial_number: config.serial_number.clone(),
        manufacturer: config.manufacturer.clone(),
        product: config.product.clone(),
    }
}

pub struct UsbClient {
    session: zenoh::Session,
    usb_info: RwLock<UsbDeviceInfo>,
}

impl UsbClient {
    fn ensure_switches(info: &mut UsbDeviceInfo) -> &mut DeviceSwitches {
        info.switches.get_or_insert_with(DeviceSwitches::default)
    }

    pub fn new() -> Self {
        Self {
            session: zenoh_bus::get_usb_session(),
            usb_info: RwLock::new(UsbDeviceInfo::default()),
        }
    }

    pub async fn cached_usb_info(&self) -> UsbDeviceInfo {
        self.usb_info.read().await.clone()
    }

    pub async fn apply_usb_devices(&self, usb_info: UsbDeviceInfo) -> Result<ApplySwitchesResponse> {
        let mut last_err: Option<anyhow::Error> = None;
        info!(usb_info = ?usb_info, "usb client apply_usb_devices start");

        for attempt in 1..=8 {
            let req = ApplySwitchesRequest {
                usb_info: Some(usb_info.clone()),
            };

            match self.send_with_reply(req).await {
                Ok(resp) => {
                    info!(
                        attempt = attempt,
                        ok = resp.ok,
                        error = %resp.error.clone().unwrap_or_default(),
                        applied = ?resp.applied,
                        "usb client apply_usb_devices received response"
                    );
                    if resp.ok {
                        *self.usb_info.write().await = usb_info;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    warn!(
                        attempt = attempt,
                        error = %e,
                        "usb client apply_usb_devices attempt failed"
                    );
                    last_err = Some(e);
                    sleep(Duration::from_millis(250)).await;
                }
            }
        }

        if let Some(err) = last_err {
            error!(error = %err, "usb client apply_usb_devices exhausted retries");
            Err(err)
        } else {
            Err(anyhow!("apply_usb_devices failed"))
        }
    }

    pub async fn apply_runtime_config(
        &self,
        usb_config: RuntimeUsbConfig,
        usb_info: UsbDeviceInfo,
        reason: impl Into<String>,
        request_id: impl Into<String>,
        mic_process_enabled: bool,
    ) -> Result<ApplyRuntimeConfigResponse> {
        let mut last_err: Option<anyhow::Error> = None;
        let reason = reason.into();
        let request_id = request_id.into();
        info!(
            reason = %reason,
            request_id = %request_id,
            mic_process_enabled = mic_process_enabled,
            "usb client apply_runtime_config start"
        );

        for attempt in 1..=5 {
            let req = ApplyRuntimeConfigRequest {
                usb_config: Some(usb_config.clone()),
                usb_info: Some(usb_info.clone()),
                reason: reason.clone(),
                request_id: request_id.clone(),
                mic_process_enabled,
            };
            match self.send_runtime_with_reply(req).await {
                Ok(resp) => {
                    info!(
                        attempt = attempt,
                        ok = resp.ok,
                        retryable = resp.retryable,
                        error_code = resp.error_code,
                        error = %resp.error.clone().unwrap_or_default(),
                        "usb client apply_runtime_config response"
                    );
                    if resp.ok {
                        *self.usb_info.write().await = usb_info;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    warn!(
                        attempt = attempt,
                        error = %e,
                        "usb client apply_runtime_config transport failed"
                    );
                    last_err = Some(e);
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }

        if let Some(err) = last_err {
            Err(err)
        } else {
            Err(anyhow!("apply_runtime_config failed"))
        }
    }

    pub async fn send_with_reply(&self, req: ApplySwitchesRequest) -> Result<ApplySwitchesResponse> {
        let mut buf = Vec::new();
        req.encode(&mut buf)?;
        info!(key = KEY_APPLY, request_size = buf.len(), "usb client send apply request");

        let replies = self
            .session
            .get(KEY_APPLY)
            .payload(ZBytes::from(buf))
            .await
            .map_err(|e| anyhow!("zenoh get failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("apply_switches query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            info!(response_size = bytes.len(), "usb client receive apply response");
            ApplySwitchesResponse::decode(&bytes[..]).context("decode ApplySwitchesResponse")
        } else {
            warn!("usb client no reply received from usb_devices");
            Err(anyhow!("no reply received from usb_devices"))
        }
    }

    /// Mount VM LUN directly via sidecar (no gadget rebuild).
    pub async fn ums_mount_vm(&self, path: &str, mode: VirtualMediaMode) -> Result<()> {
        let vm_type = match mode {
            VirtualMediaMode::CDROM => UmsVmType::VmCdRom,
            VirtualMediaMode::Disk => UmsVmType::VmDisk,
        };
        self.ums_control(UmsControlRequest {
            operation: UmsOperation::UmsMount as i32,
            target: UmsTarget::UmsVm as i32,
            image_path: path.to_string(),
            vm_type: vm_type as i32,
        })
        .await
    }

    pub async fn ums_unmount_vm(&self) -> Result<()> {
        self.ums_control(UmsControlRequest {
            operation: UmsOperation::UmsUnmount as i32,
            target: UmsTarget::UmsVm as i32,
            image_path: String::new(),
            vm_type: UmsVmType::VmCdRom as i32,
        })
        .await
    }

    pub async fn ums_mount_ft(&self, path: &str) -> Result<()> {
        self.ums_control(UmsControlRequest {
            operation: UmsOperation::UmsMount as i32,
            target: UmsTarget::UmsFt as i32,
            image_path: path.to_string(),
            vm_type: UmsVmType::VmCdRom as i32,
        })
        .await
    }

    pub async fn ums_unmount_ft(&self) -> Result<()> {
        self.ums_control(UmsControlRequest {
            operation: UmsOperation::UmsUnmount as i32,
            target: UmsTarget::UmsFt as i32,
            image_path: String::new(),
            vm_type: UmsVmType::VmCdRom as i32,
        })
        .await
    }

    pub async fn ums_switch_vm_mode(&self, mode: VirtualMediaMode) -> Result<()> {
        let vm_type = match mode {
            VirtualMediaMode::CDROM => UmsVmType::VmCdRom,
            VirtualMediaMode::Disk => UmsVmType::VmDisk,
        };
        self.ums_control(UmsControlRequest {
            operation: UmsOperation::UmsSwitchVmMode as i32,
            target: UmsTarget::UmsVm as i32,
            image_path: String::new(),
            vm_type: vm_type as i32,
        })
        .await
    }

    pub async fn ums_get_mount_state(&self, target: UmsTarget) -> Result<UmsControlResponse> {
        self.send_ums_control(UmsControlRequest {
            operation: UmsOperation::UmsGetMountState as i32,
            target: target as i32,
            image_path: String::new(),
            vm_type: UmsVmType::VmCdRom as i32,
        })
        .await
    }

    /// Legacy: use apply when switches change; use `ums_mount_vm` for image-only updates.
    pub async fn mount_vm_image(&self, path: &str, mode: VirtualMediaMode) -> Result<()> {
        self.ums_mount_vm(path, mode).await
    }

    pub async fn mount_ft_image(&self, path: &str) -> Result<()> {
        self.ums_mount_ft(path).await
    }

    async fn ums_control(&self, req: UmsControlRequest) -> Result<()> {
        let resp = self.send_ums_control(req).await?;
        if resp.ok {
            Ok(())
        } else {
            Err(anyhow!(
                "ums_control failed: {}",
                resp.error.unwrap_or_default()
            ))
        }
    }

    async fn send_ums_control(&self, req: UmsControlRequest) -> Result<UmsControlResponse> {
        let mut buf = Vec::new();
        req.encode(&mut buf)?;
        info!(key = KEY_UMS_CONTROL, op = req.operation, target = req.target, "usb client send ums_control");

        let replies = self
            .session
            .get(KEY_UMS_CONTROL)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_UMS_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh ums_control get failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply.into_result().map_err(|e| {
                anyhow!(
                    "ums_control query error: {} (op={}, target={})",
                    e,
                    req.operation,
                    req.target
                )
            })?;
            let bytes = sample.payload().to_bytes();
            let resp = UmsControlResponse::decode(&bytes[..]).context("decode UmsControlResponse")?;
            info!(
                ok = resp.ok,
                mounted = resp.mounted,
                path = %resp.mounted_path,
                error = %resp.error.clone().unwrap_or_default(),
                "usb client ums_control response"
            );
            Ok(resp)
        } else {
            Err(anyhow!("no reply received from usb_devices ums_control"))
        }
    }

    async fn send_runtime_with_reply(
        &self,
        req: ApplyRuntimeConfigRequest,
    ) -> Result<ApplyRuntimeConfigResponse> {
        let mut buf = Vec::new();
        req.encode(&mut buf)?;
        info!(
            key = KEY_APPLY_RUNTIME,
            request_size = buf.len(),
            "usb client send runtime apply request"
        );

        let replies = self
            .session
            .get(KEY_APPLY_RUNTIME)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_RUNTIME_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh runtime get failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("apply_runtime_config query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            ApplyRuntimeConfigResponse::decode(&bytes[..])
                .context("decode ApplyRuntimeConfigResponse")
        } else {
            Err(anyhow!("no reply received from usb_devices runtime apply"))
        }
    }

    pub async fn key_put_keyboard(&self, event: KeyboardReportParams) -> Result<()> {
        self.session
            .put(KEY_PUT_KEYBOARD, ZBytes::from(event.encode_to_vec()))
            .await
            .map_err(|e| anyhow!("Failed to send keyboard event: {}", e))?;
        crate::hardware::usb::note_user_input();
        Ok(())
    }

    pub async fn key_put_absmouse(&self, event: AbsMouseReportParams) -> Result<()> {
        let by_user = event.by_user;
        self.session
            .put(KEY_PUT_ABSMOUSE, ZBytes::from(event.encode_to_vec()))
            .await
            .map_err(|e| anyhow!("Failed to send abs mouse event: {}", e))?;
        if by_user {
            crate::hardware::usb::note_user_input();
        }
        Ok(())
    }

    pub async fn key_put_relmouse(&self, event: RelMouseReportParams) -> Result<()> {
        let by_user = event.by_user;
        self.session
            .put(KEY_PUT_RELMOUSE, ZBytes::from(event.encode_to_vec()))
            .await
            .map_err(|e| anyhow!("Failed to send rel mouse event: {}", e))?;
        if by_user {
            crate::hardware::usb::note_user_input();
        }
        Ok(())
    }

    pub async fn key_put_wheel(&self, event: WheelReportParams) -> Result<()> {
        self.session
            .put(KEY_PUT_WHEEL, ZBytes::from(event.encode_to_vec()))
            .await
            .map_err(|e| anyhow!("Failed to send wheel event: {}", e))?;
        crate::hardware::usb::note_user_input();
        Ok(())
    }

    pub async fn get_usb_emulation_state(&self) -> Result<bool> {
        let req = GetUsbEmulationStateRequest {};
        let mut buf = Vec::new();
        req.encode(&mut buf)?;

        let replies = self
            .session
            .get(KEY_GET_USB_EMULATION_STATE)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_EMULATION_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh get_usb_emulation_state failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("get_usb_emulation_state query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            let resp = GetUsbEmulationStateResponse::decode(&bytes[..])
                .context("decode GetUsbEmulationStateResponse")?;
            if resp.ok {
                Ok(resp.enabled)
            } else {
                Err(anyhow!(
                    "get_usb_emulation_state rejected: {}",
                    resp.error.unwrap_or_default()
                ))
            }
        } else {
            Err(anyhow!("no reply received from usb_devices get_usb_emulation_state"))
        }
    }

    pub async fn set_usb_emulation_state(&self, enabled: bool) -> Result<bool> {
        let req = SetUsbEmulationStateRequest { enabled };
        let mut buf = Vec::new();
        req.encode(&mut buf)?;

        let replies = self
            .session
            .get(KEY_SET_USB_EMULATION_STATE)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_EMULATION_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh set_usb_emulation_state failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("set_usb_emulation_state query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            let resp = SetUsbEmulationStateResponse::decode(&bytes[..])
                .context("decode SetUsbEmulationStateResponse")?;
            if resp.ok {
                Ok(resp.enabled)
            } else {
                Err(anyhow!(
                    "set_usb_emulation_state rejected: {}",
                    resp.error.unwrap_or_default()
                ))
            }
        } else {
            Err(anyhow!("no reply received from usb_devices set_usb_emulation_state"))
        }
    }

    pub async fn get_mic_process_state(&self) -> Result<bool> {
        let req = GetMicProcessStateRequest {};
        let mut buf = Vec::new();
        req.encode(&mut buf)?;

        let replies = self
            .session
            .get(KEY_GET_MIC_PROCESS_STATE)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_UDC_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh get_mic_process_state failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("get_mic_process_state query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            let resp = GetMicProcessStateResponse::decode(&bytes[..])
                .context("decode GetMicProcessStateResponse")?;
            if resp.ok {
                Ok(resp.running)
            } else {
                Err(anyhow!(
                    "get_mic_process_state rejected: {}",
                    resp.error.unwrap_or_default()
                ))
            }
        } else {
            Err(anyhow!("no reply received from usb_devices get_mic_process_state"))
        }
    }

    pub async fn set_mic_process(&self, enabled: bool) -> Result<()> {
        let req = SetMicProcessRequest { enabled };
        let mut buf = Vec::new();
        req.encode(&mut buf)?;

        let replies = self
            .session
            .get(KEY_SET_MIC_PROCESS)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_EMULATION_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh set_mic_process failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("set_mic_process query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            let resp = SetMicProcessResponse::decode(&bytes[..])
                .context("decode SetMicProcessResponse")?;
            if resp.ok {
                Ok(())
            } else {
                Err(anyhow!(
                    "set_mic_process rejected: {}",
                    resp.error.unwrap_or_default()
                ))
            }
        } else {
            Err(anyhow!("no reply received from usb_devices set_mic_process"))
        }
    }

    pub async fn get_udc_status(&self) -> Result<String> {
        let req = GetUdcStatusRequest {};
        let mut buf = Vec::new();
        req.encode(&mut buf)?;

        let replies = self
            .session
            .get(KEY_GET_UDC_STATUS)
            .payload(ZBytes::from(buf))
            .timeout(ZENOH_UDC_QUERY_TIMEOUT)
            .await
            .map_err(|e| anyhow!("zenoh get_udc_status failed: {}", e))?;

        if let Ok(reply) = replies.recv_async().await {
            let sample = reply
                .into_result()
                .map_err(|e| anyhow!("get_udc_status query error: {}", e))?;
            let bytes = sample.payload().to_bytes();
            let resp =
                GetUdcStatusResponse::decode(&bytes[..]).context("decode GetUdcStatusResponse")?;
            if resp.ok {
                let _ = jsonrpc::handlers::set_usb_state(resp.state.clone());
                Ok(resp.state)
            } else {
                Err(anyhow!(
                    "get_udc_status rejected: {}",
                    resp.error.unwrap_or_default()
                ))
            }
        } else {
            Err(anyhow!("no reply received from usb_devices get_udc_status"))
        }
    }

    fn spawn_keyboard_led_subscriber(self: &Arc<Self>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = client.keyboard_led_subscriber_loop().await {
                error!("keyboard LED subscriber exited: {}", e);
            }
        });
    }

    fn spawn_udc_state_subscriber(self: &Arc<Self>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = client.udc_state_subscriber_loop().await {
                error!("UDC state subscriber exited: {}", e);
            }
        });
    }

    async fn keyboard_led_subscriber_loop(&self) -> Result<()> {
        let subscriber = self
            .session
            .declare_subscriber(KEY_EVENT_KEYBOARD_LED)
            .await
            .map_err(|e| anyhow!("declare keyboard_led subscriber: {}", e))?;

        info!("subscribed to {}", KEY_EVENT_KEYBOARD_LED);

        loop {
            let sample = subscriber.recv_async().await.map_err(|e| anyhow!("{e}"))?;
            let bytes = sample.payload().to_bytes();
            let state = KeyboardState::decode(bytes.as_ref())
                .context("decode KeyboardState from sidecar")?;
            let hid_state = HidKeyboardState {
                num_lock: state.num_lock,
                caps_lock: state.caps_lock,
                scroll_lock: state.scroll_lock,
                compose: state.compose,
                kana: state.kana,
            };
            jsonrpc::broadcast_keyboard_led_state(hid_state).await;
        }
    }

    async fn udc_state_subscriber_loop(&self) -> Result<()> {
        let subscriber = self
            .session
            .declare_subscriber(KEY_EVENT_UDC_STATE)
            .await
            .map_err(|e| anyhow!("declare udc_state subscriber: {}", e))?;

        info!("subscribed to {}", KEY_EVENT_UDC_STATE);

        loop {
            let sample = subscriber.recv_async().await.map_err(|e| anyhow!("{e}"))?;
            let bytes = sample.payload().to_bytes();
            let status =
                UdcStatus::decode(bytes.as_ref()).context("decode UdcStatus from sidecar")?;
            jsonrpc::broadcast_usb_state(status.state).await;
        }
    }
}
