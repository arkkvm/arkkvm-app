use crate::config::UsbBaseInfo;
use crate::devices::hid_abs::{HidAbsController, HidAbsHandle};
use crate::devices::hid_kb::{HidKbRelController, HidKbRelHandle};
use crate::devices::ums1::{Ums1Controller, Ums1Handle, UmsFtEvent, UmsFtResponse};
use crate::proto::v1::DeviceSwitches;
// use crate::config::{DeviceSwitches, UsbBaseInfo};
use crate::devices::build_gadget;
use crate::devices::uac::{UacController, UacHandle};
use crate::devices::ums::{UmsController, UmsHandle, UmsMode, UmsVmEvent, UmsVmResponse, UmsVmState};
use crate::error::UsbError;
use crate::events::{
    ChangeRequest, ChangeResponse, LifecycleEvent, UdcState, UsbChangeHandler, UsbLifecycleHandler,
};
use crate::system::{configfs, udc_state_watch::UdcStateWatch};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard, RwLock, mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use usb_gadget::{RegGadget, Udc, UdcState as RawUdcState, default_udc};

const UDC_RECOVER_TIMEOUT: Duration = Duration::from_secs(20);
const UDC_RECOVER_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const UDC_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub const VIRTUAL_MEDIA_SERVICE: &str = "UMS_HANDLE";
pub const FILE_TRANSFER_SERVICE: &str = "UMS1_HANDLE";
pub const UAC_SERVICE: &str = "UAC_HANDLE";
pub const HID_KB_REL_SERVICE: &str = "HID_KB_REL_HANDLE";
pub const HID_ABS_SERVICE: &str = "HID_ABS_HANDLE";

const DEFAULT_UMS_MODE: UmsMode = UmsMode::CdRom;

pub enum DeviceService {
    Ums(Arc<UmsController>, Arc<UmsHandle>),
    Ums1(Arc<Ums1Controller>, Arc<Ums1Handle>),
    Uac(Arc<UacController>, Arc<UacHandle>),
    HidKbRel(Arc<HidKbRelController>, Arc<HidKbRelHandle>),
    HidAbs(Arc<HidAbsController>, Arc<HidAbsHandle>),
}

pub struct UsbDeviceManager {
    udc: Udc,
    base_info: UsbBaseInfo,
    switches: DeviceSwitches,
    change_handlers: Arc<RwLock<HashMap<String, Arc<dyn UsbChangeHandler>>>>,
    lifecycle_handlers: Arc<RwLock<HashMap<String, Arc<dyn UsbLifecycleHandler>>>>,
    udc_state_cache: Arc<RwLock<Option<UdcState>>>,
    active_gadget: Option<RegGadget>,
    udc_watch: Option<UdcStateWatch>,
    device_services: Arc<RwLock<HashMap<&'static str, DeviceService>>>,
    gadget_op_lock: Arc<Mutex<()>>,
    emulation_enabled: bool,
    mic_process_enabled: bool,
}

impl Drop for UsbDeviceManager {
    fn drop(&mut self) {
        if let Some(mut watch) = self.udc_watch.take() {
            watch.stop();
        }
    }
}

impl UsbDeviceManager {
    /// Initialize the manager and register event handlers
    pub fn new(base_info: Option<UsbBaseInfo>) -> Result<Self, UsbError> {
        let udc = default_udc().map_err(|e| UsbError::UDCError(e.to_string()))?;
        configfs::ensure_mounted()?;

        let mut manager = Self {
            udc,
            base_info: base_info.unwrap_or_default(),
            switches: DeviceSwitches::default(),
            change_handlers: Arc::new(RwLock::new(HashMap::new())),
            lifecycle_handlers: Arc::new(RwLock::new(HashMap::new())),
            udc_state_cache: Arc::new(RwLock::new(None)),
            active_gadget: None,
            udc_watch: None,
            device_services: Arc::new(RwLock::new(HashMap::new())),
            gadget_op_lock: Arc::new(Mutex::new(())),
            emulation_enabled: true,
            mic_process_enabled: false,
        };

        manager.start_udc_watch();
        Ok(manager)
    }

