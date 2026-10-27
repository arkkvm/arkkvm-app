use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use base64::Engine;
use reqwest::{Client, StatusCode};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::sha2::{Digest, Sha256};
use rsa::{Oaep, RsaPublicKey};
use salvo::http::HeaderValue;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use super::types::{
    CloudConnectionState, CloudRegisterRequest, CloudState, TokenExchangeRequest,
    TokenExchangeResponse,
};
use super::websocket::CloudWebSocketClient;
use crate::cloud::{OtaInfo, websocket};
use crate::config::get_config_manager;
use crate::config::types::CLOUD_API_URL;
use crate::jsonrpc;
use crate::module::rtc_response_params::OtaState;

pub const PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvWvPLrqK9fvwcLPHnik2
x73lcC9Nmv3rAain35tp9G/ULUTQeEOfQa5wT/Dlx8P61s3ZmFmvn3FopYK8CH85
87dILdljOIXbPayeUEO/QWyLnfCqedx0hFHk7zbPNinVT1T6HFWGBKcJfvC6Slbh
OysBeW2hzQlTRb23iz3nGiFiOSvl+fSTRV4ohicVzaNXWMA3fdbUEVW7n8JFk9F1
80W/UfHcaC3wRrsF9bw6jBhPeT9z86hb7Kl6/JXSYjXslSfYfLZRsisFwRUTwApv
M5VOWHvyVDm8Ks/e32smN+p4b79ktnNkMLOluier4j3kn9eU/ka95GK6Vnt98lqK
wQIDAQAB
-----END PUBLIC KEY-----";

fn encrypt_device_id_rsa(device_id: &str) -> Result<String> {
    let pub_key = RsaPublicKey::from_public_key_pem(PUBLIC_KEY)
        .map_err(|e| anyhow!("PUBLIC_KEY parse: {}", e))?;
    let mut rng = OsRng;
    let ciphertext =
        pub_key.encrypt(&mut rng, Oaep::new::<Sha256>(), device_id.as_bytes()).map_err(|e| {
            anyhow!("device_id RSA encrypt: {}", e)
        })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&ciphertext))
}

static CLOUD_MANAGER: once_cell::sync::OnceCell<CloudManager> = once_cell::sync::OnceCell::new();

pub fn get_cloud_manager() -> &'static CloudManager {
    CLOUD_MANAGER.get_or_init(CloudManager::new)
}

/// Gate that suspends cloud reconnects while a network transition (IP /
/// gateway / DNS change driven by `refresh-network.sh`) is in flight.
///
/// Without it the connection loop observes `Disconnected` the moment the old
/// socket is torn down and immediately reconnects on the *old* address; when
/// the refresh then swaps the address a couple of seconds later, that fresh
/// socket is stranded and the cloud WS turns into a zombie (see plan
/// "云端WS僵尸连接修复"). The generation counter makes a stale
/// `end_network_transition` from a superseded transition a no-op.
#[derive(Debug, Default)]
struct ReconnectGate {
    generation: u64,
    paused: bool,
}

impl ReconnectGate {
    /// Close the gate and return the generation that must be handed back to
    /// `end` to reopen it.
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.paused = true;
        self.generation
    }

    /// Reopen the gate if `generation` is still the latest transition.
    /// Returns whether the gate was actually released.
    fn end(&mut self, generation: u64) -> bool {
        if self.paused && generation == self.generation {
            self.paused = false;
            true
        } else {
            false
        }
    }

    /// Force the gate open (e.g. VLAN `force_reconnect`), invalidating any
    /// outstanding transition generation.
    fn release(&mut self) {
        self.generation += 1;
        self.paused = false;
    }

    fn is_paused(&self) -> bool {
        self.paused
    }
}

/// Cloud manager handling cloud connections and device registration
pub struct CloudManager {
    state: AtomicU8,
    reconnect_gate: std::sync::Mutex<ReconnectGate>,
}

