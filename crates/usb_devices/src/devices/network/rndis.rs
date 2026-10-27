//! RNDIS USB gadget function.

use std::fs;
use std::path::Path;

use usb_gadget::function::net::{Net, NetClass};
use usb_gadget::function::Handle;

use crate::error::UsbError;

/// Bulk IN + interrupt IN notify + bulk OUT.
pub const ENDPOINT_IN: u8 = 2;
pub const ENDPOINT_OUT: u8 = 1;

/// Win10+ RNDIS 6.0 driver match (MS OS Extended Compatibility).
const MS_COMPAT_ID: &str = "RNDIS";
const MS_SUBCOMPAT_ID: &str = "5162001";

/// Build an RNDIS function for the composite gadget.
pub fn build_function() -> (Net, Handle) {
    Net::builder(NetClass::Rndis).build()
}

/// Write function-level MS OS compatible IDs. Must run after `Gadget::register()`
/// and before `RegGadget::bind(UDC)`.
pub fn apply_ms_os_compat(net: &Net) -> Result<(), UsbError> {
    let func_dir = net.status().path().ok_or_else(|| {
        UsbError::DeviceNotFound(
            "RNDIS function path not ready (gadget not registered yet)".into(),
        )
    })?;

    let iface = func_dir.join("os_desc").join("interface.rndis");
    if !iface.is_dir() {
        return Err(UsbError::DeviceNotFound(format!(
            "RNDIS os_desc interface missing: {}",
            iface.display()
        )));
    }

    write_attr(iface.join("compatible_id"), MS_COMPAT_ID)?;
    write_attr(iface.join("sub_compatible_id"), MS_SUBCOMPAT_ID)?;
    Ok(())
}

fn write_attr(path: impl AsRef<Path>, value: &str) -> Result<(), UsbError> {
    let path = path.as_ref();
    fs::write(path, value.as_bytes()).map_err(|e| {
        UsbError::IoError(format!("failed to write {}: {e}", path.display()))
    })?;
    Ok(())
}
