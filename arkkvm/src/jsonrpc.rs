//! JSON-RPC 2.0 protocol implementation for remote procedure calls.
//!
//! This module provides a complete JSON-RPC 2.0 implementation for handling
//! RPC requests sent through WebRTC data channels. It supports method calls,
//! error handling, and event notifications.
//!
//! # Components
//! - `JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcEvent`: Protocol data structures
//! - `RpcHandler` trait: Handler interface definition
//! - `RpcRegistry`: Method registration and management
//! - `JsonRpcProcessor`: Main message processor
//!
//! # Safety
//! - Input validation and parameter checking
//! - Error handling to prevent panics
//! - Type-safe parameter serialization/deserialization

use std::collections::HashMap;
use std::panic;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info, trace, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;

use crate::cloud::types::{CloudConnectionState, CloudState};
use crate::config::get_config_manager;
use crate::config::types::{UISwitch, UsbConfig};
use crate::hardware::atx::{ATXLedState, ATXPowerAction};
use crate::hardware::hdmi::edid;
use crate::hardware::usb::KeyboardState as HidKeyboardState;
use crate::hardware::usb::storage::FileTransferTarget;
use crate::jiggler::JigglerConfig;
use crate::jsonrpc::handlers::*;
use crate::module::rtc_request_params::{
    ATXPowerParams, BacklightSettingsParams, DisplayRotationParams, FilePathParams,
    FileUploadParams, NetworkSettingsParams, PathParams, RenewVlanDhcpLeaseParams,
    SettingSwitchParams, SshKeyParam, TailscaleParams, VersionParams, VlanSettingsParams,
};
use crate::module::rtc_response_params::OtaState;
use crate::services::gui_pipeline;
use crate::session::Session;
use crate::web::get_global_app_state;
// use crate::webrtc::{get_current_session, get_rpc_channel};

lazy_static::lazy_static! {
    pub static ref PROCESSOR: JsonRpcProcessor = JsonRpcProcessor::new(Arc::new(create_default_registry()));
}

/// JSON-RPC 2.0 request structure
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    pub id: Option<Value>,
}

/// JSON-RPC 2.0 response structure
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<Value>,
}

/// JSON-RPC 2.0 event structure (notification)
#[derive(Debug, Serialize)]
pub struct JsonRpcEvent {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC error structure
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

const JSONRPC_VERSION: &str = "2.0";

impl JsonRpcError {
    /// Parse error (-32700)
    pub fn parse_error() -> Self {
        Self { code: -32700, message: "Parse error".to_string(), data: None }
    }

    /// Invalid request (-32600)
    pub fn invalid_request() -> Self {
        Self { code: -32600, message: "Invalid Request".to_string(), data: None }
    }

    /// Method not found (-32601)
    pub fn method_not_found() -> Self {
        Self { code: -32601, message: "Method not found".to_string(), data: None }
    }

    /// Invalid params (-32602)
    pub fn invalid_params(data: Option<Value>) -> Self {
        Self { code: -32602, message: "Invalid params".to_string(), data }
    }

    /// Internal error (-32603)
    pub fn internal_error(data: Option<String>) -> Self {
        Self { code: -32603, message: "Internal error".to_string(), data: data.map(Value::String) }
    }
}

/// Build an error response; reduces duplicate Response construction in handle_message
fn error_response(error: JsonRpcError, id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse { jsonrpc: JSONRPC_VERSION.to_string(), result: None, error: Some(error), id }
}

/// RPC handler trait
pub trait RpcHandler: Send + Sync {
    /// Execute RPC method
    fn call(&self, params: Option<Value>) -> Result<Value>;

    /// Execute RPC method asynchronously
    fn call_async(&self, params: Option<Value>) -> BoxFuture<'_, Result<Value>>;
}

/// Simple function handler
pub struct FunctionHandler<F> {
    func: F,
}

impl<F> FunctionHandler<F>
where
    F: Fn(Option<Value>) -> Result<Value> + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F> RpcHandler for FunctionHandler<F>
where
    F: Fn(Option<Value>) -> Result<Value> + Send + Sync,
{
    fn call(&self, params: Option<Value>) -> Result<Value> {
        (self.func)(params)
    }

    fn call_async(&self, params: Option<Value>) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move { self.call(params) })
    }
}

/// Typed parameter handler
pub struct TypedHandler<P, R, F> {
    func: F,
    _phantom: std::marker::PhantomData<(P, R)>,
}

impl<P, R, F> TypedHandler<P, R, F>
where
    P: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: Fn(P) -> Result<R> + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func, _phantom: std::marker::PhantomData }
    }
}

impl<P, R, F> RpcHandler for TypedHandler<P, R, F>
where
    P: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: Fn(P) -> Result<R> + Send + Sync,
{
    fn call(&self, params: Option<Value>) -> Result<Value> {
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let params = match params {
                Some(p) => convert_parameters::<P>(p)
                    .map_err(|e| anyhow!("Parameter conversion failed: {}", e))?,
                None => return Err(anyhow!("Missing required parameters")),
            };

            let result = (self.func)(params)?;
            serde_json::to_value(result).map_err(|e| anyhow!("Serialization failed: {}", e))
        }));

        match result {
            Ok(value) => value,
            Err(panic_payload) => {
                let error_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    format!("Handler panicked: {}", s)
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    format!("Handler panicked: {}", s)
                } else {
                    "Handler panicked with unknown payload".to_string()
                };
                error!("RPC handler panic recovered: {}", error_msg);
                Err(anyhow!("Internal error: {}", error_msg))
            }
        }
    }

    fn call_async(&self, params: Option<Value>) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move { self.call(params) })
    }
}

/// No parameters handler
pub struct NoParamsHandler<R, F> {
    func: F,
    _phantom: std::marker::PhantomData<R>,
}

impl<R, F> NoParamsHandler<R, F>
where
    R: Serialize + Send + Sync + 'static,
    F: Fn() -> Result<R> + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func, _phantom: std::marker::PhantomData }
    }
}

impl<R, F> RpcHandler for NoParamsHandler<R, F>
where
    R: Serialize + Send + Sync + 'static,
    F: Fn() -> Result<R> + Send + Sync,
{
    fn call(&self, _params: Option<Value>) -> Result<Value> {
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let result = (self.func)()?;
            serde_json::to_value(result).map_err(|e| anyhow!("Serialization failed: {}", e))
        }));

        match result {
            Ok(value) => value,
            Err(panic_payload) => {
                let error_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    format!("Handler panicked: {}", s)
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    format!("Handler panicked: {}", s)
                } else {
                    "Handler panicked with unknown payload".to_string()
                };
                error!("RPC handler panic recovered: {}", error_msg);
                Err(anyhow!("Internal error: {}", error_msg))
            }
        }
    }

    fn call_async(&self, params: Option<Value>) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move { self.call(params) })
    }
}

pub struct AsyncHandler<F> {
    func: F,
}

impl<F> AsyncHandler<F>
where
    F: Fn(Option<Value>) -> BoxFuture<'static, Result<Value>> + Send + Sync + 'static,
{
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F> RpcHandler for AsyncHandler<F>
where
    F: Fn(Option<Value>) -> BoxFuture<'static, Result<Value>> + Send + Sync + 'static,
{
    fn call(&self, params: Option<Value>) -> Result<Value> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on((self.func)(params))
        })
    }

    fn call_async(&self, params: Option<Value>) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move { (self.func)(params).await })
    }
}

/// Provides flexible parameter type conversion with fallback strategies
fn convert_parameters<P: DeserializeOwned>(params: Value) -> Result<P> {
    // First try direct deserialization
    match serde_json::from_value::<P>(params.clone()) {
        Ok(result) => Ok(result),
        Err(primary_error) => {
            // Try enhanced conversion strategies
            // match enhanced_parameter_conversion::<P>(&params) {
            //     Ok(result) => Ok(result),
            //     Err(_) => {
            //         // Return the original error for better debugging
            //         Err(anyhow!("Parameter conversion failed: {}", primary_error))
            //     }
            // }
            Err(anyhow!("Parameter conversion failed: {}", primary_error))
        }
    }
}

/// Enhanced parameter conversion with type coercion strategies
// fn enhanced_parameter_conversion<P: DeserializeOwned>(params: &Value) -> Result<P> {
//     match params {
//         // Handle object parameters - convert Map to Struct via JSON round-trip
//         Value::Object(map) => {
//             let converted_map = convert_object_values(map)?;
//             let json_value = Value::Object(converted_map);
//             serde_json::from_value(json_value)
//                 .map_err(|e| anyhow!("Object conversion failed: {}", e))
//         }

//         // Handle array parameters with element type conversion
//         Value::Array(arr) => {
//             let converted_array = convert_array_values(arr)?;
//             let json_value = Value::Array(converted_array);
//             serde_json::from_value(json_value)
//                 .map_err(|e| anyhow!("Array conversion failed: {}", e))
//         }

//         // Handle scalar values with type coercion
//         _ => {
//             let converted_value = convert_scalar_value(params)?;
//             serde_json::from_value(converted_value)
//                 .map_err(|e| anyhow!("Scalar conversion failed: {}", e))
//         }
//     }
// }

/// Convert object values with type coercion
// fn convert_object_values(
//     map: &serde_json::Map<String, Value>,
// ) -> Result<serde_json::Map<String, Value>> {
//     let mut converted_map = serde_json::Map::new();

//     for (key, value) in map {
//         let converted_value = match value {
//             // Convert float64 to integer types where reasonable
//             Value::Number(n) if n.is_f64() => {
//                 if let Some(f_val) = n.as_f64() {
//                     if f_val.fract() == 0.0 && f_val >= 0.0 && f_val <= u32::MAX as f64 {
//                         Value::Number(serde_json::Number::from(f_val as u32))
//                     } else {
//                         value.clone()
//                     }
//                 } else {
//                     value.clone()
//                 }
//             }

//             // Convert string numbers to actual numbers where appropriate
//             Value::String(s) => {
//                 if let Ok(int_val) = s.parse::<i64>() {
//                     Value::Number(serde_json::Number::from(int_val))
//                 } else if let Ok(float_val) = s.parse::<f64>() {
//                     if let Some(number) = serde_json::Number::from_f64(float_val) {
//                         Value::Number(number)
//                     } else {
//                         // Invalid float, keep as string
//                         value.clone()
//                     }
//                 } else {
//                     value.clone()
//                 }
//             }

//             // Recursively convert nested objects and arrays
//             Value::Object(nested_map) => Value::Object(convert_object_values(nested_map)?),

//             Value::Array(nested_array) => Value::Array(convert_array_values(nested_array)?),

//             _ => value.clone(),
//         };

//         converted_map.insert(key.clone(), converted_value);
//     }

//     Ok(converted_map)
// }

/// Convert array values with element type coercion
// fn convert_array_values(arr: &[Value]) -> Result<Vec<Value>> {
//     let mut converted_array = Vec::new();

//     for value in arr {
//         let converted_value = match value {
//             // Convert float64 to uint8 for byte arrays
//             Value::Number(n) if n.is_f64() => {
//                 if let Some(f_val) = n.as_f64() {
//                     if (0.0..=255.0).contains(&f_val) && f_val.fract() == 0.0 {
//                         Value::Number(serde_json::Number::from(f_val as u8))
//                     } else {
//                         value.clone()
//                     }
//                 } else {
//                     value.clone()
//                 }
//             }

//             // Recursively convert nested structures
//             Value::Object(nested_map) => Value::Object(convert_object_values(nested_map)?),

//             Value::Array(nested_array) => Value::Array(convert_array_values(nested_array)?),

//             _ => value.clone(),
//         };