    pub async fn add_change_handler(
        &self,
        handler: Arc<dyn UsbChangeHandler>,
    ) -> Result<(), UsbError> {
        let mut handlers = self.change_handlers.write().await;
        if handlers.get(handler.name()).is_some() {
            return Err(UsbError::InvalidArgument(format!(
                "Change handler name({}) already exists",
                handler.name()
            )));
        }
        let _ = handlers.insert(handler.name().to_owned(), handler);
        Ok(())
    }

    pub async fn remove_change_handler(&self, handler_name: &str) {
        let mut handlers = self.change_handlers.write().await;
        let _ = handlers.remove(handler_name);
    }

    pub async fn add_lifecycle_handler(
        &self,
        handler: Arc<dyn UsbLifecycleHandler>,
    ) -> Result<(), UsbError> {
        let mut handlers = self.lifecycle_handlers.write().await;
        if handlers.get(handler.name()).is_some() {
            return Err(UsbError::InvalidArgument(format!(
                "Lifecycle handler name({}) already exists",
                handler.name()
            )));
        }
        let _ = handlers.insert(handler.name().to_owned(), handler);
        Ok(())
    }

    pub async fn remove_lifecycle_handler(&self, handler_name: &str) {
        let mut handlers = self.lifecycle_handlers.write().await;
        let _ = handlers.remove(handler_name);
    }

    pub fn get_switches(&self) -> DeviceSwitches {
        self.switches.clone()
    }

    pub fn get_base_info(&self) -> UsbBaseInfo {
        self.base_info.clone()
    }

    pub async fn get_udc_state(&self) -> Option<UdcState> {
        self.udc_state_cache.read().await.clone()
    }

    pub async fn get_udc_status_sysfs(&self) -> String {
        match self.get_udc_state().await {
            Some(state) => crate::udc::udc_state_to_sysfs(state).to_string(),
            None => "unknown".to_string(),
        }
    }

    pub fn get_emulation_enabled(&self) -> bool {
        self.emulation_enabled && self.active_gadget.is_some()
    }

    /// User/intent flag: emulation should be on (independent of whether gadget is bound yet).
    pub fn emulation_intent_enabled(&self) -> bool {
        self.emulation_enabled
    }

    pub fn has_active_gadget(&self) -> bool {
        self.active_gadget.is_some()
    }

    /// Cold start: intent is enabled but first bind has not happened yet.
    pub fn needs_initial_bind(&self) -> bool {
        self.emulation_enabled && self.active_gadget.is_none()
    }

    pub fn get_mic_process_enabled(&self) -> bool {
        self.mic_process_enabled
    }

    pub fn set_mic_process_enabled(&mut self, enabled: bool) {
        self.mic_process_enabled = enabled;
    }

    pub async fn apply_mic_process_enabled(&mut self, enabled: bool) -> Result<(), UsbError> {
        self.set_mic_process_enabled(enabled);
        let services = self.device_services.read().await;
        if let Some(DeviceService::Uac(ctrl, _)) = services.get(UAC_SERVICE) {
            ctrl.set_process_enabled(enabled);
            ctrl.sync_state().await?;
        }
        Ok(())
    }

    pub async fn is_mic_process_running(&self) -> bool {
        let services = self.device_services.read().await;
        if let Some(DeviceService::Uac(ctrl, _)) = services.get(UAC_SERVICE) {
            ctrl.is_process_running()
        } else {
            false
        }
    }

    pub async fn set_emulation_enabled(&mut self, enabled: bool) -> Result<(), UsbError> {
        let op_lock = self.gadget_op_lock.clone();
        let _guard = Self::try_acquire_gadget_op(&op_lock)?;
        self.set_emulation_enabled_locked(enabled).await
    }

