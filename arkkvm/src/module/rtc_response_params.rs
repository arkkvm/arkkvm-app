use serde::{Serialize, Deserialize};

use crate::{
    config::types::{IpV4Mod, IpV6Mod, VlanStaticIpConfig},
    module::rtc_request_params::DisplayRotation,
    network::{DhcpLease, RpcIPv6Address},
    plugin,
};

#[derive(Serialize, Deserialize)]
pub struct StartDownloadResponse {
    #[serde(rename = "dataChannel")]
    pub data_channel: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OtaState {
    pub updating: bool,
    pub error: Option<String>,
    pub app_download_progress: u32,
    pub app_download_finished_at: i64,
    pub app_verification_progress: u32,
    pub app_verified_at: i64,
    pub system_download_progress: u32,
    pub system_download_finished_at: i64,
    pub system_verification_progress: u32,
    pub system_verified_at: i64,
    pub app_update_progress: u32,
    pub app_updated_at: i64,
    pub system_update_progress: u32,
    pub system_updated_at: i64,
    pub by_user: bool,
}

impl OtaState {
    
    pub fn system_download(process: u32, by_user: bool) -> Self {
        Self {
            system_download_progress: process,
            system_download_finished_at: if process == 100 { chrono::Utc::now().timestamp() } else { 0 },
            by_user,
            .. Default::default()
        }
    }

    pub fn system_verified(process: u32, by_user: bool) -> Self {
        Self {
            system_verification_progress: process,
            system_verified_at: if process == 100 { chrono::Utc::now().timestamp() } else { 0 },
            by_user,
            .. Default::default()
        }
    }

    pub fn system_update(process: u32) -> Self {
        Self {
            system_update_progress: process,
            .. Default::default()
        }
    }

    pub fn error(error: String) -> Self {
        Self {
            error: Some(error),
            .. Default::default()
        }
    }

    pub fn updated_succeed() -> Self {
        Self {
            updating: true,
            .. Default::default()
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GuiSettingsResponse {
    pub rotation: DisplayRotation,
    pub max_brightness: i32,
    pub dim_after: i32,
    pub off_after: i32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleStateResponse {
    pub enabled: bool,
    pub login_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_state: Option<plugin::tailscale::ConnectionState>,
    pub status: Option<plugin::tailscale::StatusResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VlanEndpointResponse {
    pub vlan_id: u16,
    pub ipv4_mode: IpV4Mod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6_mode: Option<IpV6Mod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub static_ipv4: Option<VlanStaticIpConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub static_ipv6: Option<VlanStaticIpConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6_link_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4_addresses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6_addresses: Option<Vec<RpcIPv6Address>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dhcp_lease: Option<DhcpLease>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VlanSettingsResponse {
    pub vlan_enabled: bool,
    #[serde(rename = "primaryVlan", skip_serializing_if = "Option::is_none")]
    pub primary_vlan: Option<VlanEndpointResponse>,
    #[serde(rename = "secondaryVlan", skip_serializing_if = "Option::is_none")]
    pub secondary_vlan: Option<VlanEndpointResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingVlanSettings {
    #[serde(flatten)]
    pub settings: VlanSettingsResponse,
    pub confirm_seconds_remaining: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetVlanSettingsResponse {
    pub settings: VlanSettingsResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_settings: Option<PendingVlanSettings>,
    /// Present while pending: VLAN IPs the UI should use for confirmation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<VlanRedirectInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VlanRedirectEndpoint {
    pub vlan_id: u16,
    pub interface_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VlanRedirectInfo {
    #[serde(rename = "primaryVlan", skip_serializing_if = "Option::is_none")]
    pub primary_vlan: Option<VlanRedirectEndpoint>,
    #[serde(rename = "secondaryVlan", skip_serializing_if = "Option::is_none")]
    pub secondary_vlan: Option<VlanRedirectEndpoint>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetVlanSettingsResponse {
    pub confirm_within_seconds: u64,
    pub redirect: VlanRedirectInfo,
}