//         converted_array.push(converted_value);
//     }

//     Ok(converted_array)
// }

/// Convert scalar values with type coercion
// fn convert_scalar_value(value: &Value) -> Result<Value> {
//     match value {
//         Value::Number(n) if n.is_f64() => {
//             if let Some(f_val) = n.as_f64() {
//                 // Convert float to int if it's a whole number and within reasonable range
//                 if f_val.fract() == 0.0 && f_val >= i32::MIN as f64 && f_val <= i32::MAX as f64 {
//                     Ok(Value::Number(serde_json::Number::from(f_val as i32)))
//                 } else {
//                     Ok(value.clone())
//                 }
//             } else {
//                 Ok(value.clone())
//             }
//         }

//         Value::String(s) => {
//             // Try to convert string numbers to actual numbers
//             if let Ok(int_val) = s.parse::<i64>() {
//                 Ok(Value::Number(serde_json::Number::from(int_val)))
//             } else if let Ok(float_val) = s.parse::<f64>() {
//                 if let Some(number) = serde_json::Number::from_f64(float_val) {
//                     Ok(Value::Number(number))
//                 } else {
//                     // Invalid float, keep as string
//                     Ok(value.clone())
//                 }
//             } else {
//                 Ok(value.clone())
//             }
//         }

//         _ => Ok(value.clone()),
//     }
// }

/// RPC handler registry
pub struct RpcRegistry {
    handlers: HashMap<String, Arc<dyn RpcHandler>>,
}

impl RpcRegistry {
    /// Create new registry
    pub fn new() -> Self {
        Self { handlers: HashMap::new() }
    }

    /// Register RPC handler
    pub fn register<H>(&mut self, method: &str, handler: H)
    where
        H: RpcHandler + 'static,
    {
        self.handlers.insert(method.to_string(), Arc::new(handler));
    }

    /// Register function handler
    pub fn register_function<F>(&mut self, method: &str, func: F)
    where
        F: Fn(Option<Value>) -> Result<Value> + Send + Sync + 'static,
    {
        self.register(method, FunctionHandler::new(func));
    }

    /// Register typed handler
    pub fn register_typed<P, R, F>(&mut self, method: &str, func: F)
    where
        P: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: Fn(P) -> Result<R> + Send + Sync + 'static,
    {
        self.register(method, TypedHandler::new(func));
    }

    /// Register no-parameters handler
    pub fn register_no_params<R, F>(&mut self, method: &str, func: F)
    where
        R: Serialize + Send + Sync + 'static,
        F: Fn() -> Result<R> + Send + Sync + 'static,
    {
        self.register(method, NoParamsHandler::new(func));
    }

    /// Get handler
    pub fn get_handler(&self, method: &str) -> Option<&Arc<dyn RpcHandler>> {
        self.handlers.get(method)
    }

    /// List all registered methods
    pub fn list_methods(&self) -> Vec<&String> {
        self.handlers.keys().collect()
    }

    /// Register asynchronous handler
    pub fn register_async<F>(&mut self, method: &str, func: F)
    where
        F: Fn(Option<Value>) -> BoxFuture<'static, Result<Value>> + Send + Sync + 'static,
    {
        self.register(method, AsyncHandler::new(func));
    }
}

impl Default for RpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON-RPC processor
pub struct JsonRpcProcessor {
    registry: Arc<RpcRegistry>,
}

impl JsonRpcProcessor {
    /// Create new processor
    pub fn new(registry: Arc<RpcRegistry>) -> Self {
        Self { registry }
    }

