//! USB gadget configuration and management.
//!
//! This module provides the core USB gadget functionality including:
//! - USB gadget creation and configuration
//! - HID device setup (keyboard, mouse)
//! - Mass storage device configuration
//! - ConfigFS management and UDC binding
//!
//! Safety:
//! - All file operations use standard library for simplicity
//! - Error handling follows Rust best practices
//! - Resource management is automatic through Rust ownership

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use super::descriptors::HID_REPORT_DESC;
use crate::config::types::{UsbConfig, UsbDevices};

pub enum UsbDeviceType {
    AbsoluteMouse,
    RelativeMouse,
    Keyboard,
    MassStorageVm,
    MassStorageFt,
    Microphone,
    Camera,
}

/// USB gadget configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GadgetConfig {
    pub vendor_id: String,
    pub product_id: String,
    pub serial_number: String,
    pub manufacturer: String,
    pub product: String,
    pub strict_mode: bool,
}

/// USB gadget configuration item
#[derive(Debug, Clone)]
struct GadgetConfigItem {
    order: u32,
    path: Vec<String>,
    attrs: HashMap<String, String>,
    config_attrs: HashMap<String, String>,
    config_path: Option<Vec<String>>,
    report_desc: Option<Vec<u8>>,
}

/// USB gadget manager
pub struct UsbGadget {
    name: String,
    udc: String,
    kvm_gadget_path: PathBuf,
    config_c1_path: PathBuf,
    config_map: HashMap<String, GadgetConfigItem>,
    custom_config: GadgetConfig,
    enabled_devices: UsbDevices,
}

impl UsbGadget {
    /// Create a new USB gadget
    pub fn new(name: String, enabled_devices: UsbDevices, config: UsbConfig) -> Result<Self> {
        let udc = Self::get_udc()?;
        let kvm_gadget_path = PathBuf::from("/sys/kernel/config/usb_gadget").join(&name);
        let config_c1_path = kvm_gadget_path.join("configs/c.1");

        let mut gadget = Self {
            name,
            udc,
            kvm_gadget_path,
            config_c1_path,
            config_map: Self::create_default_config_map(),
            custom_config: GadgetConfig {
                vendor_id: config.vendor_id,
                product_id: config.product_id,
                serial_number: config.serial_number,
                manufacturer: config.manufacturer,
                product: config.product,
                strict_mode: false,
            },
            enabled_devices,
        };
        info!("Try to load gadget config");
        gadget.load_gadget_config();
        Ok(gadget)
    }

    /// Initialize the USB gadget
    pub fn init(&self) -> Result<()> {
        let udcs = Self::get_udcs();
        if udcs.is_empty() {
            return Err(anyhow!("no UDC found, skipping USB stack init"));
        }

        Self::force_unbind_conflicting_gadgets(&self.udc, &self.name);

        self.configure_usb_gadget(false)?;
        info!("USB gadget initialized successfully");
        Ok(())
    }

    // pub async fn update_usb_device_config(
    //     &mut self,
    //     device_type: UsbDeviceType,
    //     enable: bool,
    // ) -> Result<()> {
    //     match device_type {
    //         UsbDeviceType::AbsoluteMouse => self.enabled_devices.absolute_mouse = enable,
    //         UsbDeviceType::RelativeMouse => self.enabled_devices.relative_mouse = enable,
    //         UsbDeviceType::Keyboard => self.enabled_devices.keyboard = enable,
    //         UsbDeviceType::MassStorageVm => self.enabled_devices.mass_storage_vm = enable,
    //         UsbDeviceType::MassStorageFt => self.enabled_devices.mass_storage_ft = enable,
    //         UsbDeviceType::Microphone => self.enabled_devices.microphone = enable,
    //         UsbDeviceType::Camera => self.enabled_devices.camera = enable,
    //     }
    //     self.update_gadget_config()?;

    //     let config = get_config_manager();
    //     config.set_usb_devices(&self.enabled_devices).await
    // }

