//! CDC NCM USB gadget function.

use usb_gadget::function::net::{Net, NetClass};
use usb_gadget::function::Handle;

/// Bulk IN + interrupt IN notify + bulk OUT.
pub const ENDPOINT_IN: u8 = 2;
pub const ENDPOINT_OUT: u8 = 1;

/// Build an NCM function for the composite gadget.
pub fn build_function() -> (Net, Handle) {
    Net::builder(NetClass::Ncm).build()
}
