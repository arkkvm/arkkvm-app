use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{
    config::types::{NetworkConfig, VlanSettings},
    hardware::atx::ATXPowerAction,
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

#[derive(Debug, Deserialize)]
pub struct FileUploadParams {
    pub path: String,
    pub name: String,
    pub size: i64,
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