impl Default for CloudManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudManager {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(CloudConnectionState::NotConfigured as u8),
            reconnect_gate: std::sync::Mutex::new(ReconnectGate::default()),
        }
    }

    fn lock_reconnect_gate(&self) -> std::sync::MutexGuard<'_, ReconnectGate> {
        self.reconnect_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reconnects_paused(&self) -> bool {
        self.lock_reconnect_gate().is_paused()
    }

    /// Suspend cloud reconnects and tear down the current WS before a network
    /// transition rewrites the interface configuration. Returns the
    /// generation token that must be passed to
    /// [`Self::end_network_transition`] once the new configuration is live.
    pub async fn begin_network_transition(&self) -> u64 {
        let generation = self.lock_reconnect_gate().begin();
        info!(generation, "Cloud reconnects suspended for network transition");
        self.reconnect_after_network_change().await;
        generation
    }

    /// Resume cloud reconnects after a network transition. A stale generation
    /// (a newer transition has begun meanwhile) is ignored so it cannot
    /// reopen the gate early.
    pub async fn end_network_transition(&self, generation: u64) {
        if self.lock_reconnect_gate().end(generation) {
            info!(generation, "Cloud reconnects resumed after network transition");
            if self.get_state() != CloudConnectionState::NotConfigured {
                self.set_state(CloudConnectionState::Disconnected);
            }
        } else {
            debug!(
                generation,
                "Ignoring stale end of network transition for cloud reconnect"
            );
        }
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

    /// Register device with cloud.
    ///
    /// De-providerized (doc auth-analysis/05 §2): no IdP verification — the
    /// TLS response of the token exchange IS the cloud's authorization
    /// assertion. The device stores the cloud-assigned owner user id
    /// (replaces google_identity); provider identity never reaches the device.
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

        // Exchange temporary token for permanent auth token. Cloud TLS response
        // is the authorization assertion; firmware does not verify IdP tokens.
        let token_resp = self.exchange_temp_token(&req.token, &cloud_api).await?;
        info!("Token exchange successful");

        // 2. Update configuration
        if cfg.cloud_url.is_empty() {
            config_manager.set_cloud_config(Some(cloud_api), Some(token_resp.secret_token)).await?;
        } else {
            config_manager
                .set_cloud_config(Some(cfg.cloud_url), Some(token_resp.secret_token))
                .await?;
        }

        // 3. Record the cloud-assigned owner (replaces google_identity)
        config_manager
            .update(|config| {
                config.owner_user_id = token_resp.user_id;
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
                    config.owner_user_id = None;
                })
                .await?;

            info!("Device deregistered, disconnecting from cloud");
            self.set_state(CloudConnectionState::NotConfigured);
            self.disconnect_from_cloud().await;
            Ok(())
        } else {
            Err(anyhow!("Deregister request failed with status: {}", response.status()))
        }
    }

    pub async fn get_ota_info(&self, os_version: &str, app_version: &str) -> Result<OtaInfo> {
        let config_manager = get_config_manager();
        let config = config_manager.get().await;

        // if config.cloud_url.is_empty() {
        //     return Err(anyhow!("Cloud token or URL is not set"));
        // }

        let device_id_enc = encrypt_device_id_rsa(&config.device_id)?;
        let response = match Client::new()
            .get(format!(
                "{}/releases?prerelease={}&deviceId={}&appVersion={}&systemVersion={}",
                CLOUD_API_URL,
                config.dev_channel_enabled,
                device_id_enc,
                app_version,
                os_version,
            ))
            .timeout(super::CLOUD_API_REQUEST_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
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

        // first request to get total size
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
            // resume download with Range request
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
                    // log once per MB
                    info!("Download progress: {}% ({}/{})", progress, downloaded, total_size);
                    jsonrpc::broadcast_ota_state(OtaState::system_download(
                        progress,
                        by_user.load(Ordering::Relaxed),
                    ))
                    .await;
                }
            }

            // retry: continue outer loop
            if should_retry {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            // download finished; verify size
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
                    if self.reconnects_paused() {
                        // A network transition is rewriting the interface
                        // config; reconnecting now would bind the new socket
                        // to the address that is about to disappear.
                        debug!("Cloud connection loop: reconnects suspended for network transition");
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        continue;
                    }
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

        // let url = response.url().to_string();
        // let msg = response.text().await;
        // info!("Token exchange url: {:?}, msg: {:?}", url, msg);
        // Ok(TokenExchangeResponse { secret_token: "".to_owned() })

        let token_resp: TokenExchangeResponse = response.json().await?;
        Ok(token_resp)
    }

    /// Disconnect cloud WebSocket before/after network restart so the connection loop can reconnect.
    pub async fn reconnect_after_network_change(&self) {
        let state = self.get_state();
        if matches!(state, CloudConnectionState::Connected | CloudConnectionState::Connecting) {
            info!("Network restarting, disconnecting cloud WebSocket for reconnect");
            self.disconnect_from_cloud().await;
            self.set_state(CloudConnectionState::Disconnected);
        }
    }

    /// Always tear down WS (if any) and mark Disconnected so the connection loop retries.
    /// Used after VLAN apply when WAN may only become ready later (no reboot required).
    pub async fn force_reconnect(&self) {
        // An explicit force-reconnect overrides any in-flight network
        // transition gate; its generation is invalidated so the transition's
        // later `end_network_transition` cannot reopen (or re-close) anything.
        self.lock_reconnect_gate().release();
        if matches!(self.get_state(), CloudConnectionState::NotConfigured) {
            return;
        }
        info!("Forcing cloud reconnect (was {:?})", self.get_state());
        self.disconnect_from_cloud().await;
        self.set_state(CloudConnectionState::Disconnected);
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

        // info!("Connecting to cloud WebSocket: {}", config.cloud_url);
        let mut client = CloudWebSocketClient::new(config.cloud_url, token, device_id);

        // Only `Connecting` here: `Connected` is set by the WS client once
        // the handshake has actually completed, so the internal state never
        // claims a live cloud connection before one exists.
        self.set_state(CloudConnectionState::Connecting);

        let result = match client.connect().await {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(io_err) = e.downcast_ref::<std::io::Error>()
                    && io_err.kind() == ErrorKind::UnexpectedEof
                {
                    warn!(
                        "WebSocket connection closed by peer without close_notify (UnexpectedEof) e: {:?}",
                        &io_err
                    );
                    Ok(())
                } else {
                    Err(e)
                }
            }
        };

        self.set_state(CloudConnectionState::Disconnected);
        result
    }

    async fn disconnect_from_cloud(&self) {
        if let Err(e) = websocket::cloud_ws_close().await {
            error!("Failed to disconnect from cloud: {}", e);
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

#[cfg(test)]
mod reconnect_gate_tests {
    use super::ReconnectGate;

    #[test]
    fn begin_pauses_and_end_resumes() {
        let mut gate = ReconnectGate::default();
        assert!(!gate.is_paused());

        let generation = gate.begin();
        assert!(gate.is_paused());

        assert!(gate.end(generation));
        assert!(!gate.is_paused());
    }

    #[test]
    fn stale_generation_cannot_reopen_gate() {
        let mut gate = ReconnectGate::default();
        let first = gate.begin();
        let second = gate.begin();
        assert_ne!(first, second);

        // The first transition was superseded; its end must not reopen.
        assert!(!gate.end(first));
        assert!(gate.is_paused());

        assert!(gate.end(second));
        assert!(!gate.is_paused());
    }

    #[test]
    fn release_forces_gate_open_and_invalidates_generation() {
        let mut gate = ReconnectGate::default();
        let generation = gate.begin();
        assert!(gate.is_paused());

        gate.release();
        assert!(!gate.is_paused());

        // The invalidated generation can no longer affect the gate.
        assert!(!gate.end(generation));
        assert!(!gate.is_paused());
    }

    #[test]
    fn end_without_begin_is_noop() {
        let mut gate = ReconnectGate::default();
        assert!(!gate.end(42));
        assert!(!gate.is_paused());
    }
}
