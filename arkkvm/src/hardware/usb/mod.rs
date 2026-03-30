//! USB gadget subsystem facade.
//!
//! - Periodically poll USB state from kernel `udc` state file
//! - Expose HID operations (keyboard/mouse)
//! - Provide keyboard LED state callback registration
//! - Upper-layer hooks for event broadcast
//! - USB gadget configuration and management

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OnceCell, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};

pub mod descriptors;
pub mod gadget;
pub mod hid;
pub use gadget::{GadgetConfig, UsbGadget};
pub use hid::{Hid, KeyboardState};

use crate::config::get_config_manager;
use crate::config::types::{UsbConfig, UsbDevices};
use crate::hardware::usb::gadget::UsbDeviceType;
use crate::services;
pub mod mic;
pub mod storage;

/// Simple USB state reader for UDC
#[derive(Debug, Clone)]
pub struct UsbState {
    pub state: String,
}

impl Default for UsbState {
    fn default() -> Self {
        Self { state: "unknown".to_string() }
    }
}

lazy_static::lazy_static! {
    static ref USB_MANAGER: Arc<RwLock<Option<UsbManager>>> = Arc::new(RwLock::new(None));
    static ref HID: OnceCell<Hid> = OnceCell::new();
}

/// Get global USB manager if initialized
pub fn get_usb_manager() -> Arc<RwLock<Option<UsbManager>>> {
    USB_MANAGER.clone()
}

pub async fn init_hid() {
    let _ = HID.get_or_init(async || {
        let hid = Hid::default();
        if let Err(e) = hid.start_keyboard_led_monitor().await {
            warn!("failed to open keyboard HID file: {}", e);
        }
        hid
    }).await;
}

pub fn get_hid() -> Option<&'static Hid> {
    HID.get()
}

/// Initialize global USB manager, start polling, and wire keyboard LED to RPC
pub async fn init_usb(config: UsbConfig, devices: UsbDevices) -> anyhow::Result<()> {
    info!("Initializing USB manager");
    let udc_name = std::fs::read_dir("/sys/class/udc")
        .ok()
        .and_then(|it| it.flatten().next())
        .and_then(|e| e.file_name().into_string().ok())
        .unwrap_or_else(|| {
            tracing::warn!("no UDC found; USB emulation will be disabled until UDC appears");
            "unknown".to_string()
        });

    let mut mgr = UsbManager::new(udc_name);

    // Initialize USB gadget with default configuration. Do not fail overall if this errors.
    if let Err(err) = mgr.init_gadget("arkkvm".to_string(), devices, config) {
        tracing::warn!("failed to initialize USB gadget: {}", err);
    }

    // mgr.update_hid();

    // Start polling
    mgr.start_polling().await;

    *USB_MANAGER.write().await = Some(mgr);

    init_hid().await;

    info!("USB manager initialized");
    Ok(())
}

pub async fn reboot_usb_manager_by_device(
    device_type: UsbDeviceType,
    enable: bool,
) -> anyhow::Result<()> {
    warn!("reboot_usb_manager_by_device");
    let mut deivces = get_config_manager().get_usb_devices().await;
    warn!("reboot_usb_manager_by_device get config manager: {:?}", deivces);

    match device_type {
        UsbDeviceType::AbsoluteMouse => deivces.absolute_mouse = enable,
        UsbDeviceType::RelativeMouse => deivces.relative_mouse = enable,
        UsbDeviceType::Keyboard => deivces.keyboard = enable,
        UsbDeviceType::MassStorageVm => deivces.mass_storage_vm = enable,
        UsbDeviceType::MassStorageFt => deivces.mass_storage_ft = enable,
        UsbDeviceType::Microphone => deivces.microphone = enable,
        UsbDeviceType::Camera => deivces.camera = enable,
    }

    warn!("reboot_usb_manager_by_device reboot manager");
    reboot_usb_manager(None, Some(deivces)).await
}

