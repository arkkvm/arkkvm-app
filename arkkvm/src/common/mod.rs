use anyhow::Result;
use tokio::process::Command;
use tracing::warn;

use crate::cloud::CloudManager;

pub mod log;
pub mod panic_handler;

const CAT_CMD: &str = "cat";
const SYSTEM_VERSION_FILE: &str = "/etc/version/system";

const DEV_TAG: &str = "-dev";

pub async fn get_system_version() -> Result<String> {
    let system_out = Command::new(CAT_CMD).args([SYSTEM_VERSION_FILE]).output().await?;
    let system_version = String::from_utf8(system_out.stdout)?.replace("\n", "");
    Ok(system_version.trim().to_owned())
}

pub fn get_app_version(ui_show: bool) -> String {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    
    // only show dev tag when the version need to view in the web UI
    if !ui_show {
        if version.ends_with(DEV_TAG) {
            version = version[..version.len() - 4].to_string();
        }
    }

    version
}

pub async fn get_web_version(ui_show: bool) -> String {
    let mut version = match CloudManager::get_web_version_info().await {
        Ok(version) => version,
        Err(e) => {
            warn!("Failed to get web version: {:?}", e);
            format!("")
        },
    };

    // only show dev tag when the version need to view in the web UI
    if !ui_show {
        if version.ends_with(DEV_TAG) {
            version = version[..version.len() - 4].to_string();
        }
    }

    version.trim().to_owned()
}