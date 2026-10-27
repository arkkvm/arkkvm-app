pub mod manager;
pub mod signaling_frame;
pub mod types;
pub mod websocket;

use std::time::Duration;

pub use manager::CloudManager;
pub use types::*;

/// WebSocket connection timeout
pub const CLOUD_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// API request timeout
pub const CLOUD_API_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// WebSocket ping interval
pub const WEBSOCKET_PING_INTERVAL: Duration = Duration::from_secs(15);
/// Inbound liveness timeout for the cloud WebSocket. The cloud (gorilla)
/// answers every protocol Ping with a Pong, so a healthy connection sees an
/// inbound frame at least once per ping interval; silence for three whole
/// intervals means the socket is stranded (e.g. its source address was
/// replaced underneath it) and the connection must be torn down and retried.
pub const WEBSOCKET_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