    fn try_acquire_gadget_op(lock: &Arc<Mutex<()>>) -> Result<MutexGuard<'_, ()>, UsbError> {
        lock.try_lock().map_err(|_| {
            UsbError::GadgetError("USB gadget operation in progress".into())
        })
    }

    async fn set_emulation_enabled_locked(&mut self, enabled: bool) -> Result<(), UsbError> {
        if enabled {
            if self.active_gadget.is_some() {
                self.emulation_enabled = true;
                return Ok(());
            }
            self.request_change_approval().await?;
            self.bind_gadget_from_current_switches().await?;
            self.emulation_enabled = true;
            for handler in self.snapshot_change_handlers().await {
                let _ = handler.on_change_request(&ChangeRequest::ChangeCompleted).await;
            }
            return Ok(());
        }

        if self.active_gadget.is_none() {
            self.emulation_enabled = false;
            return Ok(());
        }

        self.request_change_approval().await?;
        self.unbind_active_gadget().await?;
        self.emulation_enabled = false;
        for handler in self.snapshot_change_handlers().await {
            let _ = handler.on_change_request(&ChangeRequest::ChangeCompleted).await;
        }
        Ok(())
    }

    async fn bind_gadget_from_current_switches(&mut self) -> Result<(), UsbError> {
        let ums_mode = self.apply_device_services().await?;
        if !self.switches.has_any_enabled() {
            info!("UsbDeviceManager: no device functions enabled, skipping gadget bind");
            return Ok(());
        }
        let new_gadget = build_gadget(&self.base_info, &self.switches, ums_mode)?;
        let reg_gadget =
            new_gadget.bind(&self.udc).map_err(|e| UsbError::GadgetError(e.to_string()))?;
        self.active_gadget = Some(reg_gadget);
        Ok(())
    }

    async fn unbind_active_gadget(&mut self) -> Result<(), UsbError> {
        if let Some(old_gadget) = self.active_gadget.take() {
            drop(old_gadget);
            self.wait_for_udc_recovered().await?;
        }
        Ok(())
    }

    /// Apply a new device switch combination
    ///
    /// Flow:
    /// 1. Call handler.on_change_request() and wait for response
    /// 2. On Proceed, run Unbind -> Rebuild -> Bind
    /// 3. Call handler.on_lifecycle_event(ChangeCompleted)
    /// Preflight gadget change (e.g. reject if UMS is mounted); does not run Unbind/Rebuild.
    pub async fn preflight_gadget_change(&self) -> Result<(), UsbError> {
        self.request_change_only().await
    }

    pub async fn is_ums_vm_mounted(&self) -> bool {
        let services = self.device_services.read().await;
        match services.get(VIRTUAL_MEDIA_SERVICE) {
            Some(DeviceService::Ums(_, handle)) => handle.is_mounted().await,
            _ => false,
        }
    }

    pub async fn is_ums_ft_mounted(&self) -> bool {
        let services = self.device_services.read().await;
        match services.get(FILE_TRANSFER_SERVICE) {
            Some(DeviceService::Ums1(_, handle)) => handle.is_mounted().await,
            _ => false,
        }
    }

    /// UMS mount/unmount/query (mode changes may trigger gadget rebuild).
    pub async fn handle_ums_control(
        &mut self,
        req: crate::proto::v1::UmsControlRequest,
    ) -> crate::proto::v1::UmsControlResponse {
        use crate::proto::v1::{UmsOperation, UmsTarget};

        let op = UmsOperation::try_from(req.operation).unwrap_or(UmsOperation::UmsOpUnspecified);
        let target = UmsTarget::try_from(req.target).unwrap_or(UmsTarget::Unspecified);

        info!(?op, ?target, path = %req.image_path, "handle_ums_control");

        match op {
            UmsOperation::UmsOpUnspecified => ums_resp_err("unspecified ums operation"),
            UmsOperation::UmsGetMountState => match target {
                UmsTarget::UmsVm => self.ums_vm_mount_state().await,
                UmsTarget::UmsFt => self.ums_ft_mount_state().await,
                _ => ums_resp_err("get_mount_state requires ums_vm or ums_ft target"),
            },
            UmsOperation::UmsUnmount => match target {
                UmsTarget::UmsVm => self.ums_vm_unmount().await,
                UmsTarget::UmsFt => self.ums_ft_unmount().await,
                _ => ums_resp_err("unmount requires ums_vm or ums_ft target"),
            },
            UmsOperation::UmsMount => {
                if req.image_path.is_empty() || req.image_path == "\n" {
                    return match target {
                        UmsTarget::UmsVm => self.ums_vm_unmount().await,
                        UmsTarget::UmsFt => self.ums_ft_unmount().await,
                        _ => ums_resp_err("mount with empty path requires ums_vm or ums_ft target"),
                    };
                }
                match target {
                    UmsTarget::UmsVm => self.ums_vm_mount(&req.image_path, req.vm_type).await,
                    UmsTarget::UmsFt => self.ums_ft_mount(&req.image_path).await,
                    _ => ums_resp_err("mount requires ums_vm or ums_ft target"),
                }
            }
            UmsOperation::UmsSwitchVmMode => {
                let mode = proto_vm_type_to_ums_mode(req.vm_type);
                self.ums_vm_switch_mode(mode).await
            }
        }
    }

    pub async fn apply_switches(
        &mut self,
        base_info: Option<UsbBaseInfo>,
        switches: Option<DeviceSwitches>,
    ) -> Result<(), UsbError> {
        let op_lock = self.gadget_op_lock.clone();
        let _guard = Self::try_acquire_gadget_op(&op_lock)?;
        self.apply_switches_locked(base_info, switches).await
    }

    async fn apply_switches_locked(
        &mut self,
        base_info: Option<UsbBaseInfo>,
        switches: Option<DeviceSwitches>,
    ) -> Result<(), UsbError> {
        if !self.emulation_enabled {
            return Err(UsbError::GadgetError("USB emulation disabled".into()));
        }

        self.request_change_approval().await?;

        info!("UsbDeviceManager: Change request approved, releasing resources");

        self.unbind_active_gadget().await?;
        debug!(
            "UsbDeviceManager: Resources released, rebuilding gadget with switches: {:?}",
            &switches
        );

        let old_base_info = self.base_info.clone();
        if let Some(base_info) = base_info {
            self.base_info = base_info;
        }

        let old_switches = self.switches.clone();
        if let Some(switches) = switches {
            self.switches = switches;
        }

        if let Err(e) = self.bind_gadget_from_current_switches().await {
            self.base_info = old_base_info;
            self.switches = old_switches;
            let fallback_result = self.bind_gadget_from_current_switches().await;
            warn!("UsbDeviceManager: bind_gadget_from_current_switches failed, falling back to old base info and switches result: {:?}", fallback_result);
            for handler in self.snapshot_change_handlers().await {
                let _ = handler.on_change_request(&ChangeRequest::ChangeCanceled).await;
            }
            return Err(e);
        }
        debug!("UsbDeviceManager: Gadget bound to UDC, now notifying handlers");

        for handler in self.snapshot_change_handlers().await {
            let _ = handler.on_change_request(&ChangeRequest::ChangeCompleted).await;
        }
        info!("UsbDeviceManager: Handlers notified, change completed");

        Ok(())
    }

    /// Update USB base info (same flow as apply_switches)
    pub async fn update_base_info(&mut self, info: UsbBaseInfo) -> Result<(), UsbError> {
        self.apply_switches(Some(info), None).await
    }

    /// get Virtual Media Service handle
    pub async fn get_virtual_meida(&self) -> Option<Arc<UmsController>> {
        let services = self.device_services.read().await;
        if let Some(DeviceService::Ums(controller, _)) = services.get(VIRTUAL_MEDIA_SERVICE) {
            Some(controller.clone())
        } else {
            None
        }
    }

    // get File Transfer Service handle
    pub async fn get_file_transfer(&self) -> Option<Arc<Ums1Controller>> {
        let services = self.device_services.read().await;
        if let Some(DeviceService::Ums1(controller, _)) = services.get(FILE_TRANSFER_SERVICE) {
            Some(controller.clone())
        } else {
            None
        }
    }

    pub async fn get_hid_kb_rel(&self) -> Option<Arc<HidKbRelController>> {
        let services = self.device_services.read().await;
        if let Some(DeviceService::HidKbRel(controller, _)) = services.get(HID_KB_REL_SERVICE) {
            Some(controller.clone())
        } else {
            None
        }
    }

    pub async fn get_hid_abs(&self) -> Option<Arc<HidAbsController>> {
        let services = self.device_services.read().await;
        if let Some(DeviceService::HidAbs(controller, _)) = services.get(HID_ABS_SERVICE) {
            Some(controller.clone())
        } else {
            None
        }
    }
}

