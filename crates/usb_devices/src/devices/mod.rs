pub mod hid_abs;
mod hid_io;
pub mod hid_kb;
pub mod uac;
pub mod ums;
pub mod ums1;

use std::path::{Path, PathBuf};

use crate::config::*;
use crate::devices::ums::UmsMode;
use crate::error::UsbError;
use crate::proto::v1::DeviceSwitches;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use usb_gadget::function::audio::{Channel, Uac1, Uac1Config};
use usb_gadget::function::hid::Hid as LibHid;
use usb_gadget::function::msd::{Lun, Msd};
use usb_gadget::function::video::{Format, Uvc, UvcFrame};
use usb_gadget::{Class, Config, Gadget, Id, OsDescriptor, Strings};

const USB_MAX_STRING_LEN: usize = 126;

pub fn build_gadget(
    base_info: &UsbBaseInfo,
    switches: &DeviceSwitches,
    device_config: (UmsMode),
) -> Result<Gadget, UsbError> {
    let (ums_vm_mode) = device_config;

    // 1. Build device base info
    let strings = Strings::new(
        base_info.manufacturer.clone(),
        base_info.product.clone(),
        base_info.serial_number.clone(),
    );
    validate_strings(&strings)?;

    let mut gadget = Gadget::new(
        Class::new(0, 0, 0), // composite device: class defined at interface level
        Id::new(base_info.vendor_id, base_info.product_id),
        strings,
    );
    gadget.name = Some(GADGET_NAME.into());
    gadget.device_release = base_info.bcd_device;

    // 2. Build config
    let mut config = Config::new(GADGET_CONFIG_NAME);
    config.max_power = GADGET_MAX_POWER_MA;

    // 3. Add functions dynamically
    if switches.hid_kb_rel_enabled {
        let mut builder = LibHid::builder();
        builder.sub_class = HID_KB_REL_SUBCLASS;
        builder.protocol = HID_KB_REL_PROTOCOL;
        builder.report_len = HID_KB_REL_REPORT_LENGTH as u8;
        builder.report_desc = HID_KB_REL_REPORT_DESC.to_vec();
        let (_hid, handle) = builder.build();
        config.functions.push(handle);
    }

    if switches.hid_abs_enabled {
        let mut builder = LibHid::builder();
        builder.sub_class = HID_ABS_SUBCLASS;
        builder.protocol = HID_ABS_PROTOCOL;
        builder.report_len = HID_ABS_REPORT_LENGTH as u8;
        builder.no_out_endpoint = HID_ABS_NO_OUT_ENDPOINT;
        builder.report_desc = HID_ABS_REPORT_DESC.to_vec();
        let (_hid, handle) = builder.build();
        config.functions.push(handle);
    }

    if switches.ums_vm_enabled {
        let mut lun0 = Lun::default();
        lun0.removable = UMS_VM_REMOVABLE;
        lun0.inquiry_string = UMS_VM_INQUIRY_STRING.to_string();

        match ums_vm_mode {
            UmsMode::CdRom => {
                lun0.cdrom = true;
                lun0.read_only = true;
            }

            UmsMode::Disk => {
                lun0.cdrom = false;
                lun0.read_only = true;
            }
        }

        let mut builder = Msd::builder();
        builder.luns.push(lun0);
        let (_msd, handle) = builder.build();
        config.functions.push(handle);
    }

    if switches.ums_ft_enabled {
        let mut lun1 = Lun::default();
        lun1.cdrom = UMS_FT_CDROM;
        lun1.read_only = UMS_FT_RO;
        lun1.removable = UMS_FT_REMOVABLE;
        lun1.inquiry_string = UMS_FT_INQUIRY_STRING.to_string();

        let mut builder = Msd::builder();
        builder.luns.push(lun1);
        let (_msd, handle) = builder.build();
        config.functions.push(handle);
    }

    if switches.uac1_enabled {
        // Possible driver quirk: playback p_chmask=3 c_chmask=0 is inverted, which disables speaker and keeps mic only

        // Enable capture
        let mut uac1_capture_config = Uac1Config::default();
        uac1_capture_config.channel = Channel::new(0, 48000, 2);
        uac1_capture_config.mute_present = Some(true);
        uac1_capture_config.volume_present = Some(true);
        uac1_capture_config.volume_min = Some(-25600);
        uac1_capture_config.volume_max = Some(0);
        uac1_capture_config.volume_resolution = Some(1);
        uac1_capture_config.volume_name = None;
        uac1_capture_config.input_terminal_name = None;
        uac1_capture_config.input_terminal_channel_name = None;
        uac1_capture_config.output_terminal_name = None;

        // Disable playback
        let mut uac1_playback_config = Uac1Config::default();

        uac1_playback_config.channel = Channel::new(3, 48000, 2);
        let mut builder = Uac1::builder();
        builder.function_name = Some(UAC1_INSTANCE.to_string());
        let (_uac1, handle) = builder
            .with_capture_config(uac1_capture_config)
            .with_playback_config(uac1_playback_config)
            .build();
        config.functions.push(handle);
    }

    if switches.uvc_enabled {
        let mut builder = Uvc::builder();
        builder.function_name = Some(UVC_INSTANCE.to_string());
        // Match script: YUYV + MJPEG (640x480)
        // Write script-identical dwFrameInterval values (100ns units)
        builder = builder.with_frames(vec![
            UvcFrame::new(
                UVC_FRAME_WIDTH,
                UVC_FRAME_HEIGHT,
                Format::Yuyv,
                UVC_FRAME_INTERVALS_100NS.iter().copied(),
            ),
            UvcFrame::new(
                UVC_FRAME_WIDTH,
                UVC_FRAME_HEIGHT,
                Format::Mjpeg,
                UVC_FRAME_INTERVALS_100NS.iter().copied(),
            ),
        ]);
        let (_uvc, handle) = builder.build();
        config.functions.push(handle);
    }

    // 4. Assemble final gadget
    gadget.add_config(config);

    // OS descriptors (key for driverless Windows)
    gadget.os_descriptor = Some(OsDescriptor {
        vendor_code: OS_DESC_VENDOR_CODE,
        qw_sign: OS_DESC_QW_SIGN.into(),
        config: 0, // first config
    });

    Ok(gadget)
}

