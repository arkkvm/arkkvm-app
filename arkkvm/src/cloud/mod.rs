pub mod manager;
pub mod oidc;
pub mod types;
pub mod websocket;

use std::time::Duration;

pub use manager::CloudManager;
pub use types::*;

/// WebSocket connection timeout
pub const CLOUD_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// API request timeout
pub const CLOUD_API_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// OIDC request timeout
pub const CLOUD_OIDC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// WebSocket ping interval
pub const WEBSOCKET_PING_INTERVAL: Duration = Duration::from_secs(15);