pub async fn reboot_usb_manager(
    mut usb_config: Option<UsbConfig>,
    mut devices: Option<UsbDevices>,
) -> anyhow::Result<()> {
    warn!("reboot_usb_manager");
    let config = get_config_manager();

    let usb_config = if let Some(usb_config) = usb_config.take() {
        usb_config
    } else {
        config.get_usb_config().await
    };

    let devices =
        if let Some(devices) = devices.take() { devices } else { config.get_usb_devices().await };

    // warn!("reboot_usb_manager try remove manager");
    // remove_usb_manager().await?;
    // warn!("reboot_usb_manager try init usb");
    // init_usb(usb_config.clone(), devices.clone()).await?;

    warn!("reboot_usb_manager try save config");
    config.set_usb_config(usb_config).await?;
    config.set_usb_devices(&devices).await;

    if devices.microphone {
        services::init_virtual_mic_service().await?;
    }
    else {
        services::uninit_virtual_mic_service().await?;
    }
    Ok(())
}

async fn remove_usb_manager() -> anyhow::Result<()> {
    if let Some(manager) = USB_MANAGER.write().await.take() {
        manager.stop_polling().await
    }
    Ok(())
}

/// USB manager combining UDC state polling and HID access
pub struct UsbManager {
    udc_name: String,
    udc_state_path: String,
    state: Arc<RwLock<UsbState>>,
    poll_cancel: CancellationToken,
    poll_handle: RwLock<Option<JoinHandle<()>>>,
    gadget: Option<UsbGadget>,
}

impl Drop for UsbManager {
    fn drop(&mut self) {
        // self.hid = Arc::new(Hid::default());
        self.gadget.take();
    }
}

impl UsbManager {
    /// Create with UDC name and default HID device paths
    pub fn new(udc_name: String) -> Self {
        let udc_state_path = format!("/sys/class/udc/{}/state", udc_name);
        Self {
            udc_name,
            udc_state_path,
            state: Arc::new(RwLock::new(UsbState::default())),
            poll_cancel: CancellationToken::new(),
            poll_handle: RwLock::new(None),
            gadget: None,
        }
    }

    /// Initialize USB gadget with specified configuration
    pub fn init_gadget(
        &mut self,
        name: String,
        devices: UsbDevices,
        config: UsbConfig,
    ) -> anyhow::Result<()> {
        info!("initializing USB gadget: {}", name);

        let gadget = UsbGadget::new(name, devices, config)?;
        gadget.init()?;
        info!("USB gadget Initialize Done");
        self.gadget = Some(gadget);
        Ok(())
    }

    /// Get USB gadget if initialized
    // pub fn get_gadget(&self) -> &Option<UsbGadget> {
    //     self.gadget
    // }

    // fn reset_hid(&mut self) -> anyhow::Result<()> {
    //     self.hid = Arc::new(Hid::default());
    //     self.update_hid();
    //     Ok(())
    // }

    // pub fn update_hid(&self) {
    //     // Optimize: reduce lock contention by getting HID reference once
    //     let hid = self.hid();

    //     // Set keyboard LED state change callback
    //     hid.set_on_keyboard_state_change(|state| {
    //         tokio::spawn(async move {
    //             crate::jsonrpc::broadcast_keyboard_led_state(state).await;
    //         });
    //     });

    //     // Open keyboard HID file with better error handling
    //     if let Err(e) = hid.start_keyboard_led_monitor() {
    //         tracing::warn!("failed to open keyboard HID file: {}", e);
    //         // Continue initialization even if HID file fails
    //     }
    // }

    // pub fn get_last_input_time(&self) -> Instant {
    //     self.hid.get_last_user_input_time()
    // }

