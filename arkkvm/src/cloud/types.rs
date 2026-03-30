use serde::{Deserialize, Serialize};

/// Cloud connection states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloudConnectionState {
    NotConfigured = 0,
    Disconnected = 1,
    Connecting = 2,
    Connected = 3,
}

impl From<u8> for CloudConnectionState {
    fn from(value: u8) -> Self {
        match value {
            0 => CloudConnectionState::NotConfigured,
            1 => CloudConnectionState::Disconnected,
            2 => CloudConnectionState::Connecting,
            3 => CloudConnectionState::Connected,
            _ => CloudConnectionState::NotConfigured,
        }
    }
}

impl CloudConnectionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CloudConnectionState::NotConfigured => "notConfigured",
            CloudConnectionState::Disconnected => "disconnected",
            CloudConnectionState::Connecting => "connecting",
            CloudConnectionState::Connected => "connected",
        }
    }
}

impl std::str::FromStr for CloudConnectionState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "notConfigured" => Ok(CloudConnectionState::NotConfigured),
            "disconnected" => Ok(CloudConnectionState::Disconnected),
            "connecting" => Ok(CloudConnectionState::Connecting),
            "connected" => Ok(CloudConnectionState::Connected),
            _ => Err(format!(
                "Invalid CloudConnectionState: '{}'. Valid values are: notConfigured, disconnected, connecting, connected",
                s
            )),
        }
    }
}

/// Cloud registration request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudRegisterRequest {
    #[serde(alias = "tempToken")]
    pub token: String,
    #[serde(rename = "cloudApi", alias = "cloud_api", default)]
    pub cloud_api: String,
    #[serde(rename = "oidcGoogle", alias = "OidcGoogle")]
    pub oidc_google: String,
    #[serde(rename = "clientId", alias = "clientID")]
    pub client_id: String,
}

/// Token exchange request for cloud API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenExchangeRequest {
    #[serde(rename = "tempToken")]
    pub temp_token: String,
}

/// Token exchange response from cloud API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenExchangeResponse {
    #[serde(rename = "secretToken")]
    pub secret_token: String,
}

/// Cloud state response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudState {
    pub connected: bool,
    pub url: Option<String>,
    #[serde(rename = "appUrl")]
    pub app_url: Option<String>,
}

/// WebRTC session request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRTCSessionRequest {
    pub sd: String,
    #[serde(rename = "OidcGoogle")]
    pub oidc_google: Option<String>,
    pub ip: Option<String>,
    #[serde(rename = "iceServers")]
    pub ice_servers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OtaInfo {
    pub app_version: Option<String>,
    pub app_url: Option<String>,
    pub app_hash: Option<String>,
    pub system_version: Option<String>,
    pub system_url: Option<String>,
    pub system_hash_signature: Option<String>,
}