impl UsbDeviceManager {
    async fn ums_vm_mount_state(&self) -> crate::proto::v1::UmsControlResponse {
        match self.get_virtual_meida().await {
            Some(ctrl) => match ctrl.get_state().await {
                Ok(state) => ums_resp_vm_state(state),
                Err(e) => ums_resp_err(e.to_string()),
            },
            None => ums_resp_vm_state(UmsVmState {
                mode: DEFAULT_UMS_MODE,
                mounted: false,
                mounted_path: None,
                enabled: false,
            }),
        }
    }

    async fn ums_ft_mount_state(&self) -> crate::proto::v1::UmsControlResponse {
        let services = self.device_services.read().await;
        match services.get(FILE_TRANSFER_SERVICE) {
            Some(DeviceService::Ums1(_, handle)) => {
                let path = handle.mounted_path().await.unwrap_or_default();
                ums_resp_mount_state(!path.is_empty(), path)
            }
            _ => ums_resp_mount_state(false, String::new()),
        }
    }

    async fn ums_vm_unmount(&self) -> crate::proto::v1::UmsControlResponse {
        match self.get_virtual_meida().await {
            Some(ctrl) => match ctrl.unmount().await {
                Ok(()) => {
                    info!("ums_vm unmount ok");
                    ums_resp_ok()
                }
                Err(e) => {
                    warn!("ums_vm unmount failed: {:?}", e);
                    ums_resp_err(e.to_string())
                }
            },
            None => ums_resp_ok(),
        }
    }