    // pub async fn update_usb_device_state(&mut self, device_state: &UsbDevicesState) -> Result<()> {
    //     self.enabled_devices.absolute_mouse = device_state.absolute_mouse;
    //     self.enabled_devices.relative_mouse = device_state.relative_mouse;
    //     self.enabled_devices.keyboard = device_state.keyboard;
    //     self.enabled_devices.mass_storage_vm = device_state.mass_storage;

    //     self.update_gadget_config()?;

    //     let config = get_config_manager();
    //     config.set_usb_devices_state(&device_state).await
    // }

    // pub async fn set_oobe_usb_debices_settings(&mut self, request: &SetupRequest) -> Result<()> {
    //     self.enabled_devices.microphone = request.microphone_emulation;
    //     self.enabled_devices.camera = request.camera_emulation;
    //     self.enabled_devices.mass_storage_vm = request.file_transfer;

    //     self.update_gadget_config()?;

    //     let config = get_config_manager();
    //     config
    //         .set_oobe_settings(
    //             self.enabled_devices.microphone,
    //             self.enabled_devices.camera,
    //             self.enabled_devices.mass_storage_ft,
    //             request.audio_playback,
    //         )
    //         .await
    // }

    pub fn get_usb_config(&self) -> UsbConfig {
        UsbConfig {
            product: self.custom_config.product.clone(),
            vendor_id: self.custom_config.vendor_id.clone(),
            product_id: self.custom_config.product_id.clone(),
            serial_number: self.custom_config.serial_number.clone(),
            manufacturer: self.custom_config.manufacturer.clone(),
        }
    }

    // pub async fn update_usb_config(&mut self, usb_config: UsbConfig) -> Result<()> {
    //     self.custom_config.product = usb_config.product.clone();
    //     self.custom_config.manufacturer = usb_config.manufacturer.clone();
    //     self.custom_config.serial_number = usb_config.serial_number.clone();
    //     self.custom_config.product_id = usb_config.product_id.clone();
    //     self.custom_config.vendor_id = usb_config.vendor_id.clone();

    //     self.load_gadget_config();
    //     self.update_gadget_config()?;

    //     let config = get_config_manager();
    //     config.set_usb_config(usb_config).await
    // }

    /// Update gadget configuration
    fn update_gadget_config(&self) -> Result<()> {
        info!("updating USB gadget configuration");
        self.configure_usb_gadget(true)?;
        info!("USB gadget configuration updated");
        Ok(())
    }

    /// Get current USB state
    pub fn get_usb_state(&self) -> String {
        let state_file = PathBuf::from("/sys/class/udc").join(&self.udc).join("state");

        std::fs::read_to_string(&state_file)
            .map(|content| content.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Check if UDC is bound
    pub fn is_udc_bound(&self) -> Result<bool> {
        let udc_file_path = PathBuf::from("/sys/bus/platform/drivers/dwc3").join(&self.udc);
        Ok(udc_file_path.exists())
    }

    /// Bind UDC
    /// UDC binding is handled by the startup script
    pub fn bind_udc(&self) -> Result<()> {
        // let bind_path = PathBuf::from("/sys/bus/platform/drivers/dwc3/bind");
        // std::fs::write(&bind_path, &self.udc)
        //     .with_context(|| format!("failed to bind UDC: {}", self.udc))?;
        Ok(())
    }

    /// Unbind UDC
    /// Binding cannot be removed
    pub fn unbind_udc(&self) -> Result<()> {
        // let udc_file = self.kvm_gadget_path.join("UDC");
        // std::fs::write(&udc_file, "").with_context(|| {
        //     format!("failed to unbind gadget from UDC via {}", udc_file.display())
        // })?;
        Ok(())
    }

    /// Get UDC name
    pub fn get_udc_name(&self) -> &str {
        &self.udc
    }

    /// Get gadget path
    pub fn get_gadget_path(&self) -> &Path {
        &self.kvm_gadget_path
    }

    /// Get config path
    pub fn get_config_path(&self) -> &Path {
        &self.config_c1_path
    }

    /// Override gadget config for a specific item and attribute
    pub fn override_gadget_config(
        &mut self,
        item_key: &str,
        item_attr: &str,
        value: String,
    ) -> Result<bool> {
        let item = self
            .config_map
            .get_mut(item_key)
            .ok_or_else(|| anyhow!("config item {} not found", item_key))?;

        if item.attrs.get(item_attr) == Some(&value) {
            return Ok(false);
        }

        item.attrs.insert(item_attr.to_string(), value);
        info!(item_key, item_attr, "overriding gadget config");
        Ok(true)
    }

    // Private methods

    fn get_udc() -> Result<String> {
        let udcs = Self::get_udcs();
        udcs.into_iter().next().ok_or_else(|| anyhow!("no UDC found"))
    }

    fn get_udcs() -> Vec<String> {
        let mut udcs: Vec<String> = Vec::new();

        if let Ok(entries) = std::fs::read_dir("/sys/devices/platform/usbdrd") {
            for e in entries.flatten() {
                if let Ok(ft) = e.file_type()
                    && ft.is_dir()
                    && let Some(name) = e.file_name().to_str()
                    && name.ends_with(".usb")
                {
                    udcs.push(name.to_string());
                }
            }
            if !udcs.is_empty() {
                return udcs;
            }
        }

        if let Ok(entries) = std::fs::read_dir("/sys/class/udc") {
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    udcs.push(name.to_string());
                }
            }
        }

        udcs
    }

