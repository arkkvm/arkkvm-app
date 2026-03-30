use serde::{Serialize, Deserialize};

use crate::module::rtc_request_params::DisplayRotation;

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