    async fn ums_ft_unmount(&self) -> crate::proto::v1::UmsControlResponse {
        match self.get_file_transfer().await {
            Some(ctrl) => match ctrl.unmount().await {
                Ok(()) => {
                    info!("ums_ft unmount ok");
                    ums_resp_ok()
                }
                Err(e) => {
                    warn!("ums_ft unmount failed: {:?}", e);
                    ums_resp_err(e.to_string())
                }
            },
            None => ums_resp_ok(),
        }
    }

    async fn ums_vm_mount(
        &mut self,
        image_path: &str,
        vm_type: i32,
    ) -> crate::proto::v1::UmsControlResponse {
        let mode = proto_vm_type_to_ums_mode(vm_type);
        let Some(ctrl) = self.get_virtual_meida().await else {
            return ums_resp_err("virtual media controller not initialized");
        };
        match ctrl.switch_mode(mode).await {
            Ok(needs_rebuild) if needs_rebuild => {
                if let Err(e) = self.apply_switches(None, None).await {
                    warn!("ums_vm mount apply_switches failed: {:?}", e);
                    return ums_resp_err(e.to_string());
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!("ums_vm switch_mode failed: {:?}", e);
                return ums_resp_err(e.to_string());
            }
        }
        match ctrl.mount(image_path).await {
            Ok(()) => {
                info!(path = image_path, "ums_vm mount ok");
                ums_resp_ok()
            }
            Err(e) => {
                warn!(path = image_path, error = ?e, "ums_vm mount failed");
                ums_resp_err(e.to_string())
            }
        }
    }

    async fn ums_ft_mount(&self, image_path: &str) -> crate::proto::v1::UmsControlResponse {
        let Some(ctrl) = self.get_file_transfer().await else {
            return ums_resp_err("file transfer controller not initialized");
        };
        match ctrl.mount(image_path).await {
            Ok(()) => {
                info!(path = image_path, "ums_ft mount ok");
                ums_resp_ok()
            }
            Err(e) => {
                warn!(path = image_path, error = ?e, "ums_ft mount failed");
                ums_resp_err(e.to_string())
            }
        }
    }

    async fn ums_vm_switch_mode(&mut self, mode: UmsMode) -> crate::proto::v1::UmsControlResponse {
        let Some(ctrl) = self.get_virtual_meida().await else {
            return ums_resp_err("virtual media controller not initialized");
        };
        let mode_for_log = mode.clone();
        match ctrl.switch_mode(mode).await {
            Ok(needs_rebuild) if needs_rebuild => {
                if let Err(e) = self.apply_switches(None, None).await {
                    warn!(?mode_for_log, error = ?e, "ums_vm switch_mode apply_switches failed");
                    return ums_resp_err(e.to_string());
                }
                info!(?mode_for_log, "ums_vm switch_mode ok (gadget rebuilt)");
                ums_resp_ok()
            }
            Ok(_) => {
                info!(?mode_for_log, "ums_vm switch_mode ok");
                ums_resp_ok()
            }
            Err(e) => {
                warn!(?mode_for_log, error = ?e, "ums_vm switch_mode failed");
                ums_resp_err(e.to_string())
            }
        }
    }

    async fn request_change_only(&self) -> Result<(), UsbError> {
        let mut error: Option<UsbError> = None;
        for handler in self.snapshot_change_handlers().await {
            match handler.on_change_request(&ChangeRequest::RequestChange).await {
                ChangeResponse::Reject(reason) => {
                    error = Some(UsbError::ChangeRejected(reason));
                    break;
                }
                _ => {}
            }
        }

        if let Some(error) = error {
            for handler in self.snapshot_change_handlers().await {
                let _ = handler.on_change_request(&ChangeRequest::ChangeCanceled).await;
            }
            warn!("UsbDeviceManager: Change preflight rejected, error: {}", error);
            return Err(error);
        }

        Ok(())
    }

    async fn request_change_approval(&self) -> Result<(), UsbError> {
        self.request_change_only().await?;

        let mut error: Option<UsbError> = None;
        for handler in self.snapshot_change_handlers().await {
            match handler.on_change_request(&ChangeRequest::PrepareChange).await {
                ChangeResponse::Reject(reason) => {
                    error = Some(UsbError::ChangeRejected(reason));
                    break;
                }
                _ => {}
            }
        }

        if let Some(error) = error {
            for handler in self.snapshot_change_handlers().await {
                let _ = handler.on_change_request(&ChangeRequest::ChangeCanceled).await;
            }
            warn!("UsbDeviceManager: Change request rejected, error: {}", error);
            return Err(error);
        }

        Ok(())
    }

    async fn apply_device_services(&self) -> Result<(UmsMode), UsbError> {
        let mut device_services = self.device_services.write().await;

        // apply Virtual Media Service
        let ums_mode = if let Some(DeviceService::Ums(_, ums_handle)) =
            device_services.get(VIRTUAL_MEDIA_SERVICE)
        {
            ums_handle.set_enabled(self.switches.ums_vm_enabled);
            ums_handle.get_mode().await
        } else {
            let (tx, rx) = mpsc::channel::<(UmsVmEvent, oneshot::Sender<UmsVmResponse>)>(8);
            let ums_handle = UmsHandle::new(DEFAULT_UMS_MODE, self.switches.ums_vm_enabled, rx);
            let _ = self.add_change_handler(ums_handle.clone()).await;
            device_services.insert(
                VIRTUAL_MEDIA_SERVICE,
                DeviceService::Ums(Arc::new(UmsController::new(tx)), ums_handle),
            );
            DEFAULT_UMS_MODE
        };

        // apply File Transfer Service
        if let Some(DeviceService::Ums1(_, handle)) = device_services.get(FILE_TRANSFER_SERVICE) {
            handle.set_enabled(self.switches.ums_ft_enabled);
        }
        else {
            let (tx, rx) = mpsc::channel::<(UmsFtEvent, oneshot::Sender<UmsFtResponse>)>(8);
            let ums1_handle = Ums1Handle::new(self.switches.ums_ft_enabled, rx);
            let _ = self.add_change_handler(ums1_handle.clone()).await;
            device_services.insert(
                FILE_TRANSFER_SERVICE,
                DeviceService::Ums1(Arc::new(Ums1Controller::new(tx)), ums1_handle),
            );
        }

        if let Some(DeviceService::Uac(_, uac_handle)) = device_services.get(UAC_SERVICE) {
            uac_handle.set_enabled(self.switches.uac1_enabled);
            uac_handle.set_process_enabled(self.mic_process_enabled);
        } else {
            let uac_handle = UacHandle::new(self.switches.uac1_enabled, self.mic_process_enabled);
            let _ = self.add_change_handler(uac_handle.clone()).await;
            device_services.insert(
                UAC_SERVICE,
                DeviceService::Uac(Arc::new(UacController::new(uac_handle.clone())), uac_handle),
            );
        }

        if let Some(DeviceService::HidKbRel(_, kb_handle)) = device_services.get(HID_KB_REL_SERVICE)
        {
            kb_handle.set_enabled(self.switches.hid_kb_rel_enabled);
        } else {
            let kb_handle = HidKbRelHandle::new(self.switches.hid_kb_rel_enabled);
            let _ = self.add_change_handler(kb_handle.clone()).await;
            device_services.insert(
                HID_KB_REL_SERVICE,
                DeviceService::HidKbRel(
                    Arc::new(HidKbRelController::new(kb_handle.clone())),
                    kb_handle,
                ),
            );
        }

        if let Some(DeviceService::HidAbs(_, abs_handle)) = device_services.get(HID_ABS_SERVICE) {
            abs_handle.set_enabled(self.switches.hid_abs_enabled);
        } else {
            let abs_handle = HidAbsHandle::new(self.switches.hid_abs_enabled);
            let _ = self.add_change_handler(abs_handle.clone()).await;
            device_services.insert(
                HID_ABS_SERVICE,
                DeviceService::HidAbs(
                    Arc::new(HidAbsController::new(abs_handle.clone())),
                    abs_handle,
                ),
            );
        }

        Ok(ums_mode)
    }

    async fn snapshot_change_handlers(&self) -> Vec<Arc<dyn UsbChangeHandler>> {
        self.change_handlers.read().await.values().cloned().collect()
    }

    fn start_udc_watch(&mut self) {
        let handlers = Arc::clone(&self.lifecycle_handlers);
        let state_cache = Arc::clone(&self.udc_state_cache);
        let state_path =
            crate::udc::udc_state_sysfs_path(&self.udc.name().to_string_lossy());

        let (tx, mut rx) = mpsc::channel::<Result<usb_gadget::UdcState, String>>(32);
        tokio::spawn(async move {
            info!("UsbDeviceManager: UDC state watch loop");
            let mut last_state: Option<UdcState> = None;
            let mut interval = tokio::time::interval(UDC_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    result = rx.recv() => {
                        match result {
                            Some(Ok(raw_state)) => {
                                report_udc_state_if_changed(
                                    raw_state,
                                    &mut last_state,
                                    &state_cache,
                                    &handlers,
                                )
                                .await;
                            }
                            Some(Err(msg)) => {
                                warn!("UsbDeviceManager: UDC state watch error: {}", msg);
                                let handlers = handlers.read().await;
                                for handler in handlers.values() {
                                    handler.on_lifecycle_event(&LifecycleEvent::Warning(msg.clone()));
                                }
                            }
                            None => break,
                        }
                    }
                    _ = interval.tick() => {
                        match crate::udc::read_udc_state_sysfs(&state_path).await {
                            Ok(raw_state) => {
                                report_udc_state_if_changed(
                                    raw_state,
                                    &mut last_state,
                                    &state_cache,
                                    &handlers,
                                )
                                .await;
                            }
                            Err(msg) => {
                                warn!("UsbDeviceManager: UDC state poll error: {}", msg);
                                let handlers = handlers.read().await;
                                for handler in handlers.values() {
                                    handler.on_lifecycle_event(&LifecycleEvent::Warning(msg.clone()));
                                }
                            }
                        }
                    }
                }
            }
            info!("UsbDeviceManager: UDC state watch loop end");
        });

        match UdcStateWatch::start(self.udc.clone(), tx) {
            Ok(watch) => {
                self.udc_watch = Some(watch);
                info!("UsbDeviceManager: UDC state watch started");
            }
            Err(e) => {
                warn!("UsbDeviceManager: failed to start UDC watch: {}", e);
                self.udc_watch = None;
            }
        }
    }