    fn create_default_config_map() -> HashMap<String, GadgetConfigItem> {
        let mut config_map = HashMap::new();

        // Base configuration
        config_map.insert(
            "base".to_string(),
            GadgetConfigItem {
                order: 0,
                path: Vec::new(),
                attrs: HashMap::from([
                    ("bcdUSB".to_string(), "0x0200".to_string()), // USB 2.0
                    ("idVendor".to_string(), "0x1d6b".to_string()), // The Linux Foundation
                    ("idProduct".to_string(), "0x0104".to_string()), // Multifunction Composite Gadget
                    ("bcdDevice".to_string(), "0x0100".to_string()), // USB2
                ]),
                config_attrs: HashMap::new(),
                config_path: None,
                report_desc: None,
            },
        );

        // Base info
        config_map.insert(
            "base_info".to_string(),
            GadgetConfigItem {
                order: 1,
                path: vec!["strings".to_string(), "0x409".to_string()],
                attrs: HashMap::from([
                    ("serialnumber".to_string(), String::new()),
                    ("manufacturer".to_string(), "ArkKVM".to_string()),
                    ("product".to_string(), "ArkKVM USB Emulation Device".to_string()),
                ]),
                config_attrs: HashMap::from([(
                    "configuration".to_string(),
                    "Config 1: HID".to_string(),
                )]),
                config_path: Some(vec!["strings".to_string(), "0x409".to_string()]),
                report_desc: None,
            },
        );

        // Config c.1 root attributes (e.g., MaxPower)
        config_map.insert(
            "config_c1".to_string(),
            GadgetConfigItem {
                order: 2,
                path: Vec::new(),
                attrs: HashMap::new(),
                config_attrs: HashMap::from([("MaxPower".to_string(), "250".to_string())]),
                config_path: Some(Vec::new()),
                report_desc: None,
            },
        );

        // Keyboard HID
        config_map.insert(
            "keyboard".to_string(),
            GadgetConfigItem {
                order: 1000,
                path: vec!["functions".to_string(), "hid.usb0".to_string()],
                attrs: HashMap::from([
                    ("protocol".to_string(), "1".to_string()),
                    ("subclass".to_string(), "1".to_string()),
                    ("report_length".to_string(), "8".to_string()),
                    ("no_out_endpoint".to_string(), "0".to_string()),
                ]),
                config_attrs: HashMap::new(),
                config_path: Some(vec!["hid.usb0".to_string()]),
                report_desc: Some(HID_REPORT_DESC.to_vec()),
            },
        );

        // // Absolute mouse HID
        // config_map.insert(
        //     "absolute_mouse".to_string(),
        //     GadgetConfigItem {
        //         order: 1001,
        //         path: vec!["functions".to_string(), "hid.usb1".to_string()],
        //         attrs: HashMap::from([
        //             ("protocol".to_string(), "2".to_string()),
        //             ("subclass".to_string(), "0".to_string()),
        //             ("report_length".to_string(), "6".to_string()),
        //             ("no_out_endpoint".to_string(), "1".to_string()),
        //         ]),
        //         config_attrs: HashMap::new(),
        //         config_path: Some(vec!["hid.usb1".to_string()]),
        //         report_desc: Some(ABSOLUTE_MOUSE_REPORT_DESC.to_vec()),
        //     },
        // );

        // // Relative mouse HID
        // config_map.insert(
        //     "relative_mouse".to_string(),
        //     GadgetConfigItem {
        //         order: 1002,
        //         path: vec!["functions".to_string(), "hid.usb2".to_string()],
        //         attrs: HashMap::from([
        //             ("protocol".to_string(), "2".to_string()),
        //             ("subclass".to_string(), "1".to_string()),
        //             ("report_length".to_string(), "4".to_string()),
        //             ("no_out_endpoint".to_string(), "1".to_string()),
        //         ]),
        //         config_attrs: HashMap::new(),
        //         config_path: Some(vec!["hid.usb2".to_string()]),
        //         report_desc: Some(RELATIVE_MOUSE_REPORT_DESC.to_vec()),
        //     },
        // );

        // Mass storage base
        config_map.insert(
            "mass_storage_base".to_string(),
            GadgetConfigItem {
                order: 3000,
                path: vec!["functions".to_string(), "mass_storage.usb0".to_string()],
                attrs: HashMap::from([("stall".to_string(), "1".to_string())]),
                config_attrs: HashMap::new(),
                config_path: Some(vec!["mass_storage.usb0".to_string()]),
                report_desc: None,
            },
        );

        // Mass storage usb0 lun0
        config_map.insert(
            "mass_storage_lun0".to_string(),
            GadgetConfigItem {
                order: 3001,
                path: vec![
                    "functions".to_string(),
                    "mass_storage.usb0".to_string(),
                    "lun.0".to_string(),
                ],
                attrs: HashMap::from([
                    ("cdrom".to_string(), "1".to_string()),
                    ("ro".to_string(), "1".to_string()),
                    ("removable".to_string(), "1".to_string()),
                    ("file".to_string(), "\n".to_string()),
                    ("inquiry_string".to_string(), "ArkKVM Virtual Media".to_string()),
                ]),
                config_attrs: HashMap::new(),
                config_path: None,
                report_desc: None,
            },
        );

        // Mass storage base1
        config_map.insert(
            "mass_storage_base1".to_string(),
            GadgetConfigItem {
                order: 3002,
                path: vec!["functions".to_string(), "mass_storage.usb1".to_string()],
                attrs: HashMap::from([("stall".to_string(), "1".to_string())]),
                config_attrs: HashMap::new(),
                config_path: Some(vec!["mass_storage.usb1".to_string()]),
                report_desc: None,
            },
        );

        // Mass storage usb1 lun0
        config_map.insert(
            "mass_storage_lun1".to_string(),
            GadgetConfigItem {
                order: 3003,
                path: vec![
                    "functions".to_string(),
                    "mass_storage.usb1".to_string(),
                    "lun.0".to_string(),
                ],
                attrs: HashMap::from([
                    ("cdrom".to_string(), "0".to_string()),
                    ("ro".to_string(), "0".to_string()),
                    ("removable".to_string(), "1".to_string()),
                    ("file".to_string(), "\n".to_string()),
                    ("inquiry_string".to_string(), "ArkKVM File Transfer".to_string()),
                ]),
                config_attrs: HashMap::new(),
                config_path: None,
                report_desc: None,
            },
        );

        // Microphone (Audio) configuration
        config_map.insert(
            "microphone".to_string(),
            GadgetConfigItem {
                order: 4000,
                path: vec!["functions".to_string(), "uac1.mic".to_string()],
                attrs: HashMap::from([
                    ("c_chmask".to_string(), "0".to_string()),    // Stereo
                    ("c_srate".to_string(), "48000".to_string()), // 48kHz
                    ("c_ssize".to_string(), "2".to_string()),     // 16-bit
                    ("p_chmask".to_string(), "3".to_string()),
                    ("p_srate".to_string(), "48000".to_string()),
                    ("p_ssize".to_string(), "2".to_string()),
                ]),
                config_attrs: HashMap::new(),
                config_path: Some(vec!["uac1.mic".to_string()]),
                report_desc: None,
            },
        );

        config_map
    }

