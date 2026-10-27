pub mod hid_abs;
mod hid_io;
pub mod hid_kb;
pub mod hid_rel;
pub mod network;
pub mod uac;
pub mod ums;
pub mod ums1;

use std::path::{Path, PathBuf};

use crate::config::*;
use crate::devices::network::{adapter_type_from_i32, build_net_function};
use crate::devices::ums::UmsMode;
use crate::error::UsbError;
use crate::proto::v1::{DeviceSwitches, NetworkAdapterType};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use usb_gadget::function::audio::{Channel, Uac1, Uac1Config};
use usb_gadget::function::hid::Hid as LibHid;
use usb_gadget::function::msd::{Lun, Msd};
use usb_gadget::function::net::Net;
use usb_gadget::function::video::{Format, Uvc, UvcFrame};
use usb_gadget::{Class, Config, Gadget, Id, OsDescriptor, Speed, Strings};

const USB_MAX_STRING_LEN: usize = 126;

/// UVC streaming + status interrupt IN (no OUT in current build_gadget config).
pub const UVC_ENDPOINT_IN: u8 = 2;
pub const UVC_ENDPOINT_OUT: u8 = 0;

/// Sum function IN/OUT endpoints for the given switches (excludes EP0).
pub fn endpoint_usage(switches: &DeviceSwitches) -> (u8, u8) {
    let mut inn: u8 = 0;
    let mut out: u8 = 0;

    if switches.hid_kb_enabled {
        inn = inn.saturating_add(hid_kb::ENDPOINT_IN);
        out = out.saturating_add(hid_kb::ENDPOINT_OUT);
    }
    if switches.hid_abs_enabled {
        inn = inn.saturating_add(hid_abs::ENDPOINT_IN);
        out = out.saturating_add(hid_abs::ENDPOINT_OUT);
    }
    if switches.hid_rel_enabled {
        inn = inn.saturating_add(hid_rel::ENDPOINT_IN);
        out = out.saturating_add(hid_rel::ENDPOINT_OUT);
    }
    if switches.ums_vm_enabled {
        inn = inn.saturating_add(ums::ENDPOINT_IN);
        out = out.saturating_add(ums::ENDPOINT_OUT);
    }
    if switches.ums_ft_enabled {
        inn = inn.saturating_add(ums1::ENDPOINT_IN);
        out = out.saturating_add(ums1::ENDPOINT_OUT);
    }
    if switches.uac1_enabled {
        inn = inn.saturating_add(uac::ENDPOINT_IN);
        out = out.saturating_add(uac::ENDPOINT_OUT);
    }
    if switches.uvc_enabled {
        inn = inn.saturating_add(UVC_ENDPOINT_IN);
        out = out.saturating_add(UVC_ENDPOINT_OUT);
    }
    if let Some(na) = switches.network_adapter.as_ref() {
        if na.enable {
            let (ein, eout) = match adapter_type_from_i32(na.adapter_type) {
                NetworkAdapterType::Rndis => (network::rndis::ENDPOINT_IN, network::rndis::ENDPOINT_OUT),
                _ => (network::ncm::ENDPOINT_IN, network::ncm::ENDPOINT_OUT),
            };
            inn = inn.saturating_add(ein);
            out = out.saturating_add(eout);
        }
    }

    (inn, out)
}

/// Reject switch sets that exceed RV1106BG3 function endpoint limits.
pub fn check_endpoint_budget(switches: &DeviceSwitches) -> Result<(), UsbError> {
    let (inn, out) = endpoint_usage(switches);
    if inn > MAX_FUNCTION_IN_EPS || out > MAX_FUNCTION_OUT_EPS {
        return Err(UsbError::EndpointBudgetExceeded(format!(
            "in={inn}/{MAX_FUNCTION_IN_EPS} out={out}/{MAX_FUNCTION_OUT_EPS}"
        )));
    }
    Ok(())
}