    async fn wait_for_udc_recovered(&self) -> Result<(), UsbError> {
        let deadline = Instant::now() + UDC_RECOVER_TIMEOUT;
        loop {
            if let Some(state) = self.get_udc_state().await {
                if is_recovered_state(state) {
                    return Ok(());
                }
            }

            if let Ok(raw) = self.udc.state() {
                if let Some(state) = map_udc_state(raw) {
                    {
                        let mut cache = self.udc_state_cache.write().await;
                        *cache = Some(state.clone());
                    }

                    if is_recovered_state(state) {
                        return Ok(());
                    }
                }
            }

            if Instant::now() >= deadline {
                let last = self.get_udc_state().await;
                warn!(
                    last_state = ?last,
                    timeout_secs = UDC_RECOVER_TIMEOUT.as_secs(),
                    "UsbDeviceManager: timed out waiting for UDC NotAttached before rebuild"
                );
                return Err(UsbError::UDCError(format!(
                    "USB gadget busy: timed out waiting UDC recover (last state: {:?})",
                    last
                )));
            }

            tokio::time::sleep(UDC_RECOVER_CHECK_INTERVAL).await;
        }
    }
}

async fn report_udc_state_if_changed(
    raw_state: RawUdcState,
    last_state: &mut Option<UdcState>,
    state_cache: &Arc<RwLock<Option<UdcState>>>,
    handlers: &Arc<RwLock<HashMap<String, Arc<dyn UsbLifecycleHandler>>>>,
) {
    if let Some(state) = map_udc_state(raw_state) {
        if *last_state == Some(state) {
            return;
        }
        info!("UsbDeviceManager: UDC state changed: {:?}", raw_state);
        *last_state = Some(state);
        *state_cache.write().await = Some(state);

        let handlers = handlers.read().await;
        for handler in handlers.values() {
            handler.on_lifecycle_event(&LifecycleEvent::UdcStateChanged(state));
        }
    } else {
        let handlers = handlers.read().await;
        let msg = format!("unsupported UDC state: {}", raw_state);
        for handler in handlers.values() {
            handler.on_lifecycle_event(&LifecycleEvent::Warning(msg.clone()));
        }
    }
}

