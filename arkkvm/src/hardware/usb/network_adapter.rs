//! Network adapter helpers for arkkvm config comparison only.
//! Board L3 and gadget Net/ifname are owned by usb_devices.

use crate::config::types::{NetworkAdapter, NetworkAdapterL3};

/// True when gadget-relevant fields match (L3-only fields ignored).
pub fn gadget_switches_equal(
    a: &crate::config::types::UsbDevices,
    b: &crate::config::types::UsbDevices,
) -> bool {
    a.absolute_mouse == b.absolute_mouse
        && a.relative_mouse == b.relative_mouse
        && a.keyboard == b.keyboard
        && a.mass_storage_vm == b.mass_storage_vm
        && a.mass_storage_ft == b.mass_storage_ft
        && a.microphone == b.microphone
        && a.camera == b.camera
        && a.network_adapter.enable == b.network_adapter.enable
        && a.network_adapter.adapter_type == b.network_adapter.adapter_type
}

pub fn network_adapter_l3_equal(a: &NetworkAdapter, b: &NetworkAdapter) -> bool {
    l3_config_equal(&a.l3_config, &b.l3_config)
}

fn l3_config_equal(a: &NetworkAdapterL3, b: &NetworkAdapterL3) -> bool {
    a.ip_address == b.ip_address
        && a.subnet_mask == b.subnet_mask
        && a.gateway == b.gateway
        && a.dns_servers == b.dns_servers
}