pub fn build_gadget(
    base_info: &UsbBaseInfo,
    switches: &DeviceSwitches,
    device_config: (UmsMode),
) -> Result<(Gadget, Option<Net>), UsbError> {
    let (ums_vm_mode) = device_config;
    let mut net_dev: Option<Net> = None;

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
    // Always set max_speed explicitly (ConfigFS: "full-speed" / "high-speed").
    gadget.max_speed = Some(if base_info.force_full_speed {
        Speed::FullSpeed
    } else {
        Speed::HighSpeed
    });

    // 2. Build config
    let mut config = Config::new(GADGET_CONFIG_NAME);
    config.max_power = GADGET_MAX_POWER_MA;

    // 3. Add functions dynamically
    // USB net early to reduce composite EP pressure vs UMS/UAC.
    if let Some(na) = switches.network_adapter.as_ref() {
        if na.enable {
            let adapter_type = adapter_type_from_i32(na.adapter_type);
            let (net, handle) = build_net_function(adapter_type);
            config.functions.push(handle);
            net_dev = Some(net);
        }
    }

    if switches.hid_kb_enabled {
        let mut builder = LibHid::builder();
        builder.sub_class = HID_KB_SUBCLASS;
        builder.protocol = HID_KB_PROTOCOL;
        builder.report_len = HID_KB_REPORT_LENGTH as u8;
        builder.report_desc = HID_KB_REPORT_DESC.to_vec();
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

    if switches.hid_rel_enabled {
        let mut builder = LibHid::builder();
        builder.sub_class = HID_REL_SUBCLASS;
        builder.protocol = HID_REL_PROTOCOL;
        builder.report_len = HID_REL_REPORT_LENGTH as u8;
        builder.no_out_endpoint = HID_REL_NO_OUT_ENDPOINT;
        builder.report_desc = HID_REL_REPORT_DESC.to_vec();
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
        // f_uac1: p_chmask → ISO IN (mic); c_chmask=0 → no speaker.
        // Kernel defaults enable FU (mute/volume); write false to avoid AC interrupt EP.
        let mut uac1_capture = Uac1Config::default();
        uac1_capture.channel = Channel::new(UAC1_C_CHMASK, UAC1_SAMPLE_RATE, UAC1_SAMPLE_SIZE);
        uac1_capture.mute_present = Some(false);
        uac1_capture.volume_present = Some(false);

        let mut uac1_playback = Uac1Config::default();
        uac1_playback.channel = Channel::new(UAC1_P_CHMASK, UAC1_SAMPLE_RATE, UAC1_SAMPLE_SIZE);
        uac1_playback.mute_present = Some(false);
        uac1_playback.volume_present = Some(false);

        let mut builder = Uac1::builder();
        builder.function_name = Some(UAC1_INSTANCE.to_string());
        let (_uac1, handle) =
            builder.with_capture_config(uac1_capture).with_playback_config(uac1_playback).build();
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

    Ok((gadget, net_dev))
}

fn validate_strings(strings: &Strings) -> Result<(), UsbError> {
    let manufacturer_len = strings.manufacturer.len();
    let product_len = strings.product.len();
    let serial_number_len = strings.serial_number.len();

    if manufacturer_len > USB_MAX_STRING_LEN
        || product_len > USB_MAX_STRING_LEN
        || serial_number_len > USB_MAX_STRING_LEN
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

#[cfg(test)]
mod endpoint_budget_tests {
    use super::*;
    use crate::proto::v1::{NetworkAdapterSwitch, NetworkAdapterType};

    fn ncm_on() -> Option<NetworkAdapterSwitch> {
        Some(NetworkAdapterSwitch {
            enable: true,
            adapter_type: NetworkAdapterType::Ncm as i32,
        })
    }

    /// Board-proven OK: kb + dual UMS + UAC + NCM → IN=6 OUT=4.
    fn switches_kb_dual_ums_uac_ncm() -> DeviceSwitches {
        DeviceSwitches {
            hid_kb_enabled: true,
            hid_abs_enabled: false,
            hid_rel_enabled: false,
            ums_vm_enabled: true,
            ums_ft_enabled: true,
            uac1_enabled: true,
            uvc_enabled: false,
            network_adapter: ncm_on(),
        }
    }

    /// Board-failed: 3×HID + dual UMS + UAC + NCM → IN=8 OUT=4.
    fn switches_three_hid_dual_ums_uac_ncm() -> DeviceSwitches {
        DeviceSwitches {
            hid_kb_enabled: true,
            hid_abs_enabled: true,
            hid_rel_enabled: true,
            ums_vm_enabled: true,
            ums_ft_enabled: true,
            uac1_enabled: true,
            uvc_enabled: false,
            network_adapter: ncm_on(),
        }
    }

    #[test]
    fn kb_dual_ums_uac_ncm_fits_budget() {
        let s = switches_kb_dual_ums_uac_ncm();
        assert_eq!(endpoint_usage(&s), (6, 4));
        assert!(check_endpoint_budget(&s).is_ok());
    }

    #[test]
    fn three_hid_dual_ums_uac_ncm_exceeds_in_budget() {
        let s = switches_three_hid_dual_ums_uac_ncm();
        assert_eq!(endpoint_usage(&s), (8, 4));
        let err = check_endpoint_budget(&s).unwrap_err();
        assert!(matches!(err, UsbError::EndpointBudgetExceeded(_)));
        assert!(err.to_string().contains("in=8/6"));
    }
}
