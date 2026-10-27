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

/// Cloud registration request.
///
/// De-providerized (doc auth-analysis/05 §2.1): the request carries ONLY the
/// cloud-issued tempToken — no IdP material. Legacy frontends still POST
/// `oidcGoogle`/`clientId`; serde ignores unknown fields, so old webapps keep
/// working unchanged. The device trusts the cloud (the TLS response of
/// POST /devices/token is the authorization assertion), never the user's IdP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudRegisterRequest {
    #[serde(alias = "tempToken")]
    pub token: String,
    #[serde(rename = "cloudApi", alias = "cloud_api", default)]
    pub cloud_api: String,
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
    /// Cloud user id of the device owner (additive field, doc 05 §2.1).
    /// `#[serde(default)]` keeps responses from older clouds (no userId
    /// yet) parseable; None simply means "owner unknown".
    #[serde(default, rename = "userId")]
    pub user_id: Option<i64>,
}

/// Cloud state response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudState {
    pub connected: bool,
    pub url: Option<String>,
    #[serde(rename = "appUrl")]
    pub app_url: Option<String>,
}

/// One credential-bearing ICE entry from the cloud offer's additive
/// `iceServersCred` field. Prefer these over bare `iceServers` URLs when
/// present — TURN needs per-session username/credential a plain URL list
/// cannot carry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IceServerCred {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

/// WebRTC session request (offer frame from the cloud relay).
///
/// The oidcGoogle field is gone: the device never validates the user's IdP
/// (the cloud already authorized the session; the dead serde-mismatched
/// field only ever carried a stale token, doc 05 §2.1). Older clouds still
/// sending `oidcGoogle` are fine — serde drops unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRTCSessionRequest {
    pub sd: String,
    pub ip: Option<String>,
    #[serde(default, rename = "iceServers")]
    pub ice_servers: Vec<String>,
    /// Credentialed ICE entries (TURN). Empty when the cloud omitted the
    /// field (legacy) or has no TURN issuer configured.
    #[serde(default, rename = "iceServersCred")]
    pub ice_servers_cred: Vec<IceServerCred>,
    #[serde(default, rename = "clientType")]
    pub client_type: Option<String>,
}

#[cfg(test)]
mod ice_servers_cred_tests {
    use super::*;

    #[test]
    fn parses_bare_ice_servers_only() {
        let req: WebRTCSessionRequest = serde_json::from_str(
            r#"{"sd":"v=0","iceServers":["stun:stun.arkkvm.cn:3478"]}"#,
        )
        .unwrap();
        assert_eq!(req.ice_servers, vec!["stun:stun.arkkvm.cn:3478"]);
        assert!(req.ice_servers_cred.is_empty());
    }

    #[test]
    fn parses_ice_servers_cred() {
        let req: WebRTCSessionRequest = serde_json::from_str(
            r#"{
                "sd":"v=0",
                "iceServers":["stun:stun.arkkvm.cn:3478"],
                "iceServersCred":[{
                    "urls":["turn:stun.arkkvm.cn:3478?transport=udp"],
                    "username":"1791979895",
                    "credential":"c2VjcmV0"
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(req.ice_servers_cred.len(), 1);
        assert_eq!(req.ice_servers_cred[0].username, "1791979895");
        assert_eq!(req.ice_servers_cred[0].credential, "c2VjcmV0");
        assert_eq!(
            req.ice_servers_cred[0].urls,
            vec!["turn:stun.arkkvm.cn:3478?transport=udp"]
        );
    }

    #[test]
    fn parses_missing_ice_fields_as_empty() {
        let req: WebRTCSessionRequest = serde_json::from_str(r#"{"sd":"v=0"}"#).unwrap();
        assert!(req.ice_servers.is_empty());
        assert!(req.ice_servers_cred.is_empty());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OtaInfo {
    pub app_version: Option<String>,
    pub app_url: Option<String>,
    pub app_hash: Option<String>,
    pub system_version: Option<String>,
    pub system_summary: Option<String>,
    pub system_url: Option<String>,
    pub system_hash_signature: Option<String>,
}