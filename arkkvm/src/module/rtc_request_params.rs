use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{
    config::types::{NetworkConfig, VirtualMediaItem, VlanSettings}, hardware::atx::ATXPowerAction,
};

#[derive(Deserialize, Serialize, Debug)]
pub struct SettingSwitchParams {
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct PathParams {
    pub path: String,
}

#[derive(Deserialize)]
pub struct FilePathParams {
    pub path: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct FtFileDownloadParams {
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub offset: u64,
}

#[derive(Debug, Deserialize)]
pub struct FileUploadParams {
    pub path: String,
    pub name: String,
    pub size: i64,
}

#[derive(Deserialize)]
pub struct LocalFileDownloadParams {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
}

#[derive(Debug, Deserialize)]
pub struct ATXPowerParams {
    pub action: ATXPowerAction,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DisplayRotationParams {
    pub rotation: DisplayRotation,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DisplayRotation {
    #[serde(rename = "90")]
    Normal,
    #[serde(rename = "270")]
    Reverse,
}

impl From<&str> for DisplayRotation {
    fn from(value: &str) -> Self {
        if value == "270" {
            DisplayRotation::Reverse
        } else {
            DisplayRotation::Normal
        }
    }
}

impl From<String> for DisplayRotation {
    fn from(value: String) -> Self {
        if value == "270".to_owned() {
            DisplayRotation::Reverse
        } else {
            DisplayRotation::Normal
        }
    }
}

impl From<DisplayRotation> for i32 {
    fn from(value: DisplayRotation) -> Self {
        match value {
            DisplayRotation::Normal => 1,
            DisplayRotation::Reverse => 0,
        }
    }
}

impl From<i32> for DisplayRotation {
    fn from(value: i32) -> Self {
        match value {
            0 => DisplayRotation::Normal,
            1 => DisplayRotation::Reverse,
            _ => {
                error!("Invalid display rotation value: {}", value);
                DisplayRotation::Normal
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BacklightSettingsParams {
    pub max_brightness: i32,
    pub dim_after: i32,
    pub off_after: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkSettingsParams {
    pub settings: NetworkConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VlanSettingsParams {
    pub settings: VlanSettings,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenewVlanDhcpLeaseParams {
    pub role: String,
}

#[derive(Deserialize)]
pub struct SshKeyParam {
    #[serde(rename = "sshKey")]
    pub ssh_key: String,
}

#[derive(Deserialize, Serialize, Debug, Default)]
pub struct VersionParams {
    #[serde(rename = "showUI")]
    pub show_ui: bool,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleParams {
    pub enabled: bool,
    pub login_server: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetbirdParams {
    pub enabled: bool,
    /// Self-hosted management URL; `None` / empty = NetBird Cloud.
    #[serde(default)]
    pub management_url: Option<String>,
    /// Peer hostname used on registration; `None` / empty = device hostname.
    #[serde(default)]
    pub hostname: Option<String>,
}

/// `setPrivateNetwork`: choose which overlay (if any) runs.
#[derive(Deserialize, Serialize, Debug)]
pub struct PrivateNetworkParams {
    pub target: crate::plugin::private_network::PrivateNetworkTarget,
}

/// `registerNetbird` / `registerNetbirdForce`. Without a setup key the
/// response carries an SSO URL. The key is never persisted.
#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetbirdRegisterParams {
    #[serde(default)]
    pub setup_key: Option<String>,
}

impl std::fmt::Debug for NetbirdRegisterParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetbirdRegisterParams")
            .field("setup_key", &self.setup_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct VirtualMediaListParams {
    pub list: Vec<VirtualMediaItem>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct RemoveVirtualMediaItemParams {
    pub name: String,
}