    /// Start background UDC state polling loop
    pub async fn start_polling(&self) {
        if self.poll_handle.read().await.is_some() {
            return;
        }

        let state_path = self.udc_state_path.clone();
        let cancel = self.poll_cancel.clone();
        let state_ref = Arc::clone(&self.state);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            let mut last_state = String::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        trace!("usb state poll cancelled");
                        break;
                    }
                    _ = interval.tick() => {
                        // Optimize: only read file if path exists
                        if !Path::new(&state_path).exists() {
                            continue;
                        }

                        let new_state = match read_trimmed(&state_path) {
                            Ok(state) => state,
                            Err(_) => "unknown".to_string(),
                        };

                        if new_state != last_state {
                            let prev_state = std::mem::replace(&mut last_state, new_state.clone());
                            *state_ref.write().await = UsbState { state: new_state.clone() };

                            info!(from = %prev_state, to = %new_state, "USB state changed");

                            // Broadcast via RPC and request display update
                            crate::jsonrpc::broadcast_usb_state(new_state).await;
                            // let _ = crate::hardware::display::request_display_update(true).await;
                        }
                    }
                }
            }
        });
        *self.poll_handle.write().await = Some(handle);
    }

    /// Stop polling loop
    pub async fn stop_polling(&self) {
        self.poll_cancel.cancel();
        let handle_opt = { self.poll_handle.write().await.take() };
        if let Some(h) = handle_opt {
            let _ = h.await;
        }
    }

    /// Get current USB state string
    pub async fn get_usb_state(&self) -> String {
        self.state.read().await.state.clone()
    }

    /// Access HID
    // pub fn hid(&self) -> Arc<Hid> {
    //     self.hid.clone()
    // }

    /// Get the UDC name associated with this manager
    pub fn get_udc_name(&self) -> &str {
        &self.udc_name
    }

    // pub async fn set_usb_device_enable(
    //     &mut self,
    //     device_type: UsbDeviceType,
    //     enable: bool,
    // ) -> anyhow::Result<()> {
    //     let gadget = &mut self.gadget;
    //     let Some(gadget) = gadget.as_mut() else {
    //         return Err(anyhow::anyhow!("No gadget"));
    //     };
    //     gadget.update_usb_device_config(device_type, enable).await?;
    //     self.reset_hid()
    // }

    // pub async fn set_usb_devices_enable_old(
    //     &mut self,
    //     device_state: &UsbDevicesState,
    // ) -> anyhow::Result<()> {
    //     let gadget = &mut self.gadget;
    //     let Some(gadget) = gadget.as_mut() else {
    //         return Err(anyhow::anyhow!("No gadget"));
    //     };
    //     gadget.update_usb_device_state(&device_state).await?;
    //     self.reset_hid()
    // }

    // pub async fn set_oobe_device_settings(
    //     &mut self,
    //     oobe_settings: &SetupRequest,
    // ) -> anyhow::Result<()> {
    //     let gadget = &mut self.gadget;
    //     let Some(gadget) = gadget.as_mut() else {
    //         return Err(anyhow::anyhow!("No gadget"));
    //     };
    //     gadget.set_oobe_usb_debices_settings(&oobe_settings).await?;
    //     self.reset_hid()
    // }

    pub async fn get_usb_config(&self) -> anyhow::Result<UsbConfig> {
        let Some(gadget) = self.gadget.as_ref() else {
            return Err(anyhow::anyhow!("No gadget"));
        };
        Ok(gadget.get_usb_config())
    }

    // pub async fn set_usb_config(&mut self, config: UsbConfig) -> anyhow::Result<()> {
    //     let gadget = &mut self.gadget;
    //     let Some(gadget) = gadget.as_mut() else {
    //         return Err(anyhow::anyhow!("No gadget"));
    //     };
    //     gadget.update_usb_config(config).await
    // }
}

pub async fn get_current_usb_state() -> String {
    let manager = get_usb_manager();
    let manager = manager.read().await;
    let Some(mgr) = manager.as_ref() else {
        return "unknown".to_string();
    };
    mgr.get_usb_state().await
}

fn read_trimmed(path: &str) -> anyhow::Result<String> {
    if !Path::new(path).exists() {
        anyhow::bail!("path not found: {}", path);
    }
    let content = std::fs::read_to_string(path)?;
    Ok(content.trim().to_string())
}

// Initialize USB gadget with custom configuration
// pub fn init_usb_gadget(
//     name: String,
//     devices: UsbDevices,
//     config: GadgetConfig,
// ) -> anyhow::Result<()> {
//     if let Some(manager) = get_usb_manager() {
//         let mut mgr = manager.write();
//         mgr.init_gadget(name, devices, config)?;
//         Ok(())
//     } else {
//         Err(anyhow::anyhow!("USB manager not initialized"))
//     }
// }
