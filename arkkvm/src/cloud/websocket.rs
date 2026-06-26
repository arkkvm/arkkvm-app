use std::io::ErrorKind;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error, Message};
use tracing::{debug, error, info, warn};

use super::oidc::OidcAuthenticator;
use super::types::WebRTCSessionRequest;
use super::WEBSOCKET_PING_INTERVAL;
use crate::config::get_config_manager;
use crate::{common, ota};

// Global write channel for cloud WS
static CLOUD_WS_TX: tokio::sync::OnceCell<
    RwLock<Option<tokio::sync::mpsc::UnboundedSender<Message>>>,
> = tokio::sync::OnceCell::const_new();

pub async fn cloud_ws_set_tx(tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>) {
    let cell = CLOUD_WS_TX.get_or_init(|| async { RwLock::new(None) }).await;
    *cell.write().await = tx;
}

pub async fn cloud_ws_send_json(value: &serde_json::Value) -> anyhow::Result<()> {
    let cell = CLOUD_WS_TX.get_or_init(|| async { RwLock::new(None) }).await;
    if let Some(tx) = cell.read().await.as_ref() {
        tx.send(Message::Text(value.to_string().as_str().into()))?;
        Ok(())
    } else {
        anyhow::bail!("cloud ws tx not available")
    }
}

pub async fn cloud_ws_close() -> Result<()> {
    let cell = CLOUD_WS_TX.get_or_init(|| async { RwLock::new(None) }).await;
    if let Some(tx) = cell.read().await.as_ref() {
        tx.send(Message::Close(None))?;
    }
    Ok(())
}

/// Cloud WebSocket client for handling cloud connections
pub struct CloudWebSocketClient {
    url: String,
    token: String,
    device_id: String,
    write_tx: Option<tokio::sync::mpsc::UnboundedSender<Message>>,
}

impl CloudWebSocketClient {
    pub fn new(url: String, token: String, device_id: String) -> Self {
        Self { url, token, device_id, write_tx: None }
    }