    fn load_gadget_config(&mut self) {
        if self.custom_config.strict_mode {
            return;
        }

        // Update vendor and product IDs
        if let Some(base) = self.config_map.get_mut("base") {
            base.attrs.insert("idVendor".to_string(), self.custom_config.vendor_id.clone());
            base.attrs.insert("idProduct".to_string(), self.custom_config.product_id.clone());
        }

        // Update strings
        if let Some(base_info) = self.config_map.get_mut("base_info") {
            base_info
                .attrs
                .insert("serialnumber".to_string(), self.custom_config.serial_number.clone());
            base_info
                .attrs
                .insert("manufacturer".to_string(), self.custom_config.manufacturer.clone());
            base_info.attrs.insert("product".to_string(), self.custom_config.product.clone());
        }
    }

    fn is_gadget_config_item_enabled(&self, item_key: &str) -> bool {
        match item_key {
            "absolute_mouse" => self.enabled_devices.absolute_mouse,
            "relative_mouse" => self.enabled_devices.relative_mouse,
            "keyboard" => self.enabled_devices.keyboard,
            "mass_storage_base" => self.enabled_devices.mass_storage_vm,
            "mass_storage_lun0" => self.enabled_devices.mass_storage_vm,
            "mass_storage_base1" => self.enabled_devices.mass_storage_ft,
            "mass_storage_lun1" => self.enabled_devices.mass_storage_ft,
            "microphone" => self.enabled_devices.microphone,
            _ => true,
        }
    }