fn map_udc_state(raw: RawUdcState) -> Option<UdcState> {
    match raw {
        RawUdcState::NotAttached => Some(UdcState::NotAttached),
        RawUdcState::Attached => Some(UdcState::Attached),
        RawUdcState::Powered => Some(UdcState::Powered),
        RawUdcState::Default => Some(UdcState::Default),
        RawUdcState::Addressed => Some(UdcState::Address),
        RawUdcState::Configured => Some(UdcState::Configured),
        RawUdcState::Suspended => Some(UdcState::Suspended),
        _ => None,
    }
}

fn is_recovered_state(state: UdcState) -> bool {
    matches!(state, UdcState::NotAttached)
}

fn proto_vm_type_to_ums_mode(vm_type: i32) -> UmsMode {
    use crate::proto::v1::UmsVmType;
    if vm_type == UmsVmType::VmDisk as i32 {
        UmsMode::Disk
    } else {
        UmsMode::CdRom
    }
}

fn ums_mode_to_proto(mode: UmsMode) -> i32 {
    use crate::proto::v1::UmsVmType;
    match mode {
        UmsMode::Disk => UmsVmType::VmDisk as i32,
        UmsMode::CdRom => UmsVmType::VmCdRom as i32,
    }
}

fn ums_resp_ok() -> crate::proto::v1::UmsControlResponse {
    crate::proto::v1::UmsControlResponse {
        ok: true,
        error: None,
        mounted: false,
        mounted_path: String::new(),
        vm_type: crate::proto::v1::UmsVmType::Unknown as i32,
    }
}

fn ums_resp_vm_state(state: UmsVmState) -> crate::proto::v1::UmsControlResponse {
    crate::proto::v1::UmsControlResponse {
        ok: true,
        error: None,
        mounted: state.mounted,
        mounted_path: state.mounted_path.unwrap_or_default(),
        vm_type: ums_mode_to_proto(state.mode),
    }
}

fn ums_resp_mount_state(mounted: bool, mounted_path: String) -> crate::proto::v1::UmsControlResponse {
    crate::proto::v1::UmsControlResponse {
        ok: true,
        error: None,
        mounted,
        mounted_path,
        vm_type: crate::proto::v1::UmsVmType::Unknown as i32,
    }
}

fn ums_resp_err(message: impl Into<String>) -> crate::proto::v1::UmsControlResponse {
    crate::proto::v1::UmsControlResponse {
        ok: false,
        error: Some(message.into()),
        mounted: false,
        mounted_path: String::new(),
        vm_type: crate::proto::v1::UmsVmType::Unknown as i32,
    }
}
