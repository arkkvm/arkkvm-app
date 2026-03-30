//! USB HID report descriptors for keyboard and mouse devices.
//!
//! These descriptors define the structure of HID reports sent to and from
//! the USB HID devices. They are based on the USB HID specification.


/// USB HID Report Descriptor - Composite Device Descriptor
///
/// # Overview
///
/// This is a USB HID (Human Interface Device) report descriptor that defines a composite HID device.
/// The device supports both keyboard and mouse functions. The HID Report Descriptor is a core
/// part of the USB HID specification, and it describes the data format the device will send and receive.
///
/// # Basics of the HID Report Descriptor
///
/// HID Report Descriptor uses a compact binary format and is composed of a sequence of items.
/// Each item consists of one or more bytes:
/// - First byte: contains the item type, tag, and size information
/// - Following bytes: contain the item's data value
///
/// ## Item Types:
/// - **Main Items**: define or group data fields (Input, Output, Feature, Collection, End Collection)
/// - **Global Items**: global attributes that affect all following items (Usage Page, Logical Min/Max, Report Size, etc.)
/// - **Local Items**: local attributes that only affect the next Main Item (Usage, Usage Min/Max, etc.)
///
/// ## Common Item Tags:
/// - `0x05` - USAGE_PAGE: defines the usage category for subsequent Usage items
/// - `0x09` - USAGE: defines a specific purpose (e.g., keyboard, mouse, buttons, etc.)
/// - `0xA1` - COLLECTION: begins a collection and groups related items
/// - `0xC0` - END_COLLECTION: ends a collection
/// - `0x85` - REPORT_ID: defines the report ID to distinguish different report types
/// - `0x15/0x16` - LOGICAL_MINIMUM: logical minimum value (1 byte / 2 bytes)
/// - `0x25/0x26` - LOGICAL_MAXIMUM: logical maximum value (1 byte / 2 bytes)
/// - `0x35/0x36` - PHYSICAL_MINIMUM: physical minimum value (1 byte / 2 bytes)
/// - `0x45/0x46` - PHYSICAL_MAXIMUM: physical maximum value (1 byte / 2 bytes)
/// - `0x75` - REPORT_SIZE: number of bits for each field
/// - `0x95` - REPORT_COUNT: number of fields
/// - `0x81` - INPUT: defines an input data field (device to host)
/// - `0x91` - OUTPUT: defines an output data field (host to device)
/// - `0x19/0x29` - USAGE_MINIMUM/MAXIMUM: minimum and maximum usage values
///
/// ## Flags for Input/Output/Feature items:
/// - Bit 0: Data (0) / Constant (1) - whether the data is variable
/// - Bit 1: Array (0) / Variable (1) - array or independent variable
/// - Bit 2: Absolute (0) / Relative (2) - absolute or relative value
/// - For example: `0x02` = Data, Variable, Absolute
///         `0x06` = Data, Variable, Relative
///         `0x03` = Constant, Variable, Absolute
///
/// # Device Composition
///
/// This descriptor defines the following 4 HID reports:
///
/// ## Report ID 1: Standard Keyboard
/// - **Modifier key byte**: 8 modifier keys (Ctrl, Shift, Alt, GUI), 1 bit each, total 8 bits
/// - **Reserved byte**: 1 byte used for alignment
/// - **Key array**: 6 bytes, each stores one pressed key code (up to 6 keys pressed simultaneously)
/// - **LED output**: 5 LED status bits (Num Lock, Caps Lock, Scroll Lock, etc.)
/// - **Report format**: [modifier(1B)][reserved(1B)][key1(1B)][key2(1B)][key3(1B)][key4(1B)][key5(1B)][key6(1B)]
///
/// ## Report ID 2: Absolute Mouse
/// - **Buttons**: 3 mouse buttons (left, right, middle), 1 bit each, plus 5 bits padding
/// - **X coordinate**: 16-bit absolute coordinate value (0-32767)
/// - **Y coordinate**: 16-bit absolute coordinate value (0-32767)
/// - **Report format**: [buttons(1B)][X low byte(1B)][X high byte(1B)][Y low byte(1B)][Y high byte(1B)]
/// - **Usage**: for input devices that require absolute positioning, such as touch screens and drawing tablets
///
/// ## Report ID 3: Mouse Wheel
/// - **Wheel**: 8-bit signed value (-127 to +127)
/// - **Report format**: [wheel value(1B)]
/// - **Usage**: independent wheel report, can be used together with Report ID 2
///
/// ## Report ID 4: Relative Mouse
/// - **Buttons**: 8 mouse buttons (supports additional buttons), 1 bit each
/// - **X movement**: 8-bit signed relative movement value (-127 to +127)
/// - **Y movement**: 8-bit signed relative movement value (-127 to +127)
/// - **Wheel**: 8-bit signed relative movement value (-127 to +127)
/// - **Report format**: [buttons(1B)][X move(1B)][Y move(1B)][wheel(1B)]
/// - **Usage**: a standard mouse, compatible with the Boot Protocol
///
/// # Technical Details
///
/// ## Usage Page Notes:
/// - `0x01` (Generic Desktop): generic desktop controls (mouse, keyboard, etc.)
/// - `0x07` (Keyboard/Keypad): keyboard and keypad keys
/// - `0x08` (LEDs): LED indicators
/// - `0x09` (Button): buttons
///
/// ## Collection Types:
/// - `0x01` (Application): application collection, defines the device's primary function
/// - `0x00` (Physical): physical collection, groups physically related items
///
/// # Design Considerations
///
/// - **Multiple Report ID design**: using different Report IDs allows multiple functions in a single HID device
/// - **Absolute and relative mouse coexist**: provides two mouse modes for different application scenarios
/// - **Boot Protocol compatibility**: the relative mouse design for Report ID 4 is compatible with the USB Boot Protocol
/// - **6-Key Rollover**: keyboard supports up to 6 simultaneous normal key presses (excluding modifier keys)
/// - **Extended button support**: relative mouse supports 8 buttons, meeting needs of gaming mice and similar devices
///
/// # Reference Documentation
///
/// - USB HID 1.11 specification: <https://www.usb.org/hid>
/// - Linux Kernel HID documentation: <https://www.kernel.org/doc/Documentation/usb/gadget_hid.txt>
/// - HID Usage Tables: <https://usb.org/sites/default/files/hut1_3_0.pdf>
#[rustfmt::skip]
pub const HID_REPORT_DESC: &[u8] = &[
    //
    // Keyboard HID report descriptor
    //
    // Source: USB HID specification https://www.kernel.org/doc/Documentation/usb/gadget_hid.txt
    // - USAGE_PAGE (Generic Desktop)
    // - USAGE (Keyboard)
    // - 8 modifier keys + 101 regular keys + 5 LED outputs
    // - Report ID 1: Keyboard report
    //
    0x05, 0x01,         // USAGE_PAGE (Generic Desktop)
    0x09, 0x06,         // USAGE (Keyboard)
    0xa1, 0x01,         // COLLECTION (Application)
    0x85, 0x01,         //     REPORT_ID (1)
    0x05, 0x07,         //     USAGE_PAGE (Keyboard)
    0x19, 0xe0,         //     USAGE_MINIMUM (Keyboard LeftControl)
    0x29, 0xe7,         //     USAGE_MAXIMUM (Keyboard Right GUI)
    0x15, 0x00,         //     LOGICAL_MINIMUM (0)
    0x25, 0x01,         //     LOGICAL_MAXIMUM (1)
    0x75, 0x01,         //     REPORT_SIZE (1)
    0x95, 0x08,         //     REPORT_COUNT (8)
    0x81, 0x02,         //     INPUT (Data,Var,Abs)
    0x95, 0x01,         //     REPORT_COUNT (1)
    0x75, 0x08,         //     REPORT_SIZE (8)
    0x81, 0x03,         //     INPUT (Cnst,Var,Abs)
    0x95, 0x05,         //     REPORT_COUNT (5)
    0x75, 0x01,         //     REPORT_SIZE (1)
    0x05, 0x08,         //     USAGE_PAGE (LEDs)
    0x19, 0x01,         //     USAGE_MINIMUM (Num Lock)
    0x29, 0x05,         //     USAGE_MAXIMUM (Kana)
    0x91, 0x02,         //     OUTPUT (Data,Var,Abs)
    0x95, 0x01,         //     REPORT_COUNT (1)
    0x75, 0x03,         //     REPORT_SIZE (3)
    0x91, 0x03,         //     OUTPUT (Cnst,Var,Abs)
    0x95, 0x06,         //     REPORT_COUNT (6)
    0x75, 0x08,         //     REPORT_SIZE (8)
    0x15, 0x00,         //     LOGICAL_MINIMUM (0)
    0x25, 0x65,         //     LOGICAL_MAXIMUM (101)
    0x05, 0x07,         //     USAGE_PAGE (Keyboard)
    0x19, 0x00,         //     USAGE_MINIMUM (Reserved)
    0x29, 0x65,         //     USAGE_MAXIMUM (Keyboard Application)
    0x81, 0x00,         //     INPUT (Data,Ary,Abs)
    0xc0,               // END_COLLECTION
    //
    // Absolute mouse HID report descriptor with wheel support
    //
    // Source: USB HID specification
    // - Report ID 2: Absolute mouse movement (X, Y, buttons)
    // - Report ID 3: Wheel movement
    //
    0x05, 0x01,         // USAGE_PAGE (Generic Desktop Ctrls)
    0x09, 0x02,         // USAGE (Mouse)
    0xA1, 0x01,         // COLLECTION (Application)
    0x85, 0x02,         //     REPORT_ID (2)
    0x09, 0x01,         //     USAGE (Pointer)
    0xA1, 0x00,         //     COLLECTION (Physical)
    0x05, 0x09,         //         USAGE_PAGE (Button)
    0x19, 0x01,         //         USAGE_MINIMUM (0x01)
    0x29, 0x03,         //         USAGE_MAXIMUM (0x03)
    0x15, 0x00,         //         LOGICAL_MINIMUM (0)
    0x25, 0x01,         //         LOGICAL_MAXIMUM (1)
    0x75, 0x01,         //         REPORT_SIZE (1)
    0x95, 0x03,         //         REPORT_COUNT (3)
    0x81, 0x02,         //         INPUT (Data, Var, Abs)
    0x95, 0x01,         //         REPORT_COUNT (1)
    0x75, 0x05,         //         REPORT_SIZE (5)
    0x81, 0x03,         //         INPUT (Cnst, Var, Abs)
    0x05, 0x01,         //         USAGE_PAGE (Generic Desktop Ctrls)
    0x09, 0x30,         //         USAGE (X)
    0x09, 0x31,         //         USAGE (Y)
    0x16, 0x00, 0x00,   //         LOGICAL_MINIMUM (0)
    0x26, 0xFF, 0x7F,   //         LOGICAL_MAXIMUM (32767)
    0x36, 0x00, 0x00,   //         PHYSICAL_MINIMUM (0)
    0x46, 0xFF, 0x7F,   //         PHYSICAL_MAXIMUM (32767)
    0x75, 0x10,         //         REPORT_SIZE (16)
    0x95, 0x02,         //         REPORT_COUNT (2)
    0x81, 0x02,         //         INPUT (Data, Var, Abs)
    0xC0,               //     END_COLLECTION
    0x85, 0x03,         //     REPORT_ID (3)
    0x09, 0x38,         //     USAGE (Wheel)
    0x15, 0x81,         //     LOGICAL_MINIMUM (-127)
    0x25, 0x7F,         //     LOGICAL_MAXIMUM (127)
    0x35, 0x00,         //     PHYSICAL_MINIMUM (0) = Reset Physical Minimum
    0x45, 0x00,         //     PHYSICAL_MAXIMUM (0) = Reset Physical Maximum
    0x75, 0x08,         //     REPORT_SIZE (8)
    0x95, 0x01,         //     REPORT_COUNT (1)
    0x81, 0x06,         //     INPUT (Data, Var, Rel)
    0xC0,               // END_COLLECTION
    //
    // Relative mouse HID report descriptor
    //
    // Source: https://github.com/NicoHood/HID/blob/b16be57caef4295c6cd382a7e4c64db5073647f7/src/SingleReport/BootMouse.cpp#L26
    // - 8 buttons + X, Y, Wheel movement
    // - Boot protocol compatible
    //
    0x05, 0x01,         // USAGE_PAGE (Generic Desktop)	  54
    0x09, 0x02,         // USAGE (Mouse)
    0xa1, 0x01,         // COLLECTION (Application)
    0x85, 0x04,         //     Report ID (4)
    0x09, 0x01,         //     USAGE (Pointer)   Pointer and Physical are required by Apple Recovery
    0xa1, 0x00,         //     COLLECTION (Physical)
    0x05, 0x09,         //         USAGE_PAGE (Button) 8 Buttons
    0x19, 0x01,         //         USAGE_MINIMUM (Button 1)
    0x29, 0x08,         //         USAGE_MAXIMUM (Button 8)
    0x15, 0x00,         //         LOGICAL_MINIMUM (0)
    0x25, 0x01,         //         LOGICAL_MAXIMUM (1)
    0x95, 0x08,         //         REPORT_COUNT (8)
    0x75, 0x01,         //         REPORT_SIZE (1)
    0x81, 0x02,         //         INPUT (Data,Var,Abs)
    0x05, 0x01,         //         USAGE_PAGE (Generic Desktop) X, Y, Wheel
    0x09, 0x30,         //         USAGE (X)
    0x09, 0x31,         //         USAGE (Y)
    0x09, 0x38,         //         USAGE (Wheel)
    0x15, 0x81,         //         LOGICAL_MINIMUM (-127)
    0x25, 0x7f,         //         LOGICAL_MAXIMUM (127)
    0x75, 0x08,         //         REPORT_SIZE (8)
    0x95, 0x03,         //         REPORT_COUNT (3)
    0x81, 0x06,         //         INPUT (Data,Var,Rel)
    0xc0,               //     End Collection
    0xc0,               // End Collection
];

/// Mass storage gadget configuration
///
/// Basic mass storage device configuration for virtual media support
pub const MASS_STORAGE_CONFIG: &[u8] = &[
    // Basic mass storage configuration
    // This is minimal as the actual configuration is handled by the kernel
    // and the lun.0 subdirectory structure
];
