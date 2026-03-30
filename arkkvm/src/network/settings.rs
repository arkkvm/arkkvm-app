use std::env;
use std::str::FromStr;

use anyhow::Result;
use reqwest::Url;
use tracing::{info, warn};

use crate::config::get_config_manager;
use crate::config::types::NetworkConfig;
use crate::network::mdns;

pub async fn init_network_settings() {
    let config = get_network_settings().await;
    on_config_changed(&config).await;
}

pub async fn get_network_settings() -> NetworkConfig {
    let manager = get_config_manager();
    manager.get().await.network_config.clone()
}

pub async fn set_network_settings(settings: NetworkConfig) -> Result<()> {
    let mut settings = settings;

    if let Some(proxy) = settings.http_proxy.as_ref() {
        if proxy.is_empty() {
            settings.http_proxy = None;
        } else if let Err(e) = Url::parse(proxy) {
            return Err(anyhow::anyhow!(e));
        }
    }

    let manager = get_config_manager();
    let mut has_changed = false;
    manager
        .update(|config| {
            if config.network_config != settings {
                config.network_config = settings.clone();
                has_changed = true;
            }
        })
        .await?;

    if has_changed {
        on_config_changed(&settings).await;
    }

    Ok(())
}

async fn on_config_changed(config: &NetworkConfig) {
    // Set proxy
    update_proxy(&config.http_proxy);

    // Set mDNS
    mdns::update_mdns_options(
        config.hostname.clone().unwrap_or_default().as_str(),
        config.domain.clone().unwrap_or_default().as_str(),
        config.mdns_mode.as_str(),
    )
    .await;
}

fn update_proxy(proxy: &Option<String>) {
    if let Some(proxy) = proxy.as_ref() {
        if Url::from_str(proxy).is_ok() {
            unsafe { env::set_var("http_proxy", proxy) };
            unsafe { env::set_var("https_proxy", proxy) };
            info!("Setting proxy to {}", proxy);
        } else {
            unsafe { env::remove_var("http_proxy") };
            unsafe { env::remove_var("https_proxy") };
            warn!("Invalid proxy URL: {}", proxy);
        }
    } else {
        unsafe { env::remove_var("http_proxy") };
        unsafe { env::remove_var("https_proxy") };
        info!("Removing proxy");
    }
}
