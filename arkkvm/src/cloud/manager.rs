use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use reqwest::{Client, StatusCode};
use rsa::sha2::{Digest, Sha256};
use salvo::http::HeaderValue;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use super::oidc::OidcAuthenticator;
use super::types::{
    CloudConnectionState, CloudRegisterRequest, CloudState, TokenExchangeRequest,
    TokenExchangeResponse,
};
use super::websocket::CloudWebSocketClient;
use crate::cloud::OtaInfo;
use crate::config::get_config_manager;
use crate::jsonrpc;
use crate::module::rtc_response_params::OtaState;

static CLOUD_MANAGER: once_cell::sync::OnceCell<CloudManager> = once_cell::sync::OnceCell::new();

pub fn get_cloud_manager() -> &'static CloudManager {
    CLOUD_MANAGER.get_or_init(CloudManager::new)
}

/// Cloud manager handling cloud connections and device registration
pub struct CloudManager {
    state: AtomicU8,
}

impl Default for CloudManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudManager {
    pub fn new() -> Self {
        Self { state: AtomicU8::new(CloudConnectionState::NotConfigured as u8) }
    }

    pub fn get_state(&self) -> CloudConnectionState {
        CloudConnectionState::from(self.state.load(Ordering::Relaxed))
    }

    pub fn set_state(&self, state: CloudConnectionState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    /// Get current cloud state
    pub async fn get_cloud_state(&self) -> CloudState {
        let config_manager = get_config_manager();
        let config = config_manager.get().await;

        CloudState {
            connected: config.cloud_token.is_some() && !config.cloud_url.is_empty(),
            url: Some(config.cloud_url),
            app_url: Some(config.cloud_app_url),
        }
    }

    /// Register device with cloud
    pub async fn register_device(&self, req: CloudRegisterRequest) -> Result<()> {
        info!("Starting cloud device registration");

        let config_manager = get_config_manager();
        let cfg = config_manager.get().await;

        let cloud_api = if !cfg.cloud_url.is_empty() {
            cfg.cloud_url.clone()
        } else if !req.cloud_api.is_empty() {
            req.cloud_api.clone()
        } else {
            anyhow::bail!("Cloud URL is not configured");
        };

        // 1. Exchange temporary token for permanent auth token
        let token_resp = self.exchange_temp_token(&req.token, &cloud_api).await?;
        info!("Token exchange successful");

        // 2. Verify OIDC token
        let oidc_auth = OidcAuthenticator::new().await?;
        let google_identity =
            oidc_auth.verify_token_with_client_id(&req.oidc_google, &req.client_id).await?;
        info!("OIDC token verification successful");

        // 3. Update configuration
        if cfg.cloud_url.is_empty() {
            config_manager.set_cloud_config(Some(cloud_api), Some(token_resp.secret_token)).await?;
        } else {
            config_manager
                .set_cloud_config(Some(cfg.cloud_url), Some(token_resp.secret_token))
                .await?;
        }

        // 4. Set Google identity
        config_manager
            .update(|config| {
                config.google_identity = Some(google_identity);
            })
            .await?;

        info!("Cloud device registration completed successfully");

        // trigger cloud connect on the shared manager
        get_cloud_manager().set_state(CloudConnectionState::Disconnected);

        Ok(())
    }

    /// Deregister device from cloud
    pub async fn deregister_device(&self) -> Result<()> {
        let config_manager = get_config_manager();
        let config = config_manager.get().await;

        if config.cloud_token.is_none() || config.cloud_url.is_empty() {
            return Err(anyhow!("Cloud token or URL is not set"));
        }

        let client = Client::new();
        let token = config.cloud_token.as_ref().ok_or_else(|| anyhow!("Cloud token is not set"))?;
        let response = client
            .delete(format!("{}/devices/{}", config.cloud_url, config.device_id))
            .header("Authorization", format!("Bearer {}", token))
            .timeout(super::CLOUD_API_REQUEST_TIMEOUT)
            .send()
            .await?;

        // Consider both 200 OK and 404 Not Found as successful deregistration
        if response.status().is_success() || response.status().as_u16() == 404 {
            // Clear cloud configuration
            config_manager.set_cloud_config(None, None).await?;
            config_manager
                .update(|config| {
                    config.google_identity = None;
                })
                .await?;

            info!("Device deregistered, disconnecting from cloud");
            self.set_state(CloudConnectionState::NotConfigured);
            Ok(())
        } else {
            Err(anyhow!("Deregister request failed with status: {}", response.status()))
        }
    }

    pub async fn get_ota_info(&self, os_version: &str, app_version: &str) -> Result<OtaInfo> {
        let config_manager = get_config_manager();
        let config = config_manager.get().await;

        if config.cloud_url.is_empty() {
            return Err(anyhow!("Cloud token or URL is not set"));
        }

        let response = match Client::new()
            .get(format!(
                "{}/releases?prerelease={}&appVersion={}&systemVersion={}",
                config.cloud_url,
                config.dev_channel_enabled,
                app_version,
                os_version,
            ))
            .timeout(super::CLOUD_API_REQUEST_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to send OTA Requset: {:?}", &e);
                return Err(anyhow!("Failed to send OTA Requset: {:?}", &e));
            }
        };

        match response.status() {
            StatusCode::OK => Ok(response.json::<OtaInfo>().await?),
            _ => {
                let status = response.status();
                let msg = response.text().await;
                error!("Failed to requset OTA update status: {:?}, msg: {:?}", &status, &msg);
                return Err(anyhow!(
                    "Failed to requset OTA update status: {:?}, msg: {:?}",
                    &status,
                    &msg
                ));
            }
        }
    }