    fn configure_usb_gadget(&self, reset_usb: bool) -> Result<()> {
        warn!("Configuring USB gadget");
        if let Err(e) = self.mount_configfs() {
            error!("Failed to mount configfs: {:?}", &e);
            return Err(e);
        }

        warn!("Writing gadget config");
        if let Err(e) = self.write_gadget_config() {
            error!("Failed to write gadget config: {:?}", &e);
            return Err(e);
        }

        if reset_usb {
            if let Err(e) = self.rebind_usb(true) {
                error!("Failed to rebind USB: {:?}", &e);
                return Err(e);
            }
        }
        Ok(())
    }

    // Mount configfs is handled by the startup script
    fn mount_configfs(&self) -> Result<()> {
        // let configfs_path = Path::new("/sys/kernel/config");
        // // Ensure directory exists
        // if !configfs_path.exists() {
        //     std::fs::create_dir_all(configfs_path).with_context(|| {
        //         format!("failed to create configfs directory: {}", configfs_path.display())
        //     })?;
        // }

        // // Check mount state via /proc/mounts and mount if needed
        // let mounted = std::fs::read_to_string("/proc/mounts")
        //     .ok()
        //     .map(|s| {
        //         s.lines().any(|line| {
        //             let mut parts = line.split_whitespace();
        //             let _src = parts.next();
        //             let mnt = parts.next().unwrap_or("");
        //             let fstype = parts.next().unwrap_or("");
        //             mnt == "/sys/kernel/config" && fstype == "configfs"
        //         })
        //     })
        //     .unwrap_or(false);

        // if !mounted {
        //     let status = Command::new("mount")
        //         .args(["-t", "configfs", "none", "/sys/kernel/config"])
        //         .status()
        //         .with_context(|| "failed to execute mount for configfs")?;
        //     if !status.success() {
        //         return Err(anyhow!("mount configfs failed with status: {}", status));
        //     }
        //     info!("configfs mounted at /sys/kernel/config");
        // }

        Ok(())
    }