    /// Connect to cloud WebSocket and handle messages
    pub async fn connect(&mut self) -> Result<()> {
        let ws_url = self.url.replace("https://", "wss://").replace("http://", "ws://");
        info!("Connecting to cloud WebSocket: {}", ws_url);

        // Build handshake request with headers
        let mut req = ws_url.clone().into_client_request()?;
        {
            let current_version = ota::get_current_version(false).await?;
            let app_version = current_version.app_version.clone();
            let web_version = current_version.web_version.clone();

            let headers = req.headers_mut();
            headers.insert("X-Device-ID", HeaderValue::from_str(&self.device_id)?);
            headers.insert("X-App-Version", HeaderValue::from_str(app_version.as_str())?);
            headers
                .insert("Authorization", HeaderValue::from_str(&format!("Bearer {}", self.token))?);
            headers.insert("x-web-version", HeaderValue::from_str(web_version.as_str())?);
            headers.insert("X-ignore-ping", HeaderValue::from_str("true")?);
        }

        // Connect with request (no initial auth message)
        let (ws_stream, _) = connect_async(req).await?;
        let (mut write, mut read) = ws_stream.split();
        info!("Cloud WebSocket connection established");

        // Create channel for sending messages
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        self.write_tx = Some(write_tx.clone());
        cloud_ws_set_tx(Some(write_tx.clone())).await;

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        // Spawn task to handle outgoing messages
        let shutdown_tx_write = shutdown_tx.clone();
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                let is_close = matches!(&msg, Message::Close(_));
                if is_close {
                    info!("WebSocket connection closed by device");
                }
                if let Err(e) = write.send(msg).await {
                    error!("Failed to send message to cloud: {}", e);
                    let _ = shutdown_tx_write.send(true);
                    break;
                }
                if is_close {
                    break;
                }
            }
        });

        let shutdown_tx_ping = shutdown_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WEBSOCKET_PING_INTERVAL).await;
                if write_tx.send(Message::Ping(bytes::Bytes::new())).is_err() {
                    warn!("Failed to send ping to cloud: channel closed");
                    let _ = shutdown_tx_ping.send(true);
                    break;
                }
            }
        });

        // Handle incoming messages
        info!("Cloud WebSocket Starting to handle incoming messages");
        loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow_and_update() {
                        info!("WebSocket write path failed, closing read loop");
                        break;
                    }
                }
                msg = read.next() => {
                    match msg {
                        None => break,
                        Some(Ok(msg)) => match msg {
                            Message::Text(text) => {
                                if let Err(e) = self.handle_message(&text).await {
                                    error!("Failed to handle message: {}", e);
                                }
                            }
                            Message::Close(_) => {
                                info!("WebSocket connection closed by server");
                                break;
                            }
                            Message::Ping(data) => {
                                if let Some(tx) = &self.write_tx {
                                    let _ = tx.send(Message::Pong(data));
                                }
                            }
                            _ => {}
                        },
                        Some(Err(e)) => {
                            match e {
                                Error::Io(io_err) if io_err.kind() == ErrorKind::UnexpectedEof => {
                                    info!(
                                        "WebSocket connection closed by peer without close_notify (UnexpectedEof)"
                                    );
                                    write_task.abort();
                                    cloud_ws_set_tx(None).await;
                                    return Err(io_err.into());
                                }
                                Error::ConnectionClosed => {
                                    info!("WebSocket connection closed by peer");
                                    break;
                                }
                                _ => {
                                    error!("WebSocket error: {}", e);
                                    write_task.abort();
                                    cloud_ws_set_tx(None).await;
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                }
            }
        }
        info!("Cloud WebSocket connection closed");

        // Cancel the write task
        write_task.abort();
        cloud_ws_set_tx(None).await;
        Ok(())
    }

    /// Handle incoming WebSocket messages
    async fn handle_message(&self, message: &str) -> Result<()> {
        let parsed: Value = serde_json::from_str(message).map_err(|e| {
            error!("Failed to parse WebSocket message: {}, message: {}", e, message);
            anyhow::anyhow!("Invalid JSON message: {}", e)
        })?;

        if let Some(msg_type) = parsed.get("type").and_then(|v| v.as_str()) {
            match msg_type {
                "ping" => {
                    debug!("Received ping from cloud");
                    self.send_pong().await?;
                }
                "offer" => {
                    info!("Received offer from cloud");
                    self.handle_session_request(&parsed).await?;
                }
                "new-ice-candidate" => {
                    if let Some(data) = parsed.get("data") {
                        if let Some(session) = crate::web::get_global_app_state().get_current_session().await {
                            let candidate_json = serde_json::to_string(data)?;
                            if let Err(e) = session.add_ice_candidate(&candidate_json).await {
                                warn!("Failed to add ICE candidate: {}", e);
                            }
                        } else {
                            warn!("No current session for ICE candidate");
                        }
                    }
                }
                "session_request" => {
                    info!("Received session request from cloud");
                    self.handle_session_request(&parsed).await?;
                }
                "session_close" => {
                    info!("Received session close request from cloud");
                    self.handle_session_close(&parsed).await?;
                }
                "error" => {
                    error!("Received error from cloud: {}", parsed);
                    self.handle_cloud_error(&parsed).await?;
                }
                _ => {
                    warn!("Unknown message type from cloud: {}", msg_type);
                }
            }
        }

        Ok(())
    }

    /// Handle WebRTC session requests from cloud
    async fn handle_session_request(&self, message: &Value) -> Result<()> {
        info!("Handling cloud session request");

        if let Some(data) = message.get("data") {
            let request: WebRTCSessionRequest = serde_json::from_value(data.clone())?;

            // Verify OIDC token and match identity against stored config
            if let Some(oidc_token) = &request.oidc_google {
                let oidc_auth = OidcAuthenticator::new().await?;
                let google_identity = oidc_auth.verify_token_skip_client_id(oidc_token).await?;

                let config_manager = get_config_manager();
                let cfg = config_manager.get().await;
                let expected = cfg
                    .google_identity
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Cloud identity not configured"))?;
                if expected != &google_identity {
                    return Err(anyhow::anyhow!("Google identity mismatch"));
                }
                debug!("OIDC token verified and identity matched for cloud session");
            }

            // Create WebRTC session
            self.create_cloud_webrtc_session(request).await?;
        }

        Ok(())
    }

    /// Create WebRTC session for cloud connection
    async fn create_cloud_webrtc_session(&self, request: WebRTCSessionRequest) -> Result<()> {
        use crate::web::get_global_app_state;
        use crate::webrtc::{SessionConfig, get_webrtc_api};

        info!("Creating cloud WebRTC session");

        // Get global app state
        let app_state = get_global_app_state().clone();

        // Get WebRTC API
        let webrtc_api = get_webrtc_api().await;

        // Create session configuration
        let session_config = SessionConfig {
            ice_servers: Some(request.ice_servers),
            local_ip: request.ip.and_then(|ip| ip.parse().ok()),
            is_cloud: true, // Mark as cloud session
        };

        // Generate session ID
        let session_id = uuid::Uuid::new_v4().to_string();

        // Create new WebRTC session
        let session =
            webrtc_api.new_session(session_config, session_id.clone()).await.map_err(|e| {
                error!("Failed to create cloud WebRTC session: {}", e);
                anyhow::anyhow!("Failed to create WebRTC session: {}", e)
            })?;

        // Exchange SDP offer/answer
        let answer = session.exchange_offer(&request.sd).await.map_err(|e| {
            error!("Failed to exchange SDP offer: {}", e);
            anyhow::anyhow!("Failed to exchange SDP offer: {}", e)
        })?;

        let session_id = session.id.clone();
        crate::webrtc::handle_session_takeover(app_state.clone(), &session_id).await;

        // Send response back to cloud
        self.send_session_response(&answer, &session_id).await?;

        // Add session to app state
        info!("Cloud WebRTC session created successfully with id: {}", &session_id);
        // app_state.add_session(session).await;
        app_state.set_current_session_id(Some(session_id)).await;
        Ok(())
    }

    /// Send session response back to cloud
    async fn send_session_response(&self, answer: &str, _session_id: &str) -> Result<()> {
        let response = json!({
            "type": "answer",
            "data": answer,
        });

        // info!("Sending session response to cloud: {}", response);

        if let Some(tx) = &self.write_tx {
            let message = Message::Text(response.to_string().as_str().into());
            tx.send(message)
                .map_err(|_| anyhow::anyhow!("Failed to send session response to cloud"))?;
            info!("Session response sent to cloud successfully");
        } else {
            return Err(anyhow::anyhow!("WebSocket write channel not available"));
        }

        Ok(())
    }

    /// Send pong response to cloud
    async fn send_pong(&self) -> Result<()> {
        let pong_message = json!({
            "type": "pong",
            "timestamp": chrono::Utc::now().timestamp()
        });

        if let Some(tx) = &self.write_tx {
            let message = Message::Text(pong_message.to_string().as_str().into());
            tx.send(message).map_err(|_| anyhow::anyhow!("Failed to send pong to cloud"))?;
            debug!("Sent pong to cloud");
        }

        Ok(())
    }

    /// Handle session close request from cloud
    async fn handle_session_close(&self, message: &Value) -> Result<()> {
        use crate::web::get_global_app_state;

        if let Some(session_id) =
            message.get("data").and_then(|data| data.get("session_id")).and_then(|id| id.as_str())
        {
            info!("Closing session: {}", session_id);

            // Get global app state and remove session
            let app_state = get_global_app_state();
            if let Some(_session) = app_state.remove_session(session_id).await {
                info!("Session {} closed successfully", session_id);
                // Send confirmation back to cloud
                self.send_session_close_confirmation(session_id).await?;
            } else {
                warn!("Session {} not found for closing", session_id);
            }
        } else {
            warn!("Invalid session close request: missing session_id");
        }

        Ok(())
    }

    /// Send session close confirmation to cloud
    async fn send_session_close_confirmation(&self, session_id: &str) -> Result<()> {
        let response = json!({
            "type": "session_close_confirmation",
            "data": {
                "session_id": session_id,
                "status": "closed"
            }
        });

        if let Some(tx) = &self.write_tx {
            let message = Message::Text(response.to_string().as_str().into());
            tx.send(message)
                .map_err(|_| anyhow::anyhow!("Failed to send session close confirmation"))?;
            info!("Session close confirmation sent to cloud");
        }

        Ok(())
    }

    /// Handle error messages from cloud
    async fn handle_cloud_error(&self, message: &Value) -> Result<()> {
        if let Some(error_data) = message.get("data") {
            let error_msg =
                error_data.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
            let error_code = error_data.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");

            error!("Cloud error [{}]: {}", error_code, error_msg);

            // Handle specific error codes
            match error_code {
                "auth_failed" => {
                    error!("Authentication failed with cloud");
                    // Could trigger re-authentication here
                }
                "session_not_found" => {
                    warn!("Cloud requested non-existent session");
                }
                _ => {
                    warn!("Unhandled cloud error: {}", error_msg);
                }
            }
        }

        Ok(())
    }
}