    pub async fn download_file(
        &self,
        url: &str,
        path: &str,
        by_user: &'static AtomicBool,
    ) -> Result<String> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let client = Client::new();
        let mut downloaded: u64 = 0;
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 12;

        info!("Start to download file from {}", url);

        // Get total size from the first request
        let total_size = {
            let response =
                client.head(url).timeout(std::time::Duration::from_secs(30)).send().await?;
            let headers = response.headers();
            // info!("Headers: {:?}", &headers);
            headers
                .get("Content-Length")
                .unwrap_or(&HeaderValue::from_str("0")?)
                .to_str()?
                .parse::<u64>()?
        };

        info!("Total size of the file is {}", total_size);

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await?;

        loop {
            // Continue downloading from the breakpoint using a Range request
            let mut request = client.get(url).timeout(std::time::Duration::from_secs(300));

            if downloaded > 0 {
                warn!("Resuming download from byte {}", downloaded);
                request = request.header("Range", format!("bytes={}-", downloaded));
            }

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to send download request: {}", &e);
                    retry_count += 1;
                    if retry_count > MAX_RETRIES {
                        return Err(anyhow!(
                            "Failed to send download request after {} retries: {}",
                            MAX_RETRIES,
                            &e
                        ));
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if !response.status().is_success() && response.status().as_u16() != 206 {
                return Err(anyhow!("Download file failed: HTTP {}", response.status()));
            }

            let mut stream = response.bytes_stream();
            let mut should_retry = false;

            while let Some(chunk_result) = stream.next().await {
                let chunk = match chunk_result {
                    Ok(chunk) => {
                        if retry_count != 0 {
                            retry_count = 0;
                        }
                        chunk
                    },
                    Err(err) => {
                        warn!("Error reading chunk at byte {}: {}", downloaded, &err);
                        should_retry = true;
                        retry_count += 1;
                        if retry_count > MAX_RETRIES {
                            return Err(anyhow!(
                                "Download failed after {} retries: {}",
                                MAX_RETRIES,
                                &err
                            ));
                        }
                        break;
                    }
                };

                file.write_all(&chunk).await?;
                downloaded += chunk.len() as u64;

                let progress = (downloaded as f64 / total_size as f64 * 100.0) as u32;
                if downloaded % (1024 * 1024) < chunk.len() as u64 {
                    // Record progress every MB
                    info!("Download progress: {}% ({}/{})", progress, downloaded, total_size);
                    jsonrpc::broadcast_ota_state(OtaState::system_download(
                        progress,
                        by_user.load(Ordering::Relaxed),
                    ))
                    .await;
                }
            }

            // If a retry is needed, continue the outer loop
            if should_retry {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            // Download complete; check size
            if downloaded >= total_size {
                break;
            } else {
                warn!("Download incomplete: {}/{}, retrying...", downloaded, total_size);
                retry_count += 1;
                if retry_count > MAX_RETRIES {
                    return Err(anyhow!("Download incomplete after {} retries", MAX_RETRIES));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }

        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        debug!("Download finished: {} (total {} bytes)", path, downloaded);
        jsonrpc::broadcast_ota_state(OtaState::system_download(
            100,
            by_user.load(Ordering::Relaxed),
        ))
        .await;

        self.compute_hex(path, by_user).await
    }

    async fn compute_hex(
        &self,
        path: &str,
        by_user: &'static AtomicBool,
    ) -> Result<String> {
        info!("Start verifying file integrity...");
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 8192]; // 8KB buffer
        let mut verified_bytes: u64 = 0;
        let file_len = file.metadata().await?.len();
        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
            verified_bytes += bytes_read as u64;

            if verified_bytes % (10 * 1024 * 1024) < bytes_read as u64 {
                let progress = if file_len > 0 {
                    (verified_bytes as f64 / file_len as f64 * 100.0) as u32
                } else {
                    0
                };
                info!("Verification progress: {}% ({}/{})", progress, verified_bytes, file_len);
                jsonrpc::broadcast_ota_state(OtaState::system_verified(
                    progress,
                    by_user.load(Ordering::Relaxed),
                ))
                .await;
            }
        }

        let hash_result = hasher.finalize();
        Ok(hex::encode(hash_result))
    }

    /// Set cloud URL configuration
    pub async fn set_cloud_url(&self, api_url: &str, app_url: &str) -> Result<()> {
        let config_manager = get_config_manager();
        let current_config = config_manager.get().await;

        // Check if URL is changing
        if current_config.cloud_url != api_url {
            info!("Cloud URL changed from {} to {}", current_config.cloud_url, api_url);
            // Disconnect from current cloud if connected
            self.set_state(CloudConnectionState::Disconnected);
        }

        // Update configuration
        config_manager
            .update(|config| {
                config.cloud_url = api_url.to_string();
                config.cloud_app_url = app_url.to_string();
            })
            .await?;

        info!("Cloud URL configuration updated: API={}, App={}", api_url, app_url);
        Ok(())
    }

    /// Start cloud connection loop
    pub async fn start_connection_loop(&self) -> Result<()> {
        info!("Starting cloud connection loop");

        loop {
            match self.get_state() {
                CloudConnectionState::NotConfigured => {
                    debug!("Cloud connection loop: NotConfigured");
                    // Check if cloud configuration exists
                    let config_manager = get_config_manager();
                    let config = config_manager.get().await;

                    if config.cloud_token.is_some() && !config.cloud_url.is_empty() {
                        self.set_state(CloudConnectionState::Disconnected);
                    } else {
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
                CloudConnectionState::Disconnected => {
                    warn!("Cloud connection loop: Disconnected");
                    self.set_state(CloudConnectionState::Connecting);
                    match self.connect_to_cloud().await {
                        Ok(_) => {
                            // Connection successful, state will be set to Connected in connect_to_cloud
                        }
                        Err(e) => {
                            // Check if it's UnexpectedEof error
                            let is_unexpected_eof =
                                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                                    io_err.kind() == ErrorKind::UnexpectedEof
                                } else {
                                    false
                                };

                            if is_unexpected_eof {
                                info!(
                                    "Cloud connection closed by peer (UnexpectedEof), treating as normal disconnect"
                                );
                            } else {
                                warn!("Cloud connection failed: {}", e);
                            }

                            self.set_state(CloudConnectionState::Disconnected);

                            // Use shorter retry delay for UnexpectedEof
                            let retry_delay = if is_unexpected_eof {
                                tokio::time::Duration::from_secs(1)
                            } else {
                                tokio::time::Duration::from_secs(5)
                            };

                            tokio::time::sleep(retry_delay).await;
                        }
                    }
                }
                CloudConnectionState::Connecting | CloudConnectionState::Connected => {
                    debug!("Cloud connection loop: Connecting or Connected");
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Exchange temporary token for permanent token
    async fn exchange_temp_token(
        &self,
        temp_token: &str,
        cloud_api: &str,
    ) -> Result<TokenExchangeResponse> {
        let client = Client::new();
        let payload = TokenExchangeRequest { temp_token: temp_token.to_string() };

        let response = client
            .post(format!("{}/devices/token", cloud_api))
            .json(&payload)
            .timeout(super::CLOUD_API_REQUEST_TIMEOUT)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!("Token exchange failed: {}", response.status()));
        }

        let token_resp: TokenExchangeResponse = response.json().await?;
        Ok(token_resp)
    }

    /// Connect to cloud WebSocket
    async fn connect_to_cloud(&self) -> Result<()> {
        let config_manager = get_config_manager();
        let config = config_manager.get().await;

        let token = config.cloud_token.ok_or_else(|| {
            self.set_state(CloudConnectionState::NotConfigured);
            anyhow!("No cloud token available")
        })?;
        let device_id = config.device_id.clone();

        info!("Connecting to cloud WebSocket: {}", config.cloud_url);
        let mut client = CloudWebSocketClient::new(config.cloud_url, token, device_id);

        self.set_state(CloudConnectionState::Connected);

        match client.connect().await {
            Ok(_) => {
                info!("Successfully connected to cloud");
                Ok(())
            }
            Err(e) => {
                // Check if it's UnexpectedEof error
                if let Some(io_err) = e.downcast_ref::<std::io::Error>()
                    && io_err.kind() == ErrorKind::UnexpectedEof
                {
                    info!(
                        "WebSocket connection closed by peer without close_notify (UnexpectedEof)"
                    );
                    self.set_state(CloudConnectionState::Disconnected);
                    return Ok(());
                }

                self.set_state(CloudConnectionState::Disconnected);
                Err(e)
            }
        }
    }

    pub async fn get_web_version_info() -> Result<String> {
        let response = match Client::new()
            .get(format!(
                "{}/version",
                "http://localhost:80"
            ))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to request web version: {:?}", &e);
                return Err(anyhow!("Failed to request web version: {:?}", &e));
            }
        };

        match response.status() {
            StatusCode::OK => Ok(response.text().await?.trim().replace("\n", "")),
            _ => {
                let status = response.status();
                let msg = response.text().await;
                error!("Failed to get web version: {:?}, msg: {:?}", &status, &msg);
                return Err(anyhow!(
                    "Failed to get web version: {:?}, msg: {:?}",
                    &status,
                    &msg
                ));
            }
        }
    }
}
