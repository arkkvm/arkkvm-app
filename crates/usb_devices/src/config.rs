use serde::{Deserialize, Serialize};

// ============================================================================
// Static configuration
// Fixed parameters for virtual device init; aligned with S50usbdevice script
// ============================================================================

// Global config
pub const GADGET_NAME: &str = "arkkvm";
pub const GADGET_CONFIG_NAME: &str = "c.1";
pub const GADGET_MAX_POWER_MA: u16 = 500;
pub const OS_DESC_VENDOR_CODE: u8 = 0x01;
pub const OS_DESC_QW_SIGN: &str = "MSFT100";

// RV1106BG3 product function endpoint budget (excludes EP0).
// Silicon inventory (debugfs / GHWPARAMS): IN=9 OUT=5 function slots.
// Empirical usable budget is tighter on IN (TX FIFO): kb+dual-UMS+UAC+NCM
// binds at IN=6/OUT=4; adding another mouse IN (7/4) fails with uac1 -ENODEV.
pub const MAX_FUNCTION_IN_EPS: u8 = 6;
pub const MAX_FUNCTION_OUT_EPS: u8 = 5;

// HID 0: boot keyboard
pub const HID_KB_INSTANCE: &str = "usb0";
pub const HID_KB_PROTOCOL: u8 = 1;
pub const HID_KB_SUBCLASS: u8 = 1;
pub const HID_KB_REPORT_LENGTH: u16 = 8;
pub const HID_KB_NO_OUT_ENDPOINT: bool = false;
pub const HID_KB_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x03, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

// HID 1: absolute mouse
pub const HID_ABS_INSTANCE: &str = "usb1";
pub const HID_ABS_PROTOCOL: u8 = 2;
pub const HID_ABS_SUBCLASS: u8 = 0;
pub const HID_ABS_REPORT_LENGTH: u16 = 6;
pub const HID_ABS_NO_OUT_ENDPOINT: bool = true;
pub const HID_ABS_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
    0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xff, 0x7f, 0x36, 0x00,
    0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xc0, 0x85, 0x02, 0x09, 0x38, 0x15,
    0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0x05, 0x0c, 0x0a,
    0x38, 0x02, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0,
];

// HID 2: relative mouse
pub const HID_REL_INSTANCE: &str = "usb2";
pub const HID_REL_PROTOCOL: u8 = 2;
pub const HID_REL_SUBCLASS: u8 = 0;
pub const HID_REL_REPORT_LENGTH: u16 = 5;
pub const HID_REL_NO_OUT_ENDPOINT: bool = true;
pub const HID_REL_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x08, 0x15, 0x00, 0x25, 0x01, 0x95, 0x08, 0x75, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30,
    0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, 0xc0, 0xc0,
];

// UMS 0: virtual media (CD/Disk)
pub const UMS_VM_INSTANCE: &str = "0";
pub const UMS_VM_LUN_NAME: &str = "lun.0";
pub const UMS_VM_CDROM: bool = true;
pub const UMS_VM_RO: bool = true;
pub const UMS_VM_REMOVABLE: bool = true;
pub const UMS_VM_INQUIRY_STRING: &str = "ArkKVM Virtual Media";

// UMS 1: file transfer (USB mass storage)
pub const UMS_FT_INSTANCE: &str = "1";
pub const UMS_FT_LUN_NAME: &str = "lun.0";
pub const UMS_FT_CDROM: bool = false;
pub const UMS_FT_RO: bool = false;
pub const UMS_FT_REMOVABLE: bool = true;
pub const UMS_FT_INQUIRY_STRING: &str = "ArkKVM Virtual USB Drive";

// UAC1: virtual microphone (f_uac1: p_chmask → ISO IN / mic, c_chmask → ISO OUT / speaker)
pub const UAC1_INSTANCE: &str = "ArkKVM Microphone";
pub const UAC1_SAMPLE_RATE: u32 = 48000;
pub const UAC1_SAMPLE_SIZE: u32 = 2;
pub const UAC1_P_CHMASK: u32 = 0b11; // stereo ISO IN (mic data to host)
pub const UAC1_C_CHMASK: u32 = 0; // no ISO OUT (no speaker)

// UVC: virtual camera
pub const UVC_INSTANCE: &str = "gs6";
pub const UVC_FRAME_WIDTH: u32 = 640;
pub const UVC_FRAME_HEIGHT: u32 = 480;
// Match script dwFrameInterval: 333333,666666,1000000,2000000 (units: 100ns)
pub const UVC_FRAME_INTERVALS_100NS: &[u32] = &[333_333, 666_666, 1_000_000, 2_000_000];

// USB network adapter (NCM / RNDIS); board L3 applied by usb_devices.
pub const NETWORK_ADAPTER_INSTANCE: &str = "usb0";


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsbBaseInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub bcd_usb: u16,
    pub serial_number: String,
    pub manufacturer: String,
    pub product: String,
    #[serde(default)]
    pub strict_mode: bool,
    /// When true, ConfigFS `max_speed` is `full-speed`; otherwise `high-speed`.
    #[serde(default)]
    pub force_full_speed: bool,
}

impl Default for UsbBaseInfo {
    fn default() -> Self {
        Self {
            vendor_id: 0x1d6b,
            product_id: 0x0104,
            bcd_device: 0x0100,
            bcd_usb: 0x0200,
            serial_number: common::device::extract_serial_number().unwrap_or_default(),
            manufacturer: "ArkKVM".to_string(),
            product: "Multifunction Composite Gadget".to_string(),
            strict_mode: false,
            force_full_speed: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceSwitches {
    pub hid_kb_enabled: bool,  // boot keyboard (hid.usb0)
    pub hid_abs_enabled: bool, // absolute mouse (hid.usb1)
    pub hid_rel_enabled: bool, // relative mouse (hid.usb2)
    pub ums_vm_enabled: bool,  // virtual media (mass_storage.0)
    pub ums_ft_enabled: bool,  // file transfer (mass_storage.1)
    pub uac1_enabled: bool,    // virtual microphone (uac1.gs0)
    pub uvc_enabled: bool,       // virtual camera (uvc.gs6)
    pub network_adapter_enabled: bool, // USB net (NCM or RNDIS)
}

impl Default for DeviceSwitches {
    fn default() -> Self {
        Self {
            hid_kb_enabled: true,
            hid_abs_enabled: true,
            hid_rel_enabled: true,
            ums_vm_enabled: true,
            ums_ft_enabled: false,
            uac1_enabled: false,
            uvc_enabled: false,
            network_adapter_enabled: false,
        }
    }
}

fn network_adapter_enabled(switches: &crate::proto::v1::DeviceSwitches) -> bool {
    switches
        .network_adapter
        .as_ref()
        .map(|n| n.enable)
        .unwrap_or(false)
}

impl crate::proto::v1::DeviceSwitches {
    pub fn has_any_enabled(&self) -> bool {
        self.hid_kb_enabled
            || self.hid_abs_enabled
            || self.hid_rel_enabled
            || self.ums_vm_enabled
            || self.ums_ft_enabled
            || self.uac1_enabled
            || self.uvc_enabled
            || network_adapter_enabled(self)
    }
}