    /// Handle JSON-RPC message
    pub async fn handle_message(&self, message: DataChannelMessage, channel: Arc<RTCDataChannel>) {
        let request: JsonRpcRequest = match serde_json::from_slice(message.data.as_ref()) {
            Ok(req) => req,
            Err(e) => {
                warn!("Failed to parse JSON-RPC request: {}", e);
                if let Err(e) = self
                    .send_response(&error_response(JsonRpcError::parse_error(), None), &channel)
                    .await
                {
                    error!("Failed to send error response: {}", e);
                }
                return;
            }
        };
        info!("RPC handle_message request: {:?}", request);
        let handler = match self.registry.get_handler(&request.method) {
            Some(h) => h,
            None => {
                warn!("Method not found: {}", request.method);
                if let Err(e) = self
                    .send_response(
                        &error_response(JsonRpcError::method_not_found(), request.id),
                        &channel,
                    )
                    .await
                {
                    error!("Failed to send error response: {}", e);
                }
                return;
            }
        };

        match handler.call_async(request.params).await {
            Ok(result) => {
                // JSON-RPC 2.0: notification (no id) must not get a response — saves ~60 serializations+sends/sec for input_unstable
                if request.id.is_some() {
                    let response = JsonRpcResponse {
                        jsonrpc: JSONRPC_VERSION.to_string(),
                        result: Some(result),
                        error: None,
                        id: request.id,
                    };
                    if let Err(e) = self.send_response(&response, &channel).await {
                        error!("Failed to send response: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("RPC handler requested method '{}' error: {}", request.method, e);
                if request.id.is_some() {
                    if let Err(e) = self
                        .send_response(
                            &error_response(
                                JsonRpcError::internal_error(Some(e.to_string())),
                                request.id,
                            ),
                            &channel,
                        )
                        .await
                    {
                        error!("Failed to send error response: {}", e);
                    }
                }
            }
        }
    }

    /// Handle JSON-RPC message
    pub async fn handle_input_message(&self, message: DataChannelMessage) {
        let request: JsonRpcRequest = match serde_json::from_slice(message.data.as_ref()) {
            Ok(req) => req,
            Err(e) => {
                warn!("RPC input Failed to parse JSON-RPC request: {}", e);
                return;
            }
        };

        let handler = match self.registry.get_handler(&request.method) {
            Some(h) => h,
            None => {
                warn!("RPC input Method not found: {}", request.method);
                return;
            }
        };

        if let Err(e) = handler.call_async(request.params).await {
            error!("RPC input handler requested method '{}' error: {}", request.method, e);
        }
    }

    /// Send response via RPC data channel
    async fn send_response(
        &self,
        response: &JsonRpcResponse,
        channel: &Arc<RTCDataChannel>,
    ) -> Result<()> {
        let message_bytes = serde_json::to_vec(response)?;
        trace!("Sending JSON-RPC response: {} bytes", message_bytes.len());

        if let Err(e) = channel.send(&message_bytes.into()).await {
            return Err(anyhow!("Failed to send RPC response: {}", e));
        }
        // } else {
        //     warn!("No RPC channel available for session: {}", session.id);
        // }

        Ok(())
    }

    /// Send event (notification) via RPC data channel
    pub async fn send_event(
        &self,
        method: &str,
        params: Option<Value>,
        session: Arc<Session>,
    ) -> Result<()> {
        let event = JsonRpcEvent {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.to_string(),
            params,
        };
        let message_bytes = serde_json::to_vec(&event)?;
        trace!("Sending JSON-RPC event: method={}, {} bytes", method, message_bytes.len());

        // Clone channel handle first so the RwLock read guard is dropped
        // before awaiting send(), avoiding lock-holding across await.
        let rpc_channel = session.rpc_channel.read().await.clone();
        if let Some(rpc_channel) = rpc_channel {
            if let Err(e) = rpc_channel.send(&message_bytes.into()).await {
                return Err(anyhow!("Failed to send RPC event: {}", e));
            }
        } else {
            warn!("No RPC channel available for session: {}", session.id);
        }

        Ok(())
    }
}

/// Default RPC handler implementations
pub mod handlers {
    use std::sync::OnceLock;

    use parking_lot::{Mutex, RwLock};
    use reqwest::Url;
    use serde_json::json;
    use tokio::time::Duration;
    use uuid::Uuid;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;

    use super::*;
    use crate::cloud::CloudManager;
    use crate::config::types::{NetworkConfig, UISwitch};
    use crate::hardware::usb as usb_mod;
    use crate::hardware::usb::UsbDeviceType;
    use crate::hardware::usb::storage::{
        self as storage_mod, FileTransferState, FileTransferTarget,
    };
    use crate::module::rtc_request_params::{SshKeyParam, TailscaleParams};
    use crate::module::rtc_response_params::{GuiSettingsResponse, StartDownloadResponse};
    use crate::network::{self, NetworkInterfaceState, RpcIPv6Address, RpcNetworkState};
    use crate::web::get_global_app_state;
    use crate::{audio, jiggler, ota, plugin, zenoh_bus};

    static USB_STATE: once_cell::sync::OnceCell<RwLock<String>> = once_cell::sync::OnceCell::new();
    static KEYBOARD_LED: once_cell::sync::OnceCell<RwLock<HidKeyboardState>> =
        once_cell::sync::OnceCell::new();
    static CLOUD_STATE: once_cell::sync::OnceCell<RwLock<CloudConnectionState>> =
        once_cell::sync::OnceCell::new();
    // static NETWORK_IP: once_cell::sync::OnceCell<RwLock<String>> = once_cell::sync::OnceCell::new();

    static DISPLAY_ROTATION: OnceLock<Mutex<String>> = OnceLock::new();
    static KEYBOARD_LAYOUT: OnceLock<Mutex<String>> = OnceLock::new();

    /// Ping handler: returns a static string to avoid per-request allocation
    pub fn ping() -> Result<&'static str> {
        Ok("pong")
    }
    // USB state
    pub fn get_usb_state() -> Result<String> {
        let cell = USB_STATE.get_or_init(|| parking_lot::RwLock::new("unknown".to_string()));
        Ok(cell.read().clone())
    }

    pub fn set_usb_state(state: String) -> Result<Value> {
        let cell = USB_STATE.get_or_init(|| parking_lot::RwLock::new("unknown".to_string()));
        *cell.write() = state;
        Ok(Value::Null)
    }

    // Keyboard LED state
    pub fn get_keyboard_led_state() -> Result<HidKeyboardState> {
        let cell =
            KEYBOARD_LED.get_or_init(|| parking_lot::RwLock::new(HidKeyboardState::default()));
        Ok(*cell.read())
    }

    pub fn set_keyboard_led_state(state: HidKeyboardState) -> Result<Value> {
        let cell =
            KEYBOARD_LED.get_or_init(|| parking_lot::RwLock::new(HidKeyboardState::default()));
        *cell.write() = state;
        Ok(Value::Null)
    }

    // Cloud connection state management
    pub async fn get_cloud_state() -> Result<CloudState> {
        // Get real-time state from CloudManager
        let cloud_manager = CloudManager::new();
        Ok(cloud_manager.get_cloud_state().await)
    }

    pub async fn deregister_device() -> Result<Value> {
        // Call CloudManager to deregister device
        let cloud_manager = CloudManager::new();
        cloud_manager.deregister_device().await?;
        Ok(Value::Null)
    }

    /// Reset configuration to defaults and persist
    pub async fn reset_config() -> Result<Value> {
        let config_manager = get_config_manager();
        let default_config = crate::config::types::Config::default();

        // disable microphone emulation
        handlers::set_microphone_emulation(SettingSwitchParams { enabled: default_config.microphone_emulation.unwrap_or(false) }).await?;

        // reset usb devices to default
        handlers::set_usb_devices(SetUsbDevicesParams {
            devices: UsbDevicesState {
                absolute_mouse: default_config.usb_devices.absolute_mouse,
                relative_mouse: default_config.usb_devices.relative_mouse,
                keyboard: default_config.usb_devices.keyboard,
                mass_storage: default_config.usb_devices.mass_storage_vm,
                microphone: default_config.usb_devices.microphone,
            },
        })
        .await?;

        config_manager
            .update(|cfg| {
                cfg.auto_update_enabled = default_config.auto_update_enabled;
                cfg.jiggler_config = default_config.jiggler_config;
                cfg.jiggler_enabled = default_config.jiggler_enabled;
                cfg.keyboard_layout = default_config.keyboard_layout;
                cfg.keyboard_macros = default_config.keyboard_macros;
                cfg.audio_playback = default_config.audio_playback;
                cfg.usb_devices = default_config.usb_devices;
                cfg.display_max_brightness = default_config.display_max_brightness;
                cfg.display_dim_after_sec = default_config.display_dim_after_sec;
                cfg.display_off_after_sec = default_config.display_off_after_sec;
                cfg.local_auth_mode = default_config.local_auth_mode;
                cfg.hashed_password = default_config.hashed_password;
                cfg.cloud_token = default_config.cloud_token;
                cfg.google_identity = default_config.google_identity;
                cfg.local_auth_token = default_config.local_auth_token;
                cfg.dev_channel_enabled = default_config.dev_channel_enabled;
                cfg.local_loopback_only = default_config.local_loopback_only;
                cfg.microphone_emulation = default_config.microphone_emulation;
                // TODO: Video Quality, Audio Quality, Device Display, HTTPS mode, Troubleshooting mode;
            })
            .await?;

        edid::update_edid(None).await?;

        let gui_config = gui_pipeline::ServerConfig::default();
        gui_pipeline::set_brightness(gui_config.luminance).await?;
        gui_pipeline::set_dark_screen_time(gui_config.dark_screen_time).await?;
        gui_pipeline::set_sleep_time(gui_config.sleep_time).await?;

        plugin::reset_tailscale().await?;
        info!("Configuration reset to default and saved");
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct CloudStateParam {
        pub state: String,
    }

    pub fn set_cloud_state(param: CloudStateParam) -> Result<Value> {
        let new_state = param
            .state
            .parse::<CloudConnectionState>()
            .map_err(|e| anyhow!("invalid cloud state: {}", e))?;
        let cell = CLOUD_STATE.get_or_init(|| RwLock::new(CloudConnectionState::NotConfigured));
        *cell.write() = new_state;
        Ok(Value::Null)
    }

    // Cloud URL configuration
    #[derive(Deserialize)]
    pub struct CloudUrlParams {
        #[serde(rename = "apiUrl")]
        pub api_url: String,
        #[serde(rename = "appUrl")]
        pub app_url: String,
    }

    pub async fn set_cloud_url(params: CloudUrlParams) -> Result<Value> {
        let api_url = params.api_url.trim();
        let app_url = params.app_url.trim();

        if api_url.is_empty() || app_url.is_empty() {
            return Err(anyhow!("API URL and app URL cannot be empty"));
        }

        let api_url = Url::parse(api_url)?;
        let app_url = Url::parse(app_url)?;

        let api_url_scheme = api_url.scheme();
        let app_url_scheme = app_url.scheme();

        if (api_url_scheme != "https" && api_url_scheme != "http")
            || (app_url_scheme != "http" && app_url_scheme != "https")
        {
            return Err(anyhow!("unsupported url scheme"));
        }

        let api_url_origin = api_url.origin().ascii_serialization();
        let app_url_origin = app_url.origin().ascii_serialization();

        if !check_url_valid(&api_url_origin, api_url.as_str())
            || !check_url_valid(&app_url_origin, app_url.as_str())
        {
            warn!("the url is not valid: api_url_origin={}, api_url={}", api_url_origin, api_url);
            warn!("app_url_origin={}, app_url={}", app_url_origin, app_url);
            return Err(anyhow!("the url is not valid"));
        }

        let cloud_manager = CloudManager::new();
        cloud_manager.set_cloud_url(&api_url_origin, &app_url_origin).await?;
        Ok(Value::Null)
    }

    fn check_url_valid(origin: &str, url: &str) -> bool {
        let origin_url = format!("{}/", origin);
        origin_url.as_str() == url || origin == url
    }

    // Network IPv4 address (skeleton)
    // pub fn get_network_ip_address() -> Result<String> {
    //     let cell = NETWORK_IP.get_or_init(|| RwLock::new(String::new()));
    //     Ok(cell.read().clone())
    // }

    // pub fn set_network_ip_address(param: NetworkIpParam) -> Result<Value> {
    //     let cell = NETWORK_IP.get_or_init(|| RwLock::new(String::new()));
    //     *cell.write() = param.ip;
    //     Ok(Value::Null)
    // }

    pub async fn get_network_state() -> RpcNetworkState {
        NetworkInterfaceState::get_instance().rpc_get_network_state().await
    }

    pub async fn get_ipv6_addresses() -> Vec<RpcIPv6Address> {
        NetworkInterfaceState::get_instance().get_ipv6_addresses().await
    }

    pub async fn get_network_settings() -> NetworkConfig {
        network::settings::get_network_settings().await
    }

    pub async fn set_network_settings(settings: NetworkConfig) -> Result<Value> {
        network::settings::set_network_settings(settings).await?;
        Ok(Value::Null)
    }

    pub async fn renew_dhcp_lease() -> Result<Value> {
        NetworkInterfaceState::get_instance().renew_dhcp_lease().await?;
        Ok(Value::Null)
    }

    pub async fn get_vlan_settings() -> Result<Value> {
        let response = network::vlan::get_vlan_settings_response().await?;
        Ok(serde_json::to_value(response)?)
    }

    pub async fn set_vlan_settings(settings: crate::config::types::VlanSettings) -> Result<Value> {
        let response = network::vlan::set_vlan_settings(settings).await?;
        Ok(serde_json::to_value(response)?)
    }

    pub async fn confirm_vlan_settings() -> Result<Value> {
        network::vlan::confirm_vlan_settings().await?;
        Ok(Value::Null)
    }

    pub async fn revert_vlan_settings() -> Result<Value> {
        network::vlan::revert_vlan_settings().await?;
        Ok(Value::Null)
    }

    pub async fn renew_vlan_dhcp_lease(role: &str) -> Result<Value> {
        network::vlan::renew_vlan_dhcp_lease(role).await?;
        Ok(Value::Null)
    }

    // Wake-on-LAN management
    pub async fn get_wake_on_lan_devices() -> Result<Vec<crate::config::types::WakeOnLanDevice>> {
        let mgr = get_config_manager();
        let cfg = mgr.get().await;
        Ok(cfg.wake_on_lan_devices)
    }

    #[derive(Deserialize)]
    pub struct SetWakeOnLanDevicesParams {
        pub devices: Vec<crate::config::types::WakeOnLanDevice>,
    }

    pub async fn set_wake_on_lan_devices(params: SetWakeOnLanDevicesParams) -> Result<Value> {
        let mgr = get_config_manager();
        let devices = params.devices;
        mgr.update(move |cfg| {
            cfg.wake_on_lan_devices = devices;
        })
        .await?;
        info!("Wake-on-LAN devices updated");
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct SendWOLMagicPacketParams {
        #[serde(rename = "macAddress")]
        pub mac_address: String,
    }

    pub async fn send_wol_magic_packet(params: SendWOLMagicPacketParams) -> Result<Value> {
        crate::wol::send_magic_packet(&params.mac_address).await?;
        Ok(Value::Null)
    }

    // Composite display state for frontend convenience
    // #[derive(Serialize)]
    // pub struct DisplayState {
    //     pub ip: String,
    //     pub usb_connected: bool,
    //     pub cloud_state: String,
    //     pub keyboard_led: HidKeyboardState,
    // }

    // pub fn get_display_state() -> Result<DisplayState> {
    //     let ip = get_network_ip_address().unwrap_or_default();
    //     let usb_connected = get_usb_state().unwrap_or_default() == "configured";
    //     let cloud_manager = CloudManager::new();
    //     let cloud_state = cloud_manager.get_state().as_str().to_string();
    //     let keyboard_led = get_keyboard_led_state().unwrap_or_default();
    //     Ok(DisplayState { ip, usb_connected, cloud_state, keyboard_led })
    // }

    // ---- USB HID input handlers (keyboard/mouse) ----
    #[derive(Deserialize)]
    pub struct KeyboardReportParams {
        pub modifier: u8,
        pub keys: Vec<u8>,
    }

    pub async fn keyboard_report(params: KeyboardReportParams) -> Result<Value> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        usb.key_put_keyboard(crate::proto::v1::KeyboardReportParams {
            modifier: params.modifier as u32,
            keys: params.keys,
        })
        .await?;
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct AbsMouseReportParams {
        pub x: i32,
        pub y: i32,
        pub buttons: u8,
    }

    pub async fn abs_mouse_report(params: AbsMouseReportParams) -> Result<Value> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        usb.key_put_absmouse(crate::proto::v1::AbsMouseReportParams {
            x: params.x,
            y: params.y,
            buttons: params.buttons as u32,
            by_user: true,
        })
        .await?;
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct RelMouseReportParams {
        pub dx: i8,
        pub dy: i8,
        pub buttons: u8,
    }

    pub async fn rel_mouse_report(params: RelMouseReportParams) -> Result<Value> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        usb.key_put_relmouse(crate::proto::v1::RelMouseReportParams {
            dx: params.dx as i32,
            dy: params.dy as i32,
            buttons: params.buttons as u32,
            by_user: true,
        })
        .await?;
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct WheelReportParams {
        #[serde(rename = "wheelY")]
        pub wheel_y: i8,
    }

    pub async fn wheel_report(params: WheelReportParams) -> Result<Value> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        usb.key_put_wheel(crate::proto::v1::WheelReportParams { wheel_y: params.wheel_y as u32 })
            .await?;
        Ok(Value::Null)
    }

    /// Get device ID
    pub fn get_device_id() -> Result<String> {
        Ok(crate::hardware::hw::get_device_id())
    }

    /// Reboot system
    #[derive(Deserialize)]
    pub struct RebootParams {
        #[serde(default)]
        pub force: bool,
    }

    pub fn reboot(params: RebootParams) -> Result<Value> {
        info!("Reboot requested, force: {}", params.force);

        let mut cmd = std::process::Command::new("reboot");
        if params.force {
            cmd.arg("-f");
        }

        match cmd.spawn() {
            Ok(_) => {
                tokio::spawn(async {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    std::process::exit(0);
                });
                Ok(Value::Null)
            }
            Err(e) => Err(anyhow!("Failed to execute reboot command: {}", e)),
        }
    }

    /// Serial port configuration
    #[derive(Deserialize)]
    pub struct SerialSettings {
        pub baud_rate: String,
        pub data_bits: String,
        pub stop_bits: String,
        pub parity: String,
    }

    #[derive(Serialize)]
    pub struct SerialSettingsResponse {
        pub baud_rate: String,
        pub data_bits: String,
        pub stop_bits: String,
        pub parity: String,
    }

    pub fn set_serial_settings(settings: SerialSettings) -> Result<Value> {
        // Validate baud rate
        let _baud_rate: u32 = settings
            .baud_rate
            .parse()
            .map_err(|_| anyhow!("Invalid baud rate: {}", settings.baud_rate))?;

        // Validate data bits
        let data_bits: u8 = settings
            .data_bits
            .parse()
            .map_err(|_| anyhow!("Invalid data bits: {}", settings.data_bits))?;
        if !(5..=8).contains(&data_bits) {
            return Err(anyhow!("Data bits must be between 5 and 8"));
        }

        // Validate stop bits
        match settings.stop_bits.as_str() {
            "1" | "1.5" | "2" => {}
            _ => return Err(anyhow!("Invalid stop bits: {}", settings.stop_bits)),
        }

        // Validate parity
        match settings.parity.as_str() {
            "none" | "odd" | "even" | "mark" | "space" => {}
            _ => return Err(anyhow!("Invalid parity: {}", settings.parity)),
        }

        info!("Serial settings updated successfully");
        Ok(Value::Null)
    }

    pub fn get_serial_settings() -> Result<SerialSettingsResponse> {
        Ok(SerialSettingsResponse {
            baud_rate: "115200".to_string(),
            data_bits: "8".to_string(),
            stop_bits: "1".to_string(),
            parity: "none".to_string(),
        })
    }

    /// Display rotation
    // #[derive(Deserialize)]
    // pub struct DisplayRotationParams {
    //     pub rotation: String,
    // }

    // #[derive(Serialize)]
    // pub struct DisplayRotationResponse {
    //     pub rotation: String,
    // }

    // pub fn set_display_rotation(params: DisplayRotationParams) -> Result<Value> {
    //     match params.rotation.as_str() {
    //         "0" | "90" | "180" | "270" => {
    //             let rotation_mutex = DISPLAY_ROTATION.get_or_init(|| Mutex::new("0".to_string()));
    //             *rotation_mutex.lock() = params.rotation.clone();
    //             info!("Display rotation set to: {}", params.rotation);
    //             Ok(Value::Null)
    //         }
    //         _ => Err(anyhow!("Invalid rotation value: {}", params.rotation)),
    //     }
    // }

    // pub fn get_display_rotation() -> Result<DisplayRotationResponse> {
    //     let rotation_mutex = DISPLAY_ROTATION.get_or_init(|| Mutex::new("0".to_string()));
    //     let rotation = rotation_mutex.lock().clone();
    //     Ok(DisplayRotationResponse { rotation })
    // }

    /// Backlight settings
    // #[derive(Deserialize)]
    // pub struct BacklightSettings {
    //     pub max_brightness: i32,
    //     pub dim_after: i32,
    //     pub off_after: i32,
    // }

    // #[derive(Serialize)]
    // pub struct BacklightSettingsResponse {
    //     pub max_brightness: i32,
    //     pub dim_after: i32,
    //     pub off_after: i32,
    // }

    // pub fn set_backlight_settings(settings: BacklightSettings) -> Result<Value> {
    //     if !(0..=255).contains(&settings.max_brightness) {
    //         return Err(anyhow!("max_brightness must be between 0 and 255"));
    //     }
    //     if settings.dim_after < 0 {
    //         return Err(anyhow!("dim_after must be a positive integer"));
    //     }
    //     if settings.off_after < 0 {
    //         return Err(anyhow!("off_after must be a positive integer"));
    //     }

    //     // Apply backlight settings
    //     if let Err(e) = std::fs::write(
    //         "/sys/class/backlight/backlight/brightness",
    //         settings.max_brightness.to_string(),
    //     ) {
    //         warn!("Failed to set brightness: {}", e);
    //     }

    //     info!(
    //         "Backlight settings applied: brightness={}, dim_after={}, off_after={}",
    //         settings.max_brightness, settings.dim_after, settings.off_after
    //     );
    //     Ok(Value::Null)
    // }

    // pub fn get_backlight_settings() -> Result<BacklightSettingsResponse> {
    //     let max_brightness =
    //         std::fs::read_to_string("/sys/class/backlight/backlight/max_brightness")
    //             .and_then(|s| {
    //                 s.trim()
    //                     .parse()
    //                     .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    //             })
    //             .unwrap_or(255);

    //     Ok(BacklightSettingsResponse { max_brightness, dim_after: 300, off_after: 600 })
    // }

    /// Stream quality
    #[derive(Deserialize)]
    pub struct StreamQualityParams {
        pub factor: f64,
    }

    pub fn get_audio_quality() -> Result<Value> {
        let quality_str = format!("{:.3}", crate::services::audio::get_audio_quality());
        Ok(serde_json::from_str(quality_str.as_str())?)
    }

    pub async fn set_audio_quality(params: StreamQualityParams) -> Result<Value> {
        crate::services::audio::update_audio_quality(params.factor as f32).await?;
        Ok(Value::Null)
    }

    pub async fn get_stream_quality_factor() -> f32 {
        crate::video::get_video_quality().await
    }

    pub async fn set_stream_quality_factor(params: StreamQualityParams) -> Result<Value> {
        info!("Stream quality factor set to: {}", params.factor);
        crate::video::update_video_quality(params.factor as f32).await?;
        Ok(Value::Null)
    }

    /// Auto update
    pub async fn get_auto_update_state() -> bool {
        ota::get_auto_update().await
    }

    pub async fn set_auto_update_state(params: SettingSwitchParams) -> Result<Value> {
        ota::set_auto_update(params.enabled).await?;
        Ok(Value::Null)
    }

    pub async fn get_video_state() -> Result<crate::video::VideoInputState> {
        Ok(crate::video::get_video_state().await)
    }

    pub async fn get_update_status() -> Result<Value> {
        Ok(serde_json::to_value(ota::check_update(true).await?)?)
    }

    pub async fn try_update() -> Result<Value> {
        Ok(serde_json::to_value(ota::try_update_by_user().await?)?)
    }

    #[derive(Deserialize)]
    pub struct KeyboardMacrosParams {
        pub macros: Vec<serde_json::Value>,
    }

    pub async fn get_keyboard_macros() -> Result<Vec<crate::config::KeyboardMacro>> {
        let mgr = get_config_manager();
        let cfg = mgr.get().await;
        Ok(cfg.keyboard_macros)
    }

    pub async fn set_keyboard_macros(params: KeyboardMacrosParams) -> Result<serde_json::Value> {
        if params.macros.is_empty() {
            anyhow::bail!("missing or invalid macros parameter");
        }

        // Validate macro count limit
        if params.macros.len() > crate::config::types::MAX_MACROS_PER_DEVICE {
            anyhow::bail!("too many macros (max {})", crate::config::types::MAX_MACROS_PER_DEVICE);
        }

        let mut new_macros = Vec::with_capacity(params.macros.len());
        for (i, macro_value) in params.macros.into_iter().enumerate() {
            let macro_obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_value(macro_value)
                    .map_err(|e| anyhow::anyhow!("invalid macro at index {}: {}", i, e))?;

            // Extract and validate fields
            let id =
                macro_obj.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(
                    || {
                        format!(
                            "macro-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        )
                    },
                );

            let name = macro_obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            let sort_order = macro_obj
                .get("sortOrder")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or((i + 1) as u32);

            // Parse steps
            let mut steps = Vec::new();
            if let Some(steps_array) = macro_obj.get("steps").and_then(|v| v.as_array()) {
                if steps_array.is_empty() {
                    anyhow::bail!("macro at index {} must have at least one step", i);
                }
                for step_value in steps_array.iter() {
                    let step_obj = match step_value.as_object() {
                        Some(obj) => obj,
                        None => continue,
                    };

                    let mut step = crate::config::KeyboardMacroStep {
                        keys: Vec::new(),
                        modifiers: Vec::new(),
                        delay: 0,
                    };

                    // Parse keys
                    if let Some(keys_array) = step_obj.get("keys").and_then(|v| v.as_array()) {
                        for key_value in keys_array {
                            if let Some(key_str) = key_value.as_str() {
                                step.keys.push(key_str.to_string());
                            }
                        }
                    }

                    // Parse modifiers
                    if let Some(mods_array) = step_obj.get("modifiers").and_then(|v| v.as_array()) {
                        for mod_value in mods_array {
                            if let Some(mod_str) = mod_value.as_str() {
                                step.modifiers.push(mod_str.to_string());
                            }
                        }
                    }

                    // Parse delay
                    if let Some(delay_value) = step_obj.get("delay").and_then(|v| v.as_u64()) {
                        step.delay = delay_value as u32;
                    }

                    steps.push(step);
                }
            }

            let mut macro_item =
                crate::config::KeyboardMacro { id, name, steps, sort_order: Some(sort_order) };

            // Validate macro
            if let Err(e) = macro_item.validate() {
                anyhow::bail!("invalid macro at index {}: {}", i, e);
            }

            new_macros.push(macro_item);
        }

        // Update configuration
        let mgr = get_config_manager();
        mgr.update(|cfg| {
            cfg.keyboard_macros = new_macros;
        })
        .await?;

        Ok(serde_json::Value::Null)
    }

    pub fn get_default_edid() -> Result<String> {
        Ok(edid::get_default_edid_str())
    }

    pub async fn get_edid() -> Result<String> {
        edid::get_edid_str().await
    }

    pub async fn set_edid(edid: Option<&str>) -> Result<Value> {
        edid::update_edid(edid).await?;
        Ok(Value::Null)
    }

    /// USB device management
    #[derive(Serialize, Deserialize, Debug)]
    pub struct UsbDevicesState {
        pub absolute_mouse: bool,
        pub relative_mouse: bool,
        pub keyboard: bool,
        pub mass_storage: bool,
        pub microphone: bool,
    }

    /// Frontend `setUsbDevices` payload: `{ "devices": { ... } }`
    #[derive(Deserialize, Debug)]
    pub struct SetUsbDevicesParams {
        pub devices: UsbDevicesState,
    }

    #[derive(Deserialize)]
    pub struct UsbDeviceStateParams {
        pub device: String,
        pub enabled: bool,
    }

    /// USB emulation enabled (gadget bound to UDC in usb_devices sidecar).
    pub async fn get_usb_emulation_state() -> Result<bool> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        usb.get_usb_emulation_state().await
    }

    #[derive(Deserialize)]
    pub struct UsbEmulationParams {
        pub enabled: bool,
    }

    pub async fn set_usb_emulation_state(params: UsbEmulationParams) -> Result<Value> {
        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        let enabled = usb.set_usb_emulation_state(params.enabled).await?;
        info!("USB emulation state set to: {}", enabled);
        Ok(Value::Null)
    }

    /// Keyboard layout
    pub fn get_keyboard_layout() -> Result<String> {
        let layout_mutex = KEYBOARD_LAYOUT.get_or_init(|| Mutex::new("us".to_string()));
        let layout = layout_mutex.lock().clone();
        Ok(layout)
    }

    #[derive(Deserialize)]
    pub struct KeyboardLayoutParams {
        pub layout: String,
    }

    pub fn set_keyboard_layout(params: KeyboardLayoutParams) -> Result<Value> {
        // Validate layout
        const VALID_LAYOUTS: &[&str] = &["us", "uk", "de", "fr", "es", "it", "jp", "kr"];
        if !VALID_LAYOUTS.contains(&params.layout.as_str()) {
            return Err(anyhow!("Unsupported keyboard layout: {}", params.layout));
        }

        let layout_mutex = KEYBOARD_LAYOUT.get_or_init(|| Mutex::new("us".to_string()));
        *layout_mutex.lock() = params.layout.clone();

        info!("Keyboard layout set to: {}", params.layout);
        Ok(Value::Null)
    }

    pub async fn get_microphone_emulation() -> bool {
        let manager = crate::config::get_config_manager();
        manager.get_emulation_microphone().await
    }

    pub async fn set_microphone_emulation(params: SettingSwitchParams) -> Result<Value> {
        info!(enabled = params.enabled, "jsonrpc set_microphone_emulation request");
        if params.enabled {
            let devices = get_config_manager().get_usb_devices().await;
            if !devices.microphone {
                return Err(anyhow!("microphone device is not enabled"));
            }
        }

        let usb =
            crate::services::get_usb().ok_or_else(|| anyhow!("USB service not initialized"))?;
        if let Err(e) = usb.set_mic_process(params.enabled).await {
            error!(
                enabled = params.enabled,
                error = %e,
                "jsonrpc set_microphone_emulation set_mic_process failed"
            );
            return Err(e);
        }

        if params.enabled {
            if let Err(e) = crate::services::init_virtual_mic_service().await {
                error!(
                    enabled = params.enabled,
                    error = %e,
                    "jsonrpc set_microphone_emulation init_virtual_mic failed"
                );
                return Err(e);
            }
        } else if let Err(e) = crate::services::uninit_virtual_mic_service().await {
            error!(
                enabled = params.enabled,
                error = %e,
                "jsonrpc set_microphone_emulation uninit_virtual_mic failed"
            );
            return Err(e);
        }

        if let Err(e) = get_config_manager().set_microphone_emulation(params.enabled).await {
            error!(
                enabled = params.enabled,
                error = %e,
                "jsonrpc set_microphone_emulation persist failed"
            );
            return Err(anyhow!("Failed to save microphone emulation: {:?}", e));
        }

        info!(enabled = params.enabled, "jsonrpc set_microphone_emulation success");
        Ok(Value::Null)
    }

    pub async fn get_camera_emulation() -> bool {
        let manager = crate::config::get_config_manager();
        manager.get_emulation_camera().await
    }

    pub async fn set_camera_emulation(params: SettingSwitchParams) -> Result<Value> {
        usb_mod::reboot_usb_manager_by_device(UsbDeviceType::Camera, params.enabled).await?;
        Ok(Value::Null)
    }

    pub async fn get_file_transfer() -> bool {
        let manager = crate::config::get_config_manager();
        manager.get_emulation_file_transfer().await
    }

    pub async fn set_file_transfer(params: SettingSwitchParams) -> Result<Value> {
        if !params.enabled
            && storage_mod::get_file_transfer_state().await?.target == FileTransferTarget::RemoteUsb
        {
            // If RemoteUsb FT is active, unmount first then disable switch.
            storage_mod::unmount_file_img().await?;
        }

        usb_mod::reboot_usb_manager_by_device(UsbDeviceType::MassStorageFt, params.enabled).await?;
        Ok(Value::Null)
    }

    pub async fn repair_file_transfer() -> Result<Value> {
        storage_mod::repair_file_transfer().await?;
        Ok(Value::Null)
    }

    pub async fn format_file_transfer() -> Result<Value> {
        storage_mod::format_file_transfer().await?;
        Ok(Value::Null)
    }

    pub async fn get_audio_playback() -> bool {
        let manager = crate::config::get_config_manager();
        manager.get_emulation_audio_playback().await
    }

    pub async fn set_audio_playback(params: SettingSwitchParams) -> Result<Value> {
        let manager = crate::config::get_config_manager();
        match manager.set_emulation_audio_playback(params.enabled).await {
            Ok(has_changed) => {
                if !has_changed {
                    return Ok(Value::Null);
                }
            }
            Err(e) => {
                return Err(anyhow!("Failed to set audio playback: {:?}", e));
            }
        }

        if params.enabled {
            tokio::time::sleep(Duration::from_millis(100)).await;
            audio::start_native_audio().await?;
        } else {
            audio::stop_native_audio().await;
        }
        Ok(Value::Null)
    }

    pub async fn get_ui_switch() -> UISwitch {
        let manager = crate::config::get_config_manager();
        manager.get().await.ui_switch.clone()
    }

    pub async fn set_ui_switch(params: UISwitch) -> Result<Value> {
        let manager = crate::config::get_config_manager();
        manager
            .update(|config| {
                if config.ui_switch != params {
                    config.ui_switch = params;
                }
            })
            .await?;
        Ok(Value::Null)
    }

    pub fn get_jiggler_state() -> Result<bool> {
        jiggler::get_jiggler_enabled()
    }

    pub async fn set_jiggler_state(params: SettingSwitchParams) -> Result<Value> {
        jiggler::set_jigglers(params.enabled).await?;
        Ok(Value::Null)
    }

    pub async fn get_jiggler_config() -> Result<JigglerConfig> {
        jiggler::get_jiggler_config().await
    }

    pub async fn set_jiggler_config(config: JigglerConfig) -> Result<Value> {
        jiggler::set_jiggler_config(config).await?;
        Ok(Value::Null)
    }

    pub async fn get_dev_channel_state() -> bool {
        let manager = crate::config::get_config_manager();
        manager.get_dev_channel_state().await
    }

    pub async fn set_dev_channel_state(params: SettingSwitchParams) -> Result<Value> {
        let manager = crate::config::get_config_manager();
        if let Err(e) = manager.set_dev_channel_state(params.enabled).await {
            return Err(anyhow!("Failed to set audio playback: {:?}", e));
        }
        Ok(Value::Null)
    }

    pub async fn get_usb_devices() -> UsbDevicesState {
        let manager = crate::config::get_config_manager();
        manager.get_usb_devices_state().await
    }

    pub async fn set_usb_devices(params: SetUsbDevicesParams) -> Result<Value> {
        let state = params.devices;
        info!(
            absolute_mouse = state.absolute_mouse,
            keyboard = state.keyboard,
            mass_storage = state.mass_storage,
            microphone = state.microphone,
            "jsonrpc set_usb_devices request"
        );
        let mut devices = get_config_manager().get_usb_devices().await;

        devices.absolute_mouse = state.absolute_mouse;
        devices.keyboard = state.keyboard;
        // keyboard + relative mouse share one HID function in sidecar
        devices.relative_mouse = state.keyboard;
        devices.mass_storage_vm = state.mass_storage;
        devices.mass_storage_ft = state.mass_storage;
        devices.microphone = state.microphone;

        // devices.camera = state.camera;
        // Legacy API only exposes one mass-storage flag; do not mirror it onto FT.

        info!("set_usb_devices -> apply sidecar: {:?}", devices);
        if let Err(e) =
            usb_mod::reboot_usb_manager_with_reason(None, Some(devices), "set_devices").await
        {
            error!(error = %e, "jsonrpc set_usb_devices failed");
            return Err(e);
        }
        info!("jsonrpc set_usb_devices success");
        Ok(Value::Null)
    }

    pub async fn get_usb_config() -> Result<UsbConfig> {
        Ok(crate::config::get_config_manager().get_usb_config().await)
    }

    pub async fn set_usb_config(usb_config: UsbConfig) -> Result<Value> {
        usb_mod::reboot_usb_manager_with_reason(Some(usb_config), None, "set_config").await?;
        Ok(Value::Null)
    }

    /// Network settings
    pub async fn get_local_loopback_only() -> Result<bool> {
        let config_manager = crate::config::get_config_manager();
        let config = config_manager.get().await;
        Ok(config.local_loopback_only)
    }

    #[derive(Deserialize)]
    pub struct LocalLoopbackParams {
        pub enabled: bool,
    }

    pub async fn set_local_loopback_only(params: LocalLoopbackParams) -> Result<Value> {
        let config_manager = crate::config::get_config_manager();

        config_manager
            .update(|config| {
                config.local_loopback_only = params.enabled;
            })
            .await?;

        let new_state = config_manager.get().await.local_loopback_only;

        info!("Local loopback only mode set to: {}", new_state);
        Ok(serde_json::to_value(new_state)?)
    }

    pub async fn on_atx_power_action(params: ATXPowerParams) -> Result<Value> {
        info!("on_atx_power_action: {:?}", params);
        let session = zenoh_bus::get_session();
        match params.action {
            ATXPowerAction::PowerLong => {
                session.put("extension/atx/action/pwr-btn-long-press", "").await.unwrap();
            }
            ATXPowerAction::PowerShort => {
                session.put("extension/atx/action/pwr-btn-short-press", "").await.unwrap();
            }
            ATXPowerAction::Reset => {
                session.put("extension/atx/action/rst-btn-short-press", "").await.unwrap();
            }
        }
        Ok(Value::Null)
    }

    pub async fn get_gui_config() -> Result<Value> {
        let config = gui_pipeline::get_config().await?;

        Ok(serde_json::to_value(GuiSettingsResponse {
            rotation: config.orientation.into(),
            max_brightness: config.luminance,
            dim_after: config.dark_screen_time,
            off_after: config.sleep_time,
        })?)
    }

    pub async fn set_gui_orientation(orientation: i32) -> Result<()> {
        gui_pipeline::set_orientation(orientation).await
    }

    pub async fn set_gui_brightness(brightness: i32) -> Result<()> {
        gui_pipeline::set_brightness(brightness).await
    }

    pub async fn set_gui_sleep_time(seconds: i32) -> Result<()> {
        gui_pipeline::set_sleep_time(seconds).await
    }

    pub async fn set_gui_dark_screen_time(seconds: i32) -> Result<()> {
        gui_pipeline::set_dark_screen_time(seconds).await
    }

    // =====================
    // Developer mode
    // =====================
    #[derive(Deserialize)]
    pub struct DevModeParams {
        pub enabled: bool,
    }

    /// Return developer mode state
    pub async fn get_dev_mode_state_handler() -> Result<crate::config::DevModeState> {
        let state = crate::config::get_dev_mode_state().await?;
        Ok(state)
    }

    /// Set developer mode state and restart/stop SSH via dropbear.sh
    pub async fn set_dev_mode_state_handler(params: DevModeParams) -> Result<Value> {
        crate::config::set_dev_mode_state(params.enabled).await?;
        // Try to start/stop SSH (best-effort)
        if params.enabled {
            network::ssh::start_sshd().await?;
        } else {
            network::ssh::stop_sshd().await?;
        }
        Ok(Value::Null)
    }

    /// Read authorized_keys content. Returns empty string if file does not exist.
    pub async fn get_ssh_key_state() -> Result<Value> {
        Ok(serde_json::to_value(network::ssh::get_ssh_key().await)?)
    }

    /// Write or remove authorized_keys based on input. When non-empty, ensure dir perms 0700 and file perms 0600.
    pub async fn set_ssh_key_state(param: SshKeyParam) -> Result<Value> {
        let ssk_key = param.ssh_key.trim();
        network::ssh::set_ssh_key(if ssk_key.is_empty() { None } else { Some(ssk_key) }).await?;
        Ok(Value::Null)
    }

    // =====================
    // Virtual media (storage)
    // =====================

    #[derive(Serialize)]
    pub struct VirtualMediaStateResponse {
        pub source: String,
        pub mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub filename: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
        pub size: i64,
    }

    pub fn get_virtual_media_state() -> Result<Option<VirtualMediaStateResponse>> {
        let s = storage_mod::get_virtual_media_state();
        let mapped = s.map(|st| VirtualMediaStateResponse {
            source: match st.source {
                storage_mod::VirtualMediaSource::WebRTC => "WebRTC".into(),
                storage_mod::VirtualMediaSource::HTTP => "HTTP".into(),
                storage_mod::VirtualMediaSource::Storage => "Storage".into(),
            },
            mode: match st.mode {
                storage_mod::VirtualMediaMode::CDROM => "CDROM".into(),
                storage_mod::VirtualMediaMode::Disk => "Disk".into(),
            },
            filename: st.filename,
            url: st.url,
            size: st.size,
        });
        Ok(mapped)
    }

    pub async fn get_file_transfer_state() -> Result<FileTransferState> {
        storage_mod::get_file_transfer_state().await
    }

    fn parse_vm_mode(mode: &str) -> Result<storage_mod::VirtualMediaMode> {
        match mode.to_ascii_uppercase().as_str() {
            "CDROM" => Ok(storage_mod::VirtualMediaMode::CDROM),
            "DISK" => Ok(storage_mod::VirtualMediaMode::Disk),
            _ => Err(anyhow!("invalid mode")),
        }
    }

    #[derive(Deserialize)]
    pub struct MountHttpParams {
        pub url: String,
        pub mode: String,
    }

    pub async fn mount_with_http(params: MountHttpParams) -> Result<Value> {
        let mode = parse_vm_mode(&params.mode)?;
        storage_mod::mount_with_http(&params.url, mode).await?;
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct MountWebRtcParams {
        pub filename: String,
        pub size: i64,
        pub mode: String,
    }

    pub async fn mount_with_webrtc(params: MountWebRtcParams) -> Result<Value> {
        let mode = parse_vm_mode(&params.mode)?;
        storage_mod::mount_with_webrtc(&params.filename, params.size, mode).await?;
        Ok(Value::Null)
    }

    #[derive(Deserialize)]
    pub struct MountStorageParams {
        pub filename: String,
        pub mode: String,
    }

    #[derive(Deserialize)]
    pub struct MountFileImgParams {
        pub target: FileTransferTarget,
    }

    pub async fn mount_with_storage(params: MountStorageParams) -> Result<Value> {
        let mode = parse_vm_mode(&params.mode)?;
        storage_mod::mount_with_storage(&params.filename, mode).await?;
        Ok(Value::Null)
    }

    pub async fn unmount_image() -> Result<Value> {
        storage_mod::unmount_image().await?;
        Ok(Value::Null)
    }

    pub async fn mount_kvm_with_file_img() -> Result<Value> {
        storage_mod::load_with_file_img().await?;
        Ok(Value::Null)
    }

    pub async fn mount_usb_with_file_img() -> Result<Value> {
        storage_mod::mount_with_file_img().await?;
        Ok(Value::Null)
    }

    pub async fn unmount_file_img() -> Result<Value> {
        storage_mod::unmount_file_img().await?;
        Ok(Value::Null)
    }

    pub async fn get_ft_file_system_info() -> Result<Value> {
        let info = storage_mod::fs_info_with_file_img().await?;
        Ok(serde_json::to_value(info).expect("FT file system info serialization failed"))
    }

    pub async fn get_ft_file_list(path: String) -> Result<Value> {
        let list = storage_mod::list_with_file_img(path).await?;
        Ok(serde_json::to_value(list).expect("FT file list serialization failed"))
    }

    pub async fn del_ft_file(path: String) -> Result<Value> {
        storage_mod::fs_del_file_with_file_img(path).await?;
        Ok(Value::Null)
    }

    pub async fn del_ft_dir(path: String) -> Result<Value> {
        storage_mod::fs_del_dir_with_file_img(path).await?;
        Ok(Value::Null)
    }

    pub async fn create_ft_dir(path: String) -> Result<Value> {
        storage_mod::fs_create_dir_with_file_img(path).await?;
        Ok(Value::Null)
    }

    pub async fn create_ft_file(path: String) -> Result<Value> {
        storage_mod::fs_create_empty_file_with_file_img(path).await?;
        Ok(Value::Null)
    }

    pub async fn download_ft_file(path: String, name: String) -> Result<Value> {
        let Some(session) = get_global_app_state().get_current_session().await else {
            return Err(anyhow!("Can not found current session"));
        };

        let file_path = format!("{}/{}", &path, &name).replace("//", "/");

        storage_mod::fs_file_exists(file_path.as_str()).await?;

        let channel_label = format!("tf_download_{}", Uuid::new_v4().to_string());
        let channel = session
            .create_data_channel(channel_label.as_str(), Some(RTCDataChannelInit::default()))
            .await?;

        let file_path_clone = file_path.clone();
        let channel_clone = channel.clone();
        info!("add open Listener on download task channel: {}", &channel_label);
        channel.on_open(Box::new(move || {
            info!("FT Download Data channel opened");
            Box::pin(async move {
                info!("FT Download task loop started");

                let file_info = match storage_mod::fs_get_file_info(file_path_clone.as_str()).await
                {
                    Ok(file_info) => file_info,
                    Err(e) => {
                        error!("FT Download get file info error: {:?}", e);
                        return;
                    }
                };

                let mut reader = match storage_mod::fs_download_file(file_path_clone.as_str()).await
                {
                    Ok(reader) => reader,
                    Err(e) => {
                        error!("FT Download create reader error: {:?}", e);
                        return;
                    }
                };

                let package_count =
                    (reader.file_size() as f64 / reader.chunk_size() as f64).ceil() as u64;

                let info = json!({
                    "path": &path,
                    "name": file_info.name,
                    "size": reader.file_size(),
                    "packageCount": package_count
                })
                .to_string();

                channel_clone.send_text(info).await.expect("failed to send head message");
                let mut index = 0;
                loop {
                    let _data = match reader.read_next_chunk().await {
                        Ok(data) => match data {
                            Some(data) => {
                                channel_clone.send(&data.into()).await.expect(
                                    format!("failed to send file data index: {}", index).as_str(),
                                );
                            }
                            None => {
                                info!("FT Download read next chunk finished");
                                break;
                            }
                        },
                        Err(e) => {
                            error!("FT Download read next chunk error: {:?}", e);
                            break;
                        }
                    };
                    index += 1;
                }

                channel_clone
                    .send_text(
                        json!({"type": "file_finished", "path": &path, "name": &name}).to_string(),
                    )
                    .await
                    .expect("failed to send end message");
                if let Err(e) = reader.shutdown().await {
                    warn!("Failed to shutdown reader: {:?}", e);
                }
                info!("Download task finished channel: {}", &channel_clone.label());
            })
        }));

        let channel_label_clone = channel.label().to_string().clone();
        channel.on_close(Box::new(move || {
            let channel_label = channel_label_clone.clone();
            Box::pin(async move {
                let Some(session) = get_global_app_state().get_current_session().await else {
                    warn!("Current Session not found");
                    return;
                };
                session.remove_channel(&channel_label).await;
                info!("Download task channel closed: {}", channel_label.as_str());
            })
        }));

        info!("Cache download task channel: {}", &channel_label);
        session.cache_channel(channel.clone()).await;
        info!("Starting download task channel: {}", &channel_label);
        Ok(serde_json::to_value(StartDownloadResponse { data_channel: channel_label })?)
    }

    pub async fn start_ft_upload(
        path: String,
        name: String,
        size: i64,
    ) -> Result<StartUploadResponse> {
        let up = storage_mod::start_ft_file_upload(path, name, size).await?;
        Ok(StartUploadResponse {
            already_uploaded_bytes: up.already_uploaded_bytes,
            data_channel: up.data_channel,
        })
    }

    // ---------- Storage files & uploads ----------

    #[derive(Serialize)]
    pub struct StorageSpaceResponse {
        #[serde(rename = "bytesUsed")]
        pub bytes_used: i64,
        #[serde(rename = "bytesFree")]
        pub bytes_free: i64,
    }

    #[derive(Serialize)]
    pub struct StorageFileInfo {
        pub filename: String,
        pub size: i64,
        #[serde(rename = "createdAt")]
        pub created_at: String,
        #[serde(rename = "totalBytes")]
        pub total_bytes: i64,
    }

    #[derive(Serialize)]
    pub struct StorageFilesResponse {
        pub files: Vec<StorageFileInfo>,
    }

    #[derive(Deserialize)]
    pub struct StartUploadParams {
        pub filename: String,
        pub size: i64,
    }

    #[derive(Serialize)]
    pub struct StartUploadResponse {
        #[serde(rename = "alreadyUploadedBytes")]
        pub already_uploaded_bytes: i64,
        #[serde(rename = "dataChannel")]
        pub data_channel: String,
    }

    pub async fn start_storage_file_upload(
        params: StartUploadParams,
    ) -> Result<StartUploadResponse> {
        let up = storage_mod::start_storage_file_upload(&params.filename, params.size).await?;
        Ok(StartUploadResponse {
            already_uploaded_bytes: up.already_uploaded_bytes,
            data_channel: up.data_channel,
        })
    }

    // List storage files
    pub async fn list_storage_files() -> Result<StorageFilesResponse> {
        let list = storage_mod::list_storage_files().await?;
        let files = list
            .files
            .into_iter()
            .map(|e| StorageFileInfo {
                filename: e.filename,
                size: e.size,
                created_at: e.created_at,
                total_bytes: e.total_bytes,
            })
            .collect();
        Ok(StorageFilesResponse { files })
    }

    #[derive(Deserialize)]
    pub struct DeleteStorageFileParams {
        pub filename: String,
    }

    pub async fn delete_storage_file(params: DeleteStorageFileParams) -> Result<Value> {
        storage_mod::delete_storage_file(&params.filename).await?;
        Ok(Value::Null)
    }

    // Disk space
    pub async fn get_storage_space() -> Result<StorageSpaceResponse> {
        let s = storage_mod::get_storage_space().await?;
        Ok(StorageSpaceResponse { bytes_used: s.bytes_used, bytes_free: s.bytes_free })
    }

    #[derive(Deserialize)]
    pub struct MountBuiltInImageParams {
        pub filename: String,
    }

    pub async fn mount_built_in_image(params: MountBuiltInImageParams) -> Result<Value> {
        storage_mod::mount_built_in_image(&params.filename).await?;
        Ok(Value::Null)
    }

    // ---- Mass storage mode (cdrom/file) ----
    #[derive(Deserialize)]
    pub struct MassStorageModeParams {
        pub mode: String, // "cdrom" | "file"
    }

    pub async fn set_mass_storage_mode(params: MassStorageModeParams) -> Result<String> {
        let mode = params.mode.to_ascii_lowercase();
        let cdrom = match mode.as_str() {
            "cdrom" => true,
            "file" => false,
            _ => return Err(anyhow!("invalid mode: {}", params.mode)),
        };
        // "file" (disk): writable; "cdrom": read-only
        storage_mod::set_mass_storage_mode(cdrom, cdrom, storage_mod::UsbTarget::Usb0Lun0).await?;
        get_mass_storage_mode().await
    }

    pub async fn get_mass_storage_mode() -> Result<String> {
        let mode = storage_mod::get_vm_mode_from_sidecar().await?;
        Ok(if mode == storage_mod::VirtualMediaMode::CDROM {
            "cdrom".to_string()
        } else {
            "file".to_string()
        })
    }

    // ---- Check mount URL usability (HTTP) ----
    #[derive(Deserialize)]
    pub struct CheckMountUrlParams {
        pub url: String,
    }

    #[derive(Serialize)]
    pub struct VirtualMediaUrlInfo {
        #[serde(rename = "Usable")]
        pub usable: bool,
        #[serde(rename = "Reason", skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        #[serde(rename = "Size")]
        pub size: i64,
    }

    pub fn check_mount_url(params: CheckMountUrlParams) -> Result<VirtualMediaUrlInfo> {
        let r = storage_mod::check_mount_url(&params.url)?;
        Ok(VirtualMediaUrlInfo { usable: r.usable, reason: r.reason, size: r.size })
    }

    // ---- Alias to keep protocol compatible ----
    #[derive(Deserialize)]
    pub struct RpcMountBuiltInImageParams {
        pub filename: String,
    }

    pub async fn rpc_mount_built_in_image(params: RpcMountBuiltInImageParams) -> Result<Value> {
        mount_built_in_image(MountBuiltInImageParams { filename: params.filename }).await
    }

    pub async fn get_tailscale_state() -> Result<Value> {
        match plugin::get_tailscale_state().await {
            Ok(state) => Ok(serde_json::to_value(state)?),
            Err(e) => Err(anyhow!("Failed to get tailscale state: {}", e)),
        }
    }

    pub async fn switch_tailscale(params: TailscaleParams) -> Result<Value> {
        plugin::switch_tailscale(params).await?;
        Ok(Value::Null)
    }

    pub async fn register_tailscale() -> Result<Value> {
        let result = plugin::register_tailscale().await?;
        Ok(serde_json::to_value(result)?)
    }

    pub async fn register_tailscale_force() -> Result<Value> {
        let result = plugin::register_tailscale_force().await?;
        Ok(serde_json::to_value(result)?)
    }
}

/// Broadcast helpers to current session over RPC
pub async fn broadcast_usb_state(state: String) {
    let _ = handlers::set_usb_state(state.clone());
    if let Some(session) = get_global_app_state().get_current_session().await {
        // let mut session = Session::new(session_id.clone());
        // if let Some(rpc_channel) = get_rpc_channel(&session_id).await {
        //     session.rpc_channel = Some(rpc_channel);
        // }
        let params = serde_json::Value::String(state);
        if let Err(e) = PROCESSOR.send_event("usbState", Some(params), session).await {
            warn!("Failed to send usbState event: {}", e);
        }
    }
}

pub async fn broadcast_keyboard_led_state(state: HidKeyboardState) {
    let _ = handlers::set_keyboard_led_state(state);
    if let Some(session) = get_global_app_state().get_current_session().await {
        // let mut session = Session::new(session_id.clone());
        // if let Some(rpc_channel) = get_rpc_channel(&session_id).await {
        //     session.rpc_channel = Some(rpc_channel);
        // }
        let params = serde_json::to_value(state).unwrap_or(serde_json::json!({}));
        if let Err(e) = PROCESSOR.send_event("keyboardLedState", Some(params), session).await {
            warn!("Failed to send keyboardLedState event: {}", e);
        }
    }
}

// pub async fn broadcast_virtual_cm_state(state: VirtualCMState) {
//     if let Some(session_id) = get_current_session().await {
//         let mut session = Session::new(session_id.clone());
//         if let Some(rpc_channel) = get_rpc_channel(&session_id).await {
//             session.rpc_channel = Some(rpc_channel);
//         }
//         let processor = JsonRpcProcessor::new(Arc::new(create_default_registry()));
//         let params = serde_json::to_value(state).unwrap_or(serde_json::json!({}));
//         if let Err(e) = processor.send_event("useMedia", Some(params), &session).await {
//             warn!("Failed to send useMedia event: {:?}", e);
//         }
//         info!("broadcast_virtual_cm_state Send Using media: {:?}, finished", state);
//     }
// }

pub async fn broadcast_atx_led_state(state: &ATXLedState) -> Result<()> {
    if let Some(session) = get_global_app_state().get_current_session().await {
        // let mut session = Session::new(session_id.clone());
        // if let Some(rpc_channel) = get_rpc_channel(&session_id).await {
        //     session.rpc_channel = Some(rpc_channel);
        // }
        let params = serde_json::to_value(state).unwrap_or(serde_json::json!({}));
        if let Err(e) = PROCESSOR.send_event("atxState", Some(params), session).await {
            warn!("Failed to send atxState event: {:?}", e);
            return Err(anyhow!("Failed to send atxState event: {:?}", e));
        }
        return Ok(());
    }
    Err(anyhow!("No session found"))
}

pub async fn broadcast_ota_state(state: OtaState) {
    if let Some(session) = get_global_app_state().get_current_session().await {
        // let mut session = Session::new(session_id.clone());
        // if let Some(rpc_channel) = get_rpc_channel(&session_id).await {
        //     session.rpc_channel = Some(rpc_channel);
        // }
        let params = serde_json::to_value(&state).unwrap_or(serde_json::json!({}));
        if let Err(e) = PROCESSOR.send_event("otaState", Some(params), session).await {
            warn!("Failed to send OTA State event: {:?}", e);
        }
        // info!("broadcast_ota_state Send OTA State: {:?}, finished", &state);
    }
}

/// Create default RPC registry
pub fn create_default_registry() -> RpcRegistry {
    let mut registry = RpcRegistry::new();

    // Basic methods
    registry.register_no_params("ping", handlers::ping);
    registry.register_no_params("getDeviceID", handlers::get_device_id);
    registry.register_typed("reboot", handlers::reboot);

    // TODO: 51 RPC methods not implemented, mainly include:
    // - Hardware control: USB HID, video, power management
    // - Virtual media: disk mounting, storage management
    // - Network functions: DHCP, WOL, cloud services
    // - System functions: updates, developer mode

    // Serial settings
    registry.register_typed("setSerialSettings", handlers::set_serial_settings);
    registry.register_no_params("getSerialSettings", handlers::get_serial_settings);

    // Display settings
    // registry.register_typed("setDisplayRotation", handlers::set_display_rotation);
    // registry.register_no_params("getDisplayRotation", handlers::get_display_rotation);

    // // Backlight settings
    // registry.register_typed("setBacklightSettings", handlers::set_backlight_settings);
    // registry.register_no_params("getBacklightSettings", handlers::get_backlight_settings);

    // Stream quality
    registry.register_async("getStreamQualityFactor", |_params| {
        Box::pin(async move {
            let quality_str = format!("{:.3}", handlers::get_stream_quality_factor().await);
            Ok(serde_json::from_str(quality_str.as_str())?)
        })
    });
    registry.register_async("setStreamQualityFactor", |params| {
        Box::pin(async move {
            let params: StreamQualityParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_stream_quality_factor(params).await
        })
    });
    registry.register_no_params("getAudioQuality", handlers::get_audio_quality);
    registry.register_async("setAudioQuality", |params| {
        Box::pin(async move {
            let params: StreamQualityParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_audio_quality(params).await
        })
    });

    // Auto update
    registry.register_async("getAutoUpdateState", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_auto_update_state().await)?) })
    });
    registry.register_async("setAutoUpdateState", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            Ok(serde_json::to_value(handlers::set_auto_update_state(params).await?)?)
        })
    });

    // EDID management
    registry.register_no_params("getDefaultEDID", handlers::get_default_edid);
    registry.register_async("getEDID", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_edid().await?)?) })
    });
    registry.register_async("setEDID", |params| {
        Box::pin(async move {
            let Some(params) = params else {
                return Err(anyhow!("Missing required parameters"));
            };

            let mut edid = params.get("edid").and_then(|v| v.as_str());
            if let Some(edid_str) = edid.as_ref() {
                if edid_str.trim().is_empty() {
                    edid = None;
                }
            }
            handlers::set_edid(edid).await
        })
    });

    // USB device management
    // registry.register_no_params("getUsbDevices", handlers::get_usb_devices);
    registry.register_async("getUsbEmulationState", |_params| {
        Box::pin(
            async move { Ok(serde_json::to_value(handlers::get_usb_emulation_state().await?)?) },
        )
    });
    registry.register_async("setUsbEmulationState", |params| {
        Box::pin(async move {
            let params: UsbEmulationParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_usb_emulation_state(params).await
        })
    });

    // USB HID input
    registry.register_async("keyboardReport", |params| {
        Box::pin(async move {
            let params: KeyboardReportParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::keyboard_report(params).await
        })
    });
    registry.register_async("absMouseReport", |params| {
        Box::pin(async move {
            let params: AbsMouseReportParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::abs_mouse_report(params).await
        })
    });

    registry.register_async("relMouseReport", |params| {
        Box::pin(async move {
            let params: RelMouseReportParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::rel_mouse_report(params).await
        })
    });
    registry.register_async("wheelReport", |params| {
        Box::pin(async move {
            let params: WheelReportParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::wheel_report(params).await
        })
    });

    // Keyboard layout
    registry.register_no_params("getKeyboardLayout", handlers::get_keyboard_layout);
    registry.register_typed("setKeyboardLayout", handlers::set_keyboard_layout);

    // Microphone Emulation
    registry.register_async("getMicrophoneEmulation", |_params| {
        Box::pin(
            async move { Ok(serde_json::to_value(handlers::get_microphone_emulation().await)?) },
        )
    });
    registry.register_async("setMicrophoneEmulation", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_microphone_emulation(params).await
        })
    });

    // Keyboard Emulation
    registry.register_async("getCameraEmulation", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_camera_emulation().await)?) })
    });
    registry.register_async("setCameraEmulation", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_camera_emulation(params).await
        })
    });

    // File Transfer
    registry.register_async("getFileTransfer", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_file_transfer().await)?) })
    });
    registry.register_async("setFileTransfer", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_file_transfer(params).await
        })
    });
    registry.register_async("repairFileTransfer", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::repair_file_transfer().await?)?) })
    });
    registry.register_async("formatFileTransfer", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::format_file_transfer().await?)?) })
    });

    // Audio Playback
    registry.register_async("getAudioPlayback", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_audio_playback().await)?) })
    });
    registry.register_async("setAudioPlayback", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_audio_playback(params).await
        })
    });

    registry.register_async("getUiSwitch", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_ui_switch().await)?) })
    });
    registry.register_async("setUiSwitch", |params| {
        Box::pin(async move {
            let params: UISwitch =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_ui_switch(params).await
        })
    });

    registry.register_no_params("getJigglerState", handlers::get_jiggler_state);
    registry.register_async("setJigglerState", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_jiggler_state(params).await
        })
    });

    registry.register_async("getJigglerConfig", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_jiggler_config().await?)?) })
    });
    registry.register_async("setJigglerConfig", |params| {
        Box::pin(async move {
            let params: JigglerConfig =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_jiggler_config(params).await
        })
    });

    registry.register_async("getDevChannelState", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_dev_channel_state().await)?) })
    });
    registry.register_async("setDevChannelState", |params| {
        Box::pin(async move {
            let params: SettingSwitchParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_dev_channel_state(params).await
        })
    });

    registry.register_async("getUsbDevices", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_usb_devices().await)?) })
    });
    registry.register_async("setUsbDevices", |params| {
        Box::pin(async move {
            let raw = params.ok_or(anyhow!("Missing required parameters"))?;
            let inner = raw.get("params").cloned().unwrap_or(raw);
            let params: handlers::SetUsbDevicesParams = serde_json::from_value(inner)?;
            handlers::set_usb_devices(params).await
        })
    });

    registry.register_async("getUsbConfig", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_usb_config().await?)?) })
    });
    registry.register_async("setUsbConfig", |params| {
        Box::pin(async move {
            let raw = params.ok_or(anyhow!("Missing required parameters"))?;
            let inner = raw.get("params").cloned().unwrap_or(raw);
            info!("Setting USB config: {:?}", &inner);
            let usb_config: UsbConfig = serde_json::from_value(inner)?;
            handlers::set_usb_config(usb_config).await
        })
    });

    // USB state & keyboard LED
    registry.register_no_params("getUSBState", handlers::get_usb_state);
    registry.register_no_params("getKeyboardLedState", handlers::get_keyboard_led_state);
    registry.register_async("getCurrentVersion", |params| {
        Box::pin(async move {
            let version_params: VersionParams = if let Some(value) = params {
                serde_json::from_value(value)?
            } else {
                VersionParams::default()
            };

            let result = crate::ota::get_current_version(version_params.show_ui).await?;
            Ok(serde_json::to_value(result)?)
        })
    });
    // OTA update status
    registry.register_async("getUpdateStatus", |_params| {
        Box::pin(async move { handlers::get_update_status().await })
    });
    registry.register_async("tryUpdate", |_params| {
        Box::pin(async move { handlers::try_update().await })
    });

    // TLS state
    registry.register_async("getTLSState", |_params| {
        Box::pin(async move {
            let result = crate::tls::get_tls_state().await;
            Ok(serde_json::to_value(result)?)
        })
    });
    registry.register_async("setTLSState", |params| {
        Box::pin(async move {
            let raw = params.ok_or(anyhow!("Missing required parameters"))?;
            let state_val = if let Some(s) = raw.get("state") { s.clone() } else { raw };
            let state: crate::tls::TlsState = serde_json::from_value(state_val)?;
            crate::tls::set_tls_state(&state).await?;
            Ok(serde_json::Value::Null)
        })
    });
    // Keyboard macros
    registry.register_async("getKeyboardMacros", |_params| {
        Box::pin(async move {
            let result = handlers::get_keyboard_macros().await?;
            Ok(serde_json::to_value(result)?)
        })
    });
    registry.register_async("setKeyboardMacros", |params| {
        Box::pin(async move {
            let params: handlers::KeyboardMacrosParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_keyboard_macros(params).await
        })
    });

    // Cloud/network/display
    registry.register_async("getCloudState", |_params| {
        Box::pin(async move {
            let result = handlers::get_cloud_state().await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_typed("setCloudState", handlers::set_cloud_state);

    registry.register_async("deregisterDevice", |_params| {
        Box::pin(async move { handlers::deregister_device().await })
    });

    // Reset configuration
    registry.register_async("resetConfig", |_params| {
        Box::pin(async move { handlers::reset_config().await })
    });

    registry.register_async("setCloudUrl", |params| {
        Box::pin(async move {
            let params: CloudUrlParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_cloud_url(params).await
        })
    });

    registry.register_async("getNetworkState", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_network_state().await)?) })
    });
    registry.register_async("getNetworkSettings", |_params| {
        Box::pin(async move { Ok(serde_json::to_value(handlers::get_network_settings().await)?) })
    });
    registry.register_async("setNetworkSettings", |params| {
        Box::pin(async move {
            let params: NetworkSettingsParams = serde_json::from_value(
                params.ok_or(anyhow!("Missing required parameters(NetworkSettingsParams)"))?,
            )?;
            handlers::set_network_settings(params.settings).await
        })
    });
    registry.register_async("renewDHCPLease", |_params| {
        Box::pin(async move { handlers::renew_dhcp_lease().await })
    });
    registry.register_async("getVlanSettings", |_params| {
        Box::pin(async move { handlers::get_vlan_settings().await })
    });
    registry.register_async("setVlanSettings", |params| {
        info!("setVlanSettings: {:?}", params);

        Box::pin(async move {
            let params: VlanSettingsParams = serde_json::from_value(
                params.ok_or(anyhow!("Missing required parameters(VlanSettingsParams)"))?,
            )?;
            handlers::set_vlan_settings(params.settings).await
        })
    });
    registry.register_async("confirmVlanSettings", |_params| {
        Box::pin(async move { handlers::confirm_vlan_settings().await })
    });
    registry.register_async("revertVlanSettings", |_params| {
        Box::pin(async move { handlers::revert_vlan_settings().await })
    });
    registry.register_async("renewVlanDhcpLease", |params| {
        Box::pin(async move {
            let params: RenewVlanDhcpLeaseParams = serde_json::from_value(
                params.ok_or(anyhow!("Missing required parameters(RenewVlanDhcpLeaseParams)"))?,
            )?;
            handlers::renew_vlan_dhcp_lease(&params.role).await
        })
    });
    // registry.register_no_params("getNetworkIpAddress", handlers::get_network_ip_address);
    // registry.register_typed("setNetworkIpAddress", handlers::set_network_ip_address);
    // registry.register_no_params("getDisplayState", handlers::get_display_state);

    // Network settings
    registry.register_async("getLocalLoopbackOnly", |_params| {
        Box::pin(async move {
            let result = handlers::get_local_loopback_only().await?;
            Ok(serde_json::to_value(result)?)
        })
    });
    registry.register_async("setLocalLoopbackOnly", |params| {
        Box::pin(async move {
            let params: handlers::LocalLoopbackParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_local_loopback_only(params).await
        })
    });

    // Wake-on-LAN
    registry.register_async("getWakeOnLanDevices", |_params| {
        Box::pin(async move {
            let list = handlers::get_wake_on_lan_devices().await?;
            Ok(serde_json::to_value(list)?)
        })
    });
    registry.register_async("setWakeOnLanDevices", |params| {
        Box::pin(async move {
            let raw = params.ok_or(anyhow!("Missing required parameters"))?;
            let inner = raw.get("params").ok_or_else(|| anyhow!("Missing field 'params'"))?.clone();
            let params: handlers::SetWakeOnLanDevicesParams = serde_json::from_value(inner)?;
            handlers::set_wake_on_lan_devices(params).await
        })
    });
    registry.register_async("sendWOLMagicPacket", |params| {
        Box::pin(async move {
            let params: handlers::SendWOLMagicPacketParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::send_wol_magic_packet(params).await
        })
    });

    // Virtual media (storage) RPCs
    registry.register_no_params("getVirtualMediaState", handlers::get_virtual_media_state);

    // File transfer State
    registry.register_async("getFileTransferState", |_params| {
        Box::pin(
            async move { Ok(serde_json::to_value(handlers::get_file_transfer_state().await?)?) },
        )
    });

    // Video state
    registry.register_async("getVideoState", |_params| {
        Box::pin(async move {
            let result = handlers::get_video_state().await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_async("mountWithHTTP", |params| {
        Box::pin(async move {
            let params: MountHttpParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::mount_with_http(params).await
        })
    });

    registry.register_async("mountWithWebRTC", |params| {
        Box::pin(async move {
            let params: MountWebRtcParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::mount_with_webrtc(params).await
        })
    });

    registry.register_async("mountWithStorage", |params| {
        Box::pin(async move {
            let params: MountStorageParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::mount_with_storage(params).await
        })
    });

    registry.register_async("unmountImage", |_params| {
        Box::pin(async move { handlers::unmount_image().await })
    });

    //File Transfer mount
    registry.register_async("mountWithFileImg", |params| {
        Box::pin(async move {
            let params: MountFileImgParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            match params.target {
                FileTransferTarget::Kvm => handlers::mount_kvm_with_file_img().await,
                FileTransferTarget::RemoteUsb => handlers::mount_usb_with_file_img().await,
                _ => Err(anyhow!("Unsupported file transfer target: {:?}", params.target)),
            }
        })
    });

    //File Transfer unmount
    registry.register_async("unmountWithFileImg", |_params| {
        Box::pin(async move { handlers::unmount_file_img().await })
    });

    registry.register_async("ftFileSystemInfo", |_params| {
        Box::pin(async move { handlers::get_ft_file_system_info().await })
    });

    registry.register_async("ftListFile", |params| {
        Box::pin(async move {
            let params: PathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::get_ft_file_list(params.path).await
        })
    });

    registry.register_async("ftDelFile", |params| {
        Box::pin(async move {
            let params: PathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::del_ft_file(params.path).await
        })
    });

    registry.register_async("ftCreateFile", |params| {
        Box::pin(async move {
            let params: PathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::create_ft_file(params.path).await
        })
    });

    registry.register_async("ftDelDir", |params| {
        Box::pin(async move {
            let params: PathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::del_ft_dir(params.path).await
        })
    });

    registry.register_async("ftCreateDir", |params| {
        Box::pin(async move {
            let params: PathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::create_ft_dir(params.path).await
        })
    });

    registry.register_async("startFtFsDownload", |params| {
        Box::pin(async move {
            let params: FilePathParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::download_ft_file(params.path, params.name).await
        })
    });

    registry.register_async("startFtFsUpload", |params| {
        Box::pin(async move {
            let params: FileUploadParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            info!("startFtFsUpload: {:?}", params);
            let result = handlers::start_ft_upload(params.path, params.name, params.size).await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    // Mass storage mode
    registry.register_async("setMassStorageMode", |params| {
        Box::pin(async move {
            let params: MassStorageModeParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            let result = handlers::set_mass_storage_mode(params).await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_async("getMassStorageMode", |_params| {
        Box::pin(async move {
            let result = handlers::get_mass_storage_mode().await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_typed("checkMountUrl", handlers::check_mount_url);

    registry.register_async("startStorageFileUpload", |params| {
        Box::pin(async move {
            let params: StartUploadParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            let result = handlers::start_storage_file_upload(params).await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_async("listStorageFiles", |_params| {
        Box::pin(async move {
            let result = handlers::list_storage_files().await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_async("deleteStorageFile", |params| {
        Box::pin(async move {
            let params: DeleteStorageFileParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::delete_storage_file(params).await
        })
    });

    registry.register_async("getStorageSpace", |_params| {
        Box::pin(async move {
            let result = handlers::get_storage_space().await?;
            Ok(serde_json::to_value(result)?)
        })
    });

    registry.register_async("mountBuiltInImage", |params| {
        Box::pin(async move {
            let params: MountBuiltInImageParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::mount_built_in_image(params).await
        })
    });

    registry.register_async("rpcMountBuiltInImage", |params| {
        Box::pin(async move {
            let params: RpcMountBuiltInImageParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::rpc_mount_built_in_image(params).await
        })
    });

    // SSH key management
    registry.register_async("getSSHKeyState", |_params| {
        Box::pin(async move { Ok(handlers::get_ssh_key_state().await?) })
    });
    registry.register_async("setSSHKeyState", |params| {
        Box::pin(async move {
            let params: SshKeyParam =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_ssh_key_state(params).await
        })
    });

    // Developer mode
    registry.register_async("getDevModeState", |_params| {
        Box::pin(async move {
            let result = handlers::get_dev_mode_state_handler().await?;
            Ok(serde_json::to_value(result)?)
        })
    });
    registry.register_async("setDevModeState", |params| {
        Box::pin(async move {
            let params: handlers::DevModeParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_dev_mode_state_handler(params).await
        })
    });

    registry.register_async("useMediaSuccess", |params| {
        Box::pin(async move {
            info!("jsonrpc useMediaSuccess: {:?}", params);
            Ok(Value::Null)
        })
    });

    registry.register_async("setATXPowerAction", |params| {
        Box::pin(async move {
            let params: ATXPowerParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::on_atx_power_action(params).await?;
            Ok(Value::Null)
        })
    });

    registry.register_async("getGuiSettings", |_params| {
        Box::pin(async move { Ok(handlers::get_gui_config().await?) })
    });

    registry.register_async("setDisplayRotation", |params| {
        Box::pin(async move {
            let params: DisplayRotationParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_gui_orientation(params.rotation.into()).await?;
            Ok(Value::Null)
        })
    });

    registry.register_async("setBacklightSettings", |params| {
        Box::pin(async move {
            let params: BacklightSettingsParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            handlers::set_gui_brightness(params.max_brightness).await?;
            handlers::set_gui_dark_screen_time(params.dim_after).await?;
            handlers::set_gui_sleep_time(params.off_after).await?;
            Ok(Value::Null)
        })
    });

    registry.register_async("getTailscaleState", |_params| {
        Box::pin(async move { Ok(handlers::get_tailscale_state().await?) })
    });

    registry.register_async("switchTailscale", |params| {
        Box::pin(async move {
            let params: TailscaleParams =
                serde_json::from_value(params.ok_or(anyhow!("Missing required parameters"))?)?;
            Ok(handlers::switch_tailscale(params).await?)
        })
    });

    registry.register_async("registerTailscale", |_params| {
        Box::pin(async move { Ok(handlers::register_tailscale().await?) })
    });

    registry.register_async("registerTailscaleForce", |_params| {
        Box::pin(async move { Ok(handlers::register_tailscale_force().await?) })
    });

    registry
}