    fn create_config_path(&self) -> Result<()> {
        // // Create configs/c.1 directory
        // if Path::new(&self.config_c1_path).exists() {
        //     return Ok(());
        // }

        // std::fs::create_dir_all(&self.config_c1_path).with_context(|| {
        //     format!("failed to create config path: {}", self.config_c1_path.display())
        // })?;

        // info!("config path created: {}", self.config_c1_path.display());
        Ok(())
    }

    // Gadget configuration control is handled by the startup script
    fn write_gadget_config(&self) -> Result<()> {
        // // Create gadget base directory
        // if !Path::new(&self.kvm_gadget_path).exists() {
        //     std::fs::create_dir_all(&self.kvm_gadget_path).with_context(|| {
        //         format!("failed to create gadget path: {}", self.kvm_gadget_path.display())
        //     })?;
        // }

        // let _ = std::fs::write(self.kvm_gadget_path.join("UDC"), "");

        // // Get ordered config items - optimize by pre-allocating
        // let mut ordered_items = Vec::with_capacity(self.config_map.len());
        // ordered_items.extend(self.config_map.iter());
        // ordered_items.sort_by_key(|(_, item)| item.order);

        // for (_, item) in ordered_items.iter() {
        //     if item.order <= 1 {
        //         self.write_gadget_item_config(item)?;
        //     }
        // }

        // self.create_config_path()?;

        // // Process each config item
        // for (key, item) in ordered_items {
        //     if !self.is_gadget_config_item_enabled(key) {
        //         self.disable_gadget_item_config(item)?;
        //         continue;
        //     }

        //     self.write_gadget_item_config(item)?;
        // }

        // // Reorder function symlinks under configs/c.1 to ensure stable order expected by configfs
        // self.reorder_config_symlinks()?;

        // // Write UDC binding
        // self.write_udc()?;

        warn!("Finished writing UDC");
        Ok(())
    }

    fn disable_gadget_item_config(&self, _item: &GadgetConfigItem) -> Result<()> {
        // if let Some(config_path) = &item.config_path {
        //     let full_config_path =
        //         self.build_path_from_components(&self.config_c1_path, config_path);
        //     if full_config_path.exists() {
        //         let meta = std::fs::metadata(&full_config_path)?;
        //         if meta.is_dir() {
        //             std::fs::remove_dir_all(&full_config_path).with_context(|| {
        //                 format!("failed to remove config dir: {}", full_config_path.display())
        //             })?;
        //         } else {
        //             std::fs::remove_file(&full_config_path).with_context(|| {
        //                 format!("failed to remove config: {}", full_config_path.display())
        //             })?;
        //         }
        //         debug!("disabled gadget config: {}", full_config_path.display());
        //     }
        // }
        Ok(())
    }

