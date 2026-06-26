use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Clone, Eq)]
pub enum UsbError {
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("UDC operation failed: {0}")]
    UDCError(String),

    #[error("Gadget operation failed: {0}")]
    GadgetError(String),

    #[error("Change rejected by upper layer: {0}")]
    ChangeRejected(String),

    #[error("Device not enabled: {0}")]
    DeviceNotEnabled(String),

    #[error("File not found: {0}")]
    DeviceNotFound(String),

    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("Strict mode violation: {0}")]
    StrictModeViolation(String),

    #[error("IO Error: {0}")]
    IoError(String),
}