fn validate_strings(strings: &Strings) -> Result<(), UsbError> {
    let manufacturer_len = strings.manufacturer.len();
    let product_len = strings.product.len();
    let serial_number_len = strings.serial_number.len();

    if manufacturer_len > USB_MAX_STRING_LEN ||
        product_len > USB_MAX_STRING_LEN || 
        serial_number_len > USB_MAX_STRING_LEN
    {
        return Err(UsbError::InvalidArgument(format!(
            "length of manufacturer, product or serial_number must not exceed {USB_MAX_STRING_LEN} characters"
        )));
    }

    Ok(())
}

/// Resolve `lun.0/file` under the active gadget by matching SCSI inquiry_string.
pub(crate) async fn resolve_mass_storage_file(inquiry_string: &str) -> Result<PathBuf, UsbError> {
    let functions_dir =
        Path::new("/sys/kernel/config/usb_gadget").join(GADGET_NAME).join("functions");

    if !functions_dir.exists() {
        return Err(UsbError::DeviceNotFound(format!(
            "gadget functions dir not found: {}",
            functions_dir.display()
        )));
    }

    let mut entries = tokio::fs::read_dir(&functions_dir).await.map_err(|e| {
        UsbError::IoError(format!("failed to read {}: {:?}", functions_dir.display(), e))
    })?;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| UsbError::IoError(format!("failed to read dir entry: {:?}", e)))?
    {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("mass_storage.") {
            continue;
        }

        let lun_dir = entry.path().join("lun.0");
        let inquiry_path = lun_dir.join("inquiry_string");
        if !inquiry_path.exists() {
            continue;
        }

        let actual = tokio::fs::read_to_string(&inquiry_path).await.map_err(|e| {
            UsbError::IoError(format!("failed to read {}: {:?}", inquiry_path.display(), e))
        })?;
        if actual.trim() != inquiry_string {
            continue;
        }

        return Ok(lun_dir.join("file"));
    }

    Err(UsbError::DeviceNotFound(format!(
        "mass_storage lun with inquiry_string {inquiry_string:?} not found"
    )))
}

