//! Control JSON structures for native <-> KVM IPC.
//!
//! Keep field names and shapes stable for cross-language compatibility.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Control action sent to native.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtrlAction {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Map<String, Value>>,
}

/// Response or event received from native.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtrlResponse {
    #[serde(default)]
    pub seq: i32,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub errno: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Map<String, Value>>,
    #[serde(default)]
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
