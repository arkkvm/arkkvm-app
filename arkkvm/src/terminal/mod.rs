//! Interactive terminals over WebRTC DataChannels.
//!
//! Public facades:
//! - [`WebTerminal`] — local `/bin/sh` on label `terminal`

mod core;
mod web;

pub use core::{TerminalHandle, TerminalSize};
pub use web::WebTerminal;