    fn write_gadget_item_config(&self, _item: &GadgetConfigItem) -> Result<()> {
        // if let Some(config_path) = &item.config_path
        //     && item.config_attrs.is_empty()
        // {
        //     let config_link_path =
        //         self.build_path_from_components(&self.config_c1_path, config_path);
        //     if config_link_path.exists() {
        //         return Ok(());
        //         // let meta = std::fs::symlink_metadata(&config_link_path)?;
        //         // if meta.file_type().is_symlink() || meta.is_file() {
        //         //     std::fs::remove_file(&config_link_path)?;
        //         // } else if meta.is_dir() {
        //         //     std::fs::remove_dir_all(&config_link_path)?;
        //         // }
        //         debug!("temporarily removed config link: {}", config_link_path.display());
        //     }
        // }

        // // Create gadget item directory
        // let gadget_item_path = self.build_path_from_components(&self.kvm_gadget_path, &item.path);
        // if gadget_item_path != self.kvm_gadget_path {
        //     if !gadget_item_path.exists() {
        //         std::fs::create_dir_all(&gadget_item_path).with_context(|| {
        //             format!(
        //                 "failed to create gadget item directory: {}",
        //                 gadget_item_path.display()
        //             )
        //         })?;
        //     }
        // }

        // // HID: attributes before report_desc (subclass -> protocol -> report_length -> report_desc)
        // let is_hid = item.path.last().map(|s| s.starts_with("hid.usb")).unwrap_or(false);
        // if is_hid {
        //     // 1) subclass
        //     if let Some(v) = item.attrs.get("subclass") {
        //         self.write_file_content(&gadget_item_path.join("subclass"), v)?;
        //     }
        //     // 2) protocol
        //     if let Some(v) = item.attrs.get("protocol") {
        //         self.write_file_content(&gadget_item_path.join("protocol"), v)?;
        //     }
        //     // 3) report_length
        //     if let Some(v) = item.attrs.get("report_length") {
        //         self.write_file_content(&gadget_item_path.join("report_length"), v)?;
        //     }
        //     // 4) report_desc
        //     if let Some(report_desc) = &item.report_desc {
        //         self.write_file_content_bytes(&gadget_item_path.join("report_desc"), report_desc)?;
        //     }
        //     for (attr_name, attr_value) in &item.attrs {
        //         if ["protocol", "subclass", "report_length"].contains(&attr_name.as_str()) {
        //             continue;
        //         }
        //         let attr_path = gadget_item_path.join(attr_name);
        //         self.write_file_content(&attr_path, attr_value)?;
        //     }
        // } else {
        //     // non-HID: keep existing flow
        //     for (attr_name, attr_value) in &item.attrs {
        //         let attr_path = gadget_item_path.join(attr_name);
        //         self.write_file_content(&attr_path, attr_value)?;
        //     }
        //     if let Some(report_desc) = &item.report_desc {
        //         self.write_file_content_bytes(&gadget_item_path.join("report_desc"), report_desc)?;
        //     }
        // }

        // // Config attributes (e.g., strings/0x409, MaxPower at root under configs/c.1)
        // if let Some(config_path) = &item.config_path
        //     && !item.config_attrs.is_empty()
        // {
        //     let config_item_path =
        //         self.build_path_from_components(&self.config_c1_path, config_path);
        //     if config_item_path != self.config_c1_path {
        //         std::fs::create_dir_all(&config_item_path).with_context(|| {
        //             format!(
        //                 "failed to create config item directory: {}",
        //                 config_item_path.display()
        //             )
        //         })?;
        //     }
        //     for (attr_name, attr_value) in &item.config_attrs {
        //         let attr_path = config_item_path.join(attr_name);
        //         self.write_file_content(&attr_path, attr_value)?;
        //     }
        // }

        // // Create function symlink under configs/c.1 (only when config_attrs empty)
        // if let Some(config_path) = &item.config_path
        //     && item.config_attrs.is_empty()
        // {
        //     let config_link_path =
        //         self.build_path_from_components(&self.config_c1_path, config_path);
        //     let gadget_link_target =
        //         self.build_path_from_components(&self.kvm_gadget_path, &item.path);

        //     std::os::unix::fs::symlink(&gadget_link_target, &config_link_path).with_context(
        //         || {
        //             format!(
        //                 "failed to create symlink: {} -> {}",
        //                 config_link_path.display(),
        //                 gadget_link_target.display()
        //             )
        //         },
        //     )?;
        // }

        Ok(())
    }

    /// Ensure symlinks under configs/c.1 are created in a deterministic order
    fn reorder_config_symlinks(&self) -> Result<()> {
        // // Collect expected symlinks in order based on config_map ordering
        // let mut ordered_items = Vec::with_capacity(self.config_map.len());
        // ordered_items.extend(self.config_map.iter());
        // ordered_items.sort_by_key(|(_, item)| item.order);

        // let mut expected: Vec<(PathBuf, PathBuf)> = Vec::new();
        // for (key, item) in ordered_items {
        //     if !self.is_gadget_config_item_enabled(key) {
        //         continue;
        //     }
        //     // Only function links (config_path present, but no config_attrs)
        //     if let Some(cfg_path) = &item.config_path {
        //         if !item.config_attrs.is_empty() {
        //             continue;
        //         }
        //         let link = self.build_path_from_components(&self.config_c1_path, cfg_path);
        //         let target = self.build_path_from_components(&self.kvm_gadget_path, &item.path);
        //         expected.push((link, target));
        //     }
        // }

        // // Remove existing symlinks (keep other entries like strings/*)
        // if self.config_c1_path.exists() {
        //     for entry in std::fs::read_dir(&self.config_c1_path)
        //         .with_context(|| "failed to read configs/c.1 directory")?
        //     {
        //         let entry = entry?;
        //         let ftype = entry.file_type()?;
        //         if ftype.is_symlink() {
        //             std::fs::remove_file(entry.path()).with_context(|| {
        //                 format!("failed to remove symlink: {}", entry.path().display())
        //             })?;
        //         }
        //     }
        // }

        // // Recreate symlinks in the expected order
        // for (link, target) in expected {
        //     std::os::unix::fs::symlink(&target, &link).with_context(|| {
        //         format!(
        //             "failed to create symlink in order: {} -> {}",
        //             link.display(),
        //             target.display()
        //         )
        //     })?;
        // }

        warn!("re-creating symlinks in order");

        Ok(())
    }