async fn read_sysfs_u8(path: &Path) -> Result<u8, UsbError> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| UsbError::IoError(format!("failed to read {}: {:?}", path.display(), e)))?;
    raw.trim().parse::<u8>().map_err(|_| {
        UsbError::InvalidArgument(format!("invalid u8 at {}: {:?}", path.display(), raw))
    })
}

async fn read_sysfs_u16(path: &Path) -> Result<u16, UsbError> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| UsbError::IoError(format!("failed to read {}: {:?}", path.display(), e)))?;
    raw.trim().parse::<u16>().map_err(|_| {
        UsbError::InvalidArgument(format!("invalid u16 at {}: {:?}", path.display(), raw))
    })
}

/// Resolve `/dev/hidgN` by matching HID function attributes in configfs.
pub(crate) async fn resolve_hid_device_path(
    protocol: u8,
    subclass: u8,
    report_length: u16,
) -> Result<PathBuf, UsbError> {
    let functions_dir =
        Path::new("/sys/kernel/config/usb_gadget").join(GADGET_NAME).join("functions");

    if !functions_dir.exists() {
        return Err(UsbError::DeviceNotFound(format!(
            "gadget functions dir not found: {}",
            functions_dir.display()
        )));
    }

    let mut entries = tokio::fs::read_dir(&functions_dir).await.map_err(|e| {
        UsbError::IoError(format!("failed to read {}: {:?}", functions_dir.display(), e))
    })?;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| UsbError::IoError(format!("failed to read dir entry: {:?}", e)))?
    {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("hid.") {
            continue;
        }

        let func_dir = entry.path();
        let actual_protocol = read_sysfs_u8(&func_dir.join("protocol")).await?;
        let actual_subclass = read_sysfs_u8(&func_dir.join("subclass")).await?;
        let actual_report_length = read_sysfs_u16(&func_dir.join("report_length")).await?;

        if actual_protocol != protocol
            || actual_subclass != subclass
            || actual_report_length != report_length
        {
            continue;
        }

        let dev_raw = tokio::fs::read_to_string(func_dir.join("dev"))
            .await
            .map_err(|e| UsbError::IoError(format!("failed to read hid dev: {:?}", e)))?;
        let dev = dev_raw.trim();
        if dev.is_empty() {
            return Err(UsbError::DeviceNotFound(format!(
                "hid function {} has empty dev (not bound?)",
                func_dir.display()
            )));
        }

        let minor = dev
            .split(':')
            .nth(1)
            .ok_or_else(|| UsbError::InvalidArgument(format!("invalid hid dev format: {dev:?}")))?
            .parse::<u32>()
            .map_err(|_| UsbError::InvalidArgument(format!("invalid hid dev minor: {dev:?}")))?;

        return Ok(PathBuf::from(format!("/dev/hidg{minor}")));
    }

    Err(UsbError::DeviceNotFound(format!(
        "hid function with protocol={protocol}, subclass={subclass}, report_length={report_length} not found"
    )))
}

pub(crate) async fn write_file(path: &Path, data: &str) -> Result<(), UsbError> {
    let mut f = OpenOptions::new().write(true).open(path).await.map_err(|e| {
        UsbError::InvalidArgument(format!("failed to open {}, error: {:?}", path.display(), e))
    })?;
    let mut writer = BufWriter::new(&mut f);
    writer.write_all(data.as_bytes()).await.map_err(|e| {
        UsbError::IoError(format!("failed to write file {}, error: {:?}", path.display(), e))
    })?;
    writer.flush().await.ok();
    Ok(())
}