    /// Helper function to build paths from components - optimized to reduce allocations
    fn build_path_from_components(&self, base: &Path, components: &[String]) -> PathBuf {
        if components.is_empty() {
            return base.to_path_buf();
        }

        let mut path = base.to_path_buf();
        path.reserve(components.iter().map(|s| s.len()).sum::<usize>() + components.len());
        for component in components {
            path.push(component);
        }
        path
    }

    fn write_udc(&self) -> Result<()> {
        // Self::force_unbind_conflicting_gadgets(&self.udc, &self.name);
        // let udc_path = self.kvm_gadget_path.join("UDC");
        // self.write_file_content(&udc_path, &self.udc)?;
        // info!("UDC bound: {}", self.udc);
        Ok(())
    }

    // Do not rebind the USB controller; it will hang
    fn rebind_usb(&self, _ignore_unbind_error: bool) -> Result<()> {
        // // Unbind from UDC
        // let unbind_path = PathBuf::from("/sys/bus/platform/drivers/dwc3/unbind");
        // if let Err(e) = std::fs::write(&unbind_path, &self.udc) {
        //     if !ignore_unbind_error {
        //         return Err(e).with_context(|| "failed to unbind UDC");
        //     }
        //     warn!("failed to unbind UDC (ignored): {}", e);
        // }

        // // Bind to UDC
        // let bind_path = PathBuf::from("/sys/bus/platform/drivers/dwc3/bind");
        // std::fs::write(&bind_path, &self.udc).with_context(|| "failed to bind UDC")?;

        // info!("USB gadget rebound successfully");
        Ok(())
    }

    fn write_file_content(&self, path: &Path, content: &str) -> Result<()> {
        std::fs::write(path, content)
            .with_context(|| format!("failed to write content to: {}", path.display()))?;

        Ok(())
    }

    fn write_file_content_bytes(&self, path: &Path, content: &[u8]) -> Result<()> {
        std::fs::write(path, content)
            .with_context(|| format!("failed to write content to: {}", path.display()))?;

        Ok(())
    }

    fn force_unbind_conflicting_gadgets(_udc: &str, _our_name: &str) {
        // let root = Path::new("/sys/kernel/config/usb_gadget");
        // if let Ok(entries) = fs::read_dir(root) {
        //     for e in entries.flatten() {
        //         let name = e.file_name().to_string_lossy().to_string();
        //         if name == our_name {
        //             continue;
        //         }
        //         let udc_file = e.path().join("UDC");
        //         if let Ok(s) = fs::read_to_string(&udc_file)
        //             && s.trim() == udc
        //         {
        //             let _ = fs::write(&udc_file, ""); // best-effort unbind
        //             info!("force-unbound conflicting gadget '{}' from UDC {}", name, udc);
        //         }
        //     }
        // }
    }
}

impl Drop for UsbGadget {
    fn drop(&mut self) {
        // Cleanup when gadget is dropped
        if let Err(e) = self.unbind_udc() {
            warn!("failed to unbind UDC during cleanup: {}", e);
        }

        if let Err(e) = std::fs::remove_dir_all(&self.config_c1_path) {
            warn!("failed to remove gadget config dir: {}", e);
        }
    }
}
