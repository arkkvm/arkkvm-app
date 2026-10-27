use futures::{Stream, StreamExt, TryStreamExt};
use salvo::http::cookie::{Cookie, SameSite};
use salvo::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE,
};
use salvo::http::HeaderValue;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use salvo::conn::rustls::RustlsConfig;
use salvo::prelude::*;
use salvo::serve_static::{StaticDir, static_embed};
use salvo::websocket::{Message, WebSocket, WebSocketUpgrade};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::assets::FrontendAssets;
use crate::cloud::types::CloudRegisterRequest;
use crate::config::get_config_manager;
use crate::config::types::UISwitch;
use crate::hardware::usb;
use crate::jsonrpc::handlers;
use crate::middleware::{
    self, auth_middleware, authenticated_role_from_depot, csrf_middleware,
    developer_auth_middleware, public_middleware, role_acl_middleware, security_middleware,
    AUTH_COOKIE_AGENT, AUTH_COOKIE_USER,
};
use crate::module::rtc_request_params::SettingSwitchParams;
use crate::services::ft_file;
use crate::services::local_file::{self, ByteRange};
use crate::session::SessionRole;
use crate::state::{AppState, ClaimRoleResult};
use crate::{
    common, ota,
    tls::{self, TlsMode},
    util, webrtc,
};

static GLOBAL_APP_STATE: OnceLock<Arc<AppState>> = OnceLock::new();
static TLS_CONFIG_TX: OnceLock<mpsc::UnboundedSender<RustlsConfig>> = OnceLock::new();
/// Last leaf PEM pushed to Salvo; used to skip no-op TLS hot reloads.
static LAST_PUSHED_TLS_PEM: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();

fn last_pushed_tls_pem() -> &'static Mutex<Option<(String, String)>> {
    LAST_PUSHED_TLS_PEM.get_or_init(|| Mutex::new(None))
}

/// Stream of TLS configs pushed via `TLS_CONFIG_TX` for Salvo hot reload.
struct RustlsConfigStream {
    rx: mpsc::UnboundedReceiver<RustlsConfig>,
}

impl RustlsConfigStream {
    fn new(rx: mpsc::UnboundedReceiver<RustlsConfig>) -> Self {
        Self { rx }
    }
}

impl Stream for RustlsConfigStream {
    type Item = RustlsConfig;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebRTCSessionRequest {
    sd: String,
    #[serde(default)]
    ice_servers: Option<Vec<String>>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    client_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct WebRTCSessionResponse {
    sd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest {
    password: String,
    #[serde(default)]
    client_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPasswordRequest {
    password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangePasswordRequest {
    old_password: String,
    new_password: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
    local_auth_mode: String,
    #[serde(default)]
    password: Option<String>,
    pub auto_update: Option<bool>,
    // Virtual Device Settings
    pub microphone_emulation: Option<bool>,
    pub camera_emulation: Option<bool>,
    pub file_transfer: Option<bool>,
    pub audio_playback: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct DeviceStatusResponse {
    is_setup: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct LocalDeviceResponse {
    auth_mode: Option<String>,
    device_id: String,
    loopback_only: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CloudStateResponse {
    connected: bool,
    url: Option<String>,
    app_url: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
}

pub async fn init() -> anyhow::Result<JoinHandle<()>> {
    // Eager init so local WebRTC/HTTP handlers never see an empty OnceLock.
    let _app_state = get_global_app_state();

    // let (layer, io) = sio::SocketIo::builder().with_state(app_state.clone()).build_layer();

    // io.ns("/", handle_session_connect);

    let app = Router::new()
        .hoop(security_middleware)
        .push(init_local_file_routes().await?)
        .push(init_ft_file_routes().await?)
        .push(init_protected_routes().await?)
        .push(init_public_routes().await?)
        .push(init_developer_routes().await?)
        .push(init_static_routes().await?);
    // .hoop(layer.compat());

    let doc = OpenApi::new("arkkvm api", "0.0.1").merge_router(&app);

    let app = app
        .unshift(doc.into_router("/api-doc/openapi.json"))
        .unshift(Scalar::new("/api-doc/openapi.json").into_router("/scalar-ui"));
    // .unshift(RapiDoc::new("/api-doc/openapi.json").into_router("/rapidoc-ui"));
    // .unshift(SwaggerUi::new("/api-doc/openapi.json").into_router("/swagger-ui"))
    // .unshift(ReDoc::new("/api-doc/openapi.json").into_router("/redoc-ui"));

    let local_ip = util::local_ip();
    let local_ip_v6 = util::local_ip_v6();

    let ipv6_available = check_ipv6_availability().await;
    let (http_addr, https_addr) = determine_binding_addresses(ipv6_available).await;
    let rustls_config = build_rustls_config(local_ip, local_ip_v6).await?;

    // Build acceptor based on available protocols
    let handle = if let Some((initial_rustls_config, cert_pem, key_pem)) = rustls_config {
        info!("Starting HTTP server on {} and {}", http_addr, https_addr);
        let (tls_tx, tls_rx) = mpsc::unbounded_channel();
        tls_tx
            .send(initial_rustls_config)
            .map_err(|_| anyhow::anyhow!("failed to seed initial TLS config"))?;
        if let Ok(mut last) = last_pushed_tls_pem().lock() {
            *last = Some((cert_pem, key_pem));
        }
        let _ = TLS_CONFIG_TX.set(tls_tx);
        let tls_stream = RustlsConfigStream::new(tls_rx);
        let acceptor = TcpListener::new(https_addr)
            .rustls(tls_stream)
            .join(TcpListener::new(http_addr))
            .bind()
            .await;
        tokio::spawn(Server::new(acceptor).serve(app))
    } else {
        info!("Starting HTTP server on {}", http_addr);
        let acceptor = TcpListener::new(http_addr).bind().await;
        tokio::spawn(Server::new(acceptor).serve(app))
    };
    Ok(handle)
}

/// Reload TLS config after network hostname/domain changes (SelfSigned mode only).
pub async fn reload_tls_config() -> anyhow::Result<()> {
    reload_tls_config_with_fqdn("").await
}

/// Reload TLS config and include an extra FQDN (e.g. mDNS conflict-resolved name).
pub async fn reload_tls_config_with_fqdn(extra_fqdn: &str) -> anyhow::Result<()> {
    let Some(tls_tx) = TLS_CONFIG_TX.get() else {
        return Ok(());
    };

    match tls::get_tls_mode().await {
        TlsMode::Disabled => return Ok(()),
        TlsMode::Custom(..) => {
            warn!("Skipping TLS reload for custom certificate on hostname change");
            return Ok(());
        }
        TlsMode::SelfSigned => {}
    }

    let local_ip = util::local_ip();
    let local_ip_v6 = util::local_ip_v6();
    let extra_fqdns = if extra_fqdn.trim().is_empty() {
        vec![]
    } else {
        vec![extra_fqdn.trim().to_string()]
    };
    let (new_config, cert_pem, key_pem) =
        tls::init_rustls_material(local_ip, local_ip_v6, None, None, &extra_fqdns).await?;

    {
        let mut last = last_pushed_tls_pem()
            .lock()
            .map_err(|_| anyhow::anyhow!("TLS PEM lock poisoned"))?;
        if last.as_ref().is_some_and(|(c, k)| c == &cert_pem && k == &key_pem) {
            info!("TLS reload skipped, leaf unchanged");
            return Ok(());
        }
        *last = Some((cert_pem, key_pem));
    }

    tls_tx
        .send(new_config)
        .map_err(|_| anyhow::anyhow!("failed to push TLS config reload"))?;
    info!("TLS config reloaded for network hostname change");
    Ok(())
}

/// Check if IPv6 is available on the system
async fn check_ipv6_availability() -> bool {
    match tokio::net::TcpListener::bind("[::1]:80").await {
        Ok(listener) => {
            drop(listener); // Close the test listener
            true
        }
        Err(e) => {
            warn!("IPv6 not available ({})", e);
            false
        }
    }
}

/// Determine binding addresses based on loopback-only setting and IPv6 availability
async fn determine_binding_addresses(ipv6_available: bool) -> (&'static str, &'static str) {
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.local_loopback_only {
        if ipv6_available { ("[::1]:80", "[::1]:443") } else { ("127.0.0.1:80", "127.0.0.1:443") }
    } else {
        if ipv6_available { ("[::]:80", "[::]:443") } else { ("0.0.0.0:80", "0.0.0.0:443") }
    }
}

/// Build Rustls configuration based on TLS mode.
/// Returns `(config, cert_pem, key_pem)` so the caller can seed reload dedupe state.
async fn build_rustls_config(
    local_ip: Option<IpAddr>,
    local_ip_v6: Option<IpAddr>,
) -> anyhow::Result<Option<(salvo::conn::rustls::RustlsConfig, String, String)>> {
    let tls_mode = tls::get_tls_mode().await;
    let rustls_config = match tls_mode {
        TlsMode::Disabled => None,
        TlsMode::SelfSigned => {
            Some(tls::init_rustls_material(local_ip, local_ip_v6, None, None, &[]).await?)
        }
        TlsMode::Custom(certificate, private_key) => Some(
            tls::init_rustls_material(
                local_ip,
                local_ip_v6,
                Some(certificate),
                Some(private_key),
                &[],
            )
            .await?,
        ),
    };
    Ok(rustls_config)
}

// Public routes - no authentication required
async fn init_public_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(public_middleware)
        .push(Router::with_path("/auth/login-local").post(handle_login_local))
        .push(Router::with_path("/device/status").get(handle_device_status))
        .push(Router::with_path("/device/setup").post(handle_device_setup))
        .push(Router::with_path("/metrics").get(handle_metrics));

    Ok(router)
}

/// Local file download routes — auth only (no CSRF on GET/HEAD).
async fn init_local_file_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(auth_middleware)
        .hoop(role_acl_middleware)
        .push(Router::with_path("/local/file").get(handle_local_file_get).head(handle_local_file_head));
    Ok(router)
}

/// FT file routes — auth only (no CSRF on GET/HEAD/POST).
async fn init_ft_file_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(auth_middleware)
        .hoop(role_acl_middleware)
        .push(Router::with_path("/ft/file").get(handle_ft_file_get).head(handle_ft_file_head))
        .push(Router::with_path("/ft/upload").post(handle_ft_upload));
    Ok(router)
}

/// Protected routes - authentication required, CSRF required for write operations
async fn init_protected_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(auth_middleware)
        .hoop(csrf_middleware)
        .hoop(role_acl_middleware)
        // WebRTC routes
        .push(Router::with_path("/webrtc/session").post(handle_webrtc_session))
        .push(Router::with_path("/webrtc/signaling/client").get(handle_webrtc_signaling_client))
        // Cloud routes
        .push(Router::with_path("/cloud/register").post(handle_cloud_register))
        .push(Router::with_path("/cloud/state").get(handle_cloud_status))
        // Device routes
        .push(Router::with_path("/device").get(handle_device))
        // Auth routes
        .push(Router::with_path("/auth/logout").post(handle_logout))
        .push(Router::with_path("/auth/password-local").post(create_password_local))
        .push(Router::with_path("/auth/password-local").put(modify_password_local))
        .push(Router::with_path("/auth/local-password").delete(disable_local_password))
        // Storage routes
        .push(Router::with_path("/storage/upload").post(handle_storage_upload));
    Ok(router)
}

/// Developer mode routes - developer authentication required
async fn init_developer_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(developer_auth_middleware)
        .push(Router::with_path("/developer/pprof").get(handle_pprof_index))
        .push(Router::with_path("/developer/pprof/cmdline").get(handle_pprof_cmdline))
        .push(Router::with_path("/developer/pprof/profile").get(handle_pprof_profile))
        .push(Router::with_path("/developer/pprof/symbol").get(handle_pprof_symbol))
        .push(Router::with_path("/developer/pprof/symbol").post(handle_pprof_create_symbol))
        .push(Router::with_path("/developer/pprof/trace").get(handle_pprof_trace))
        .push(Router::with_path("/developer/pprof/allocs").get(handle_pprof_allocs))
        .push(Router::with_path("/developer/pprof/block").get(handle_pprof_block))
        .push(Router::with_path("/developer/pprof/goroutine").get(handle_pprof_goroutine))
        .push(Router::with_path("/developer/pprof/heap").get(handle_pprof_heap))
        .push(Router::with_path("/developer/pprof/mutex").get(handle_pprof_mutex))
        .push(Router::with_path("/developer/pprof/threadcreate").get(handle_pprof_threadcreate));
    Ok(router)
}

/// Initializes static file serving routes
///
/// # Returns
/// * Returns a Router configured to either:
///   - Serve files from a directory specified by ARKKVM_SERVE_DIR env var
///   - Or serve embedded frontend assets via serve_index and serve_static_file handlers
pub async fn init_static_routes() -> anyhow::Result<Router> {
    let mut router = Router::with_path("{*path}");
    let local_serve_dir = "/userdata/arkkvm/frontend";

    router = if let Some(serve_dir) = option_env!("ARKKVM_SERVE_DIR") {
        info!("Serving frontend from: ARKKVM_SERVE_DIR({})", serve_dir);
        router.get(StaticDir::new(serve_dir).fallback("index.html"))
    } else if std::fs::exists(local_serve_dir).unwrap_or(false) {
        info!("Serving frontend from: local directory({})", local_serve_dir);
        router.get(StaticDir::new(local_serve_dir).fallback("index.html"))
    } else {
        info!("Serving frontend from: embedded assets");
        router.get(static_embed::<FrontendAssets>().fallback("index.html"))
    };

    Ok(router)
}

/// This is a summary of the operation
///
/// All lines of the doc comment will be included to operation description.
#[endpoint]
async fn handle_login_local(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;
    
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.local_auth_mode == "noPassword" {
        return Err(StatusError::bad_request().brief("Login is disabled in noPassword mode"));
    }

    let login_req: LoginRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    let role = parse_client_type(login_req.client_type.as_deref())
        .map_err(|e| StatusError::bad_request().brief(e))?;

    if !config_manager.validate_password(&login_req.password).await {
        return Err(StatusError::unauthorized().brief("Invalid password"));
    }

    let auth_token = Uuid::new_v4().to_string();
    let csrf_token = config_manager
        .set_auth_token_for_role(role, Some(auth_token.clone()))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?
        .ok_or_else(|| {
            StatusError::internal_server_error().brief("Failed to save configuration")
        })?;

    let updated_config = config_manager.get().await;
    let saved_ok = match role {
        SessionRole::User => updated_config.local_auth_token == Some(auth_token.clone()),
        SessionRole::Agent => updated_config.local_agent_auth_token == Some(auth_token.clone()),
    };
    if !saved_ok {
        warn!("Auth token not properly saved to configuration");
        return Err(StatusError::internal_server_error().brief("Failed to save configuration"));
    }

    set_token(req, res, role, auth_token.as_str());
    
    // Return CSRF token in header and body so frontend can use it without a separate request
    res.headers_mut()
        .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());

    Ok(Json(ApiResponse { message: "Login successful".to_string(), csrf_token: Some(csrf_token) }))
}

#[endpoint]
async fn handle_logout(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let role = authenticated_role_from_depot(depot).unwrap_or(SessionRole::User);

    config_manager
        .set_auth_token_for_role(role, None)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;

    clear_token(res, role);

    Ok(Json(ApiResponse { message: "Logout successful".to_string(), csrf_token: None }))
}

#[endpoint]
async fn create_password_local(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.hashed_password.is_some() {
        return Err(StatusError::bad_request().brief("Password already set"));
    }

    if config.local_auth_mode != "noPassword" {
        return Err(StatusError::bad_request().brief("Password mode is not enabled"));
    }

    let password_req: SetPasswordRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    if password_req.password.is_empty() {
        return Err(StatusError::bad_request().brief("Invalid request"));
    }

    // Hash password using bcrypt
    let hashed_password = config_manager
        .hash_password(&password_req.password)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to hash password"))?;
    let auth_token = Uuid::new_v4().to_string();

    config_manager
        .set_hashed_password(Some(hashed_password))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save password"))?;
    let csrf_token = config_manager
        .set_auth_token(Some(auth_token.clone()))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?
        .ok_or_else(|| {
            StatusError::internal_server_error().brief("Failed to save configuration")
        })?;
    config_manager
        .set_auth_mode("password".to_string())
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;

    // Set auth cookie
    set_token(req, res, SessionRole::User, auth_token.as_str());

    res.headers_mut()
        .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());
    
    Ok(Json(ApiResponse {
        message: "Password set successfully".to_string(),
        csrf_token: Some(csrf_token),
    }))
}

#[endpoint]
async fn modify_password_local(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.hashed_password.is_none() {
        return Err(StatusError::bad_request().brief("Password is not set"));
    }

    if config.local_auth_mode != "password" {
        return Err(StatusError::bad_request().brief("Password mode is not enabled"));
    }

    let change_req: ChangePasswordRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    if change_req.old_password.is_empty() || change_req.new_password.is_empty() {
        return Err(StatusError::bad_request().brief("Invalid request"));
    }

    if !config_manager.validate_password(&change_req.old_password).await {
        return Err(StatusError::bad_request().brief("Incorrect old password"));
    }

    // Hash new password using bcrypt
    let new_hashed_password = config_manager
        .hash_password(&change_req.new_password)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to hash new password"))?;
    let new_auth_token = Uuid::new_v4().to_string();

    config_manager
        .set_hashed_password(Some(new_hashed_password))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save new password"))?;
    let csrf_token = config_manager
        .set_auth_token(Some(new_auth_token.clone()))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?
        .ok_or_else(|| {
            StatusError::internal_server_error().brief("Failed to save configuration")
        })?;

    // Set new auth cookie
    set_token(req, res, SessionRole::User, new_auth_token.as_str());

    res.headers_mut()
        .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());
    
    Ok(Json(ApiResponse {
        message: "Password updated successfully".to_string(),
        csrf_token: Some(csrf_token),
    }))
}

#[endpoint]
async fn disable_local_password(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.hashed_password.is_none() {
        return Err(StatusError::bad_request().brief("Password is not set"));
    }

    if config.local_auth_mode != "password" {
        return Err(StatusError::bad_request().brief("Password mode is not enabled"));
    }

    let login_req: LoginRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    if login_req.password.is_empty() {
        return Err(StatusError::bad_request().brief("Invalid request"));
    }

    if !config_manager.validate_password(&login_req.password).await {
        return Err(StatusError::bad_request().brief("Incorrect password"));
    }

    config_manager
        .set_hashed_password(None)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;
    config_manager
        .clear_all_auth_tokens()
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;
    config_manager
        .set_auth_mode("noPassword".to_string())
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;

    // Clear both role cookies
    clear_token(res, SessionRole::User);
    clear_token(res, SessionRole::Agent);

    Ok(Json(ApiResponse {
        message: "Password disabled successfully".to_string(),
        csrf_token: None,
    }))
}

#[endpoint]
async fn handle_device(req: &mut Request) -> Result<Json<LocalDeviceResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    Ok(Json(LocalDeviceResponse {
        auth_mode: Some(config.local_auth_mode),
        device_id: config.device_id,
        loopback_only: config.local_loopback_only,
    }))
}

#[endpoint]
async fn handle_device_status(req: &mut Request) -> Result<Json<DeviceStatusResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let is_setup = config_manager.is_setup().await;

    Ok(Json(DeviceStatusResponse { is_setup }))
}

/// Validates Origin/Referer for CSRF: only allow same-origin or trusted origin.
/// Returns Ok(()) if allowed, Err(StatusError) with 403 if rejected.
/// Compatible with HTTP/1.1 (Host header) and HTTP/2 (uri.authority from :authority pseudo-header).
fn validate_setup_origin_referer(req: &Request) -> Result<(), StatusError> {
    let request_host = req
        .headers()
        .get("Host")
        .and_then(|v| v.to_str().ok())
        .map(|s| extract_host_authority(s).to_lowercase())
        .or_else(|| {
            req.uri()
                .authority()
                .map(|a| extract_host_authority(a.as_str()).to_lowercase())
        });

    let request_host = match request_host {
        Some(h) if !h.is_empty() => h,
        _ => {
            warn!("Device setup: missing Host header, rejecting");
            return Err(StatusError::forbidden().brief("Missing Host"));
        }
    };

    let request_host_norm = normalize_host_for_compare(&request_host).to_lowercase();

    let origin_host = req
        .headers()
        .get("Origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| extract_host_authority(s).to_lowercase());

    let referer_host = req
        .headers()
        .get("Referer")
        .and_then(|v| v.to_str().ok())
        .map(|s| extract_host_authority(s).to_lowercase());

    // If Origin is present, it must match request host (same-origin). Compare normalized (ignore default port).
    if let Some(ref origin) = origin_host {
        let origin_norm = normalize_host_for_compare(origin).to_lowercase();
        if origin_norm != request_host_norm {
            warn!(
                "Device setup: Origin host mismatch, origin={}, request_host={}",
                origin, request_host
            );
            return Err(StatusError::forbidden().brief("Origin not allowed"));
        }
    }

    // If Referer is present (and no Origin), it must match request host.
    if let Some(ref referer) = referer_host {
        let referer_norm = normalize_host_for_compare(referer).to_lowercase();
        if referer_norm != request_host_norm {
            warn!(
                "Device setup: Referer host mismatch, referer={}, request_host={}",
                referer, request_host
            );
            return Err(StatusError::forbidden().brief("Referer not allowed"));
        }
    }

    // Both missing: allow (same-origin form POST may not send Origin/Referer in some clients).
    Ok(())
}

/// Extracts host (and optional port) from "host:port" or URL (e.g. "https://host:port/path" -> "host:port").
fn extract_host_authority(s: &str) -> String {
    let s = s.trim();
    // Strip scheme: "http://" or "https://" (strip_prefix("://") only matches literal "://" at start, so use find)
    let after_scheme = if let Some(i) = s.find("://") { &s[i + 3..] } else { s };
    // Take authority until first '/'
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}

/// Normalizes authority for comparison: strip default port ":80" or ":443" so that
/// "192.168.0.113:80" matches "192.168.0.113" (Origin often omits default port).
fn normalize_host_for_compare(s: &str) -> String {
    let s = s.trim();
    if let Some((host, port)) = s.rsplit_once(':') {
        if port == "80" || port == "443" {
            return host.to_string();
        }
    }
    s.to_string()
}

#[endpoint]
async fn handle_device_setup(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    info!("Device setup request received");

    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    let config_manager = get_config_manager();
    let config = config_manager.get().await;
    // warn!("Setup request auth_mode: {}, hash {:?}", config.local_auth_mode, config.hashed_password);
    if !config.local_auth_mode.is_empty() || config.hashed_password.is_some() {
        return Err(StatusError::bad_request().brief("Device is already set up"));
    }

    let setup_req: SetupRequest = match req.parse_json().await {
        Ok(setup_req) => setup_req,
        Err(err) => {
            error!("Failed to parse setup request body: {:?}", err);
            return Err(StatusError::bad_request().brief(format!("Invalid request body: {}", err)));
        }
    };

    // warn!("Setup request received: {:?}", &setup_req);

    if setup_req.local_auth_mode != "password" && setup_req.local_auth_mode != "noPassword" {
        return Err(StatusError::bad_request().brief("Invalid localAuthMode"));
    }

    if let Err(e) = handlers::set_audio_playback(SettingSwitchParams {
        enabled: setup_req.audio_playback.unwrap_or(false),
    })
    .await
    {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to config audio playback: {:?}", &e)));
    }

    if let Err(e) =
        handlers::set_ui_switch(UISwitch { hdmi_audio: setup_req.audio_playback.unwrap_or(false) })
            .await
    {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to config ui switch: {:?}", &e)));
    }

    if let Err(e) = ota::set_auto_update(setup_req.auto_update.unwrap_or(false)).await {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to config auto update: {:?}", &e)));
    }

    let mic = setup_req.microphone_emulation.unwrap_or(false);
    let mut devices = config_manager.get_usb_devices().await;
    devices.camera = setup_req.camera_emulation.unwrap_or(false);
    // devices.microphone = mic; // Only allow microphone emulation if it's enabled, do not control usb devices state
    devices.mass_storage_ft = setup_req.file_transfer.unwrap_or(false);
    if let Err(e) = config_manager.set_microphone_emulation(mic).await {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to save microphone emulation: {:?}", &e)));
    }
    if let Err(e) = usb::reboot_usb_manager(None, Some(devices)).await {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to config usb devices: {:?}", &e)));
    }
    if mic {
        if let Err(e) = crate::services::init_virtual_mic_service().await {
            return Err(StatusError::internal_server_error()
                .brief(format!("Failed to start virtual mic: {:?}", &e)));
        }
    }

    config_manager
        .set_auth_mode(setup_req.local_auth_mode.clone())
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save config"))?;

    if setup_req.local_auth_mode == "password" {
        let password = setup_req.password.ok_or_else(|| {
            StatusError::bad_request().brief("Password is required for password mode")
        })?;

        if password.is_empty() {
            return Err(StatusError::bad_request().brief("Password is required for password mode"));
        }

        // Hash password using bcrypt
        let hashed_password = config_manager
            .hash_password(&password)
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to hash password"))?;
        let auth_token = Uuid::new_v4().to_string();

        config_manager
            .set_hashed_password(Some(hashed_password))
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to save password"))?;
        let csrf_token = config_manager
            .set_auth_token(Some(auth_token.clone()))
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to save config"))?
            .ok_or_else(|| StatusError::internal_server_error().brief("Failed to save config"))?;

        // Set auth cookie
        set_token(req, res, SessionRole::User, auth_token.as_str());
        
        res.headers_mut()
            .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());
    } else {
        config_manager
            .set_hashed_password(None)
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to save config"))?;
        config_manager
            .clear_all_auth_tokens()
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to save config"))?;
    }

    Ok(Json(ApiResponse {
        message: "Device setup completed successfully".to_string(),
        csrf_token: None,
    }))
}

#[endpoint]
async fn handle_cloud_register(req: &mut Request) -> Result<Json<ApiResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    // let _app_state = GLOBAL_APP_STATE.get().ok_or_else(StatusError::internal_server_error)?;
    let cloud_manager = crate::cloud::manager::get_cloud_manager();

    let register_req: CloudRegisterRequest = match req.parse_json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid register JSON: {}", e);
            let err = StatusError::bad_request();
            return Err(err.brief("Invalid request body"));
        }
    };

    match cloud_manager.register_device(register_req).await {
        Ok(_) => Ok(Json(ApiResponse {
            message: "Cloud registration successful".to_string(),
            csrf_token: None,
        })),
        Err(e) => {
            tracing::error!("Cloud registration failed: {}", e);
            Err(StatusError::bad_request().brief(format!("Cloud registration failed: {}", e)))
        }
    }
}

#[endpoint]
async fn handle_cloud_status(req: &mut Request) -> Result<Json<CloudStateResponse>, StatusError> {
    #[cfg(not(feature = "env_unsafe"))]
    validate_setup_origin_referer(req)?;

    // let _app_state = GLOBAL_APP_STATE.get().ok_or_else(StatusError::internal_server_error)?;
    let cloud_manager = crate::cloud::CloudManager::new();

    let cloud_state = cloud_manager.get_cloud_state().await;

    Ok(Json(CloudStateResponse {
        connected: cloud_state.connected,
        url: cloud_state.url,
        app_url: cloud_state.app_url,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FtFileQuery {
    path: String,
    name: String,
}

async fn serve_ft_file(
    req: &mut Request,
    res: &mut Response,
    head_only: bool,
) -> Result<(), StatusError> {
    let query = req
        .parse_queries::<FtFileQuery>()
        .map_err(|_| StatusError::bad_request().brief("invalid query parameters"))?;

    let info = ft_file::get_ft_file_info(&query.path, &query.name)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not mounted") {
                StatusError::service_unavailable().brief("file transfer filesystem not mounted")
            } else {
                StatusError::not_found().brief("file not found")
            }
        })?;

    let has_range = req.headers().get(RANGE).is_some();
    let byte_range = if let Some(range_hdr) =
        req.headers().get(RANGE).and_then(|v| v.to_str().ok())
    {
        match local_file::parse_range_header(range_hdr, info.size) {
            Ok(Some(r)) => r,
            Ok(None) => ByteRange::full_file(info.size),
            Err(_) => {
                res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", info.size)) {
                    res.headers_mut().insert(CONTENT_RANGE, v);
                }
                return Ok(());
            }
        }
    } else {
        ByteRange::full_file(info.size)
    };

    let resolved = ft_file::resolve_ft_file_path(&query.path, &query.name)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not mounted") {
                StatusError::service_unavailable().brief("file transfer filesystem not mounted")
            } else {
                StatusError::not_found().brief("file not found")
            }
        })?;

    let range_len = byte_range.content_len(info.size);
    let is_partial = has_range && info.size > 0;
    res.status_code(if is_partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK });

    res.headers_mut().insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    res.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    let disposition = format!("attachment; filename=\"{}\"", info.name.replace('"', "_"));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        res.headers_mut().insert(CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&range_len.to_string()) {
        res.headers_mut().insert(CONTENT_LENGTH, v);
    }
    if is_partial {
        let content_range = format!("bytes {}-{}/{}", byte_range.start, byte_range.end, info.size);
        if let Ok(v) = HeaderValue::from_str(&content_range) {
            res.headers_mut().insert(CONTENT_RANGE, v);
        }
    }

    if head_only {
        return Ok(());
    }

    let mut file = tokio::fs::File::open(&resolved)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("failed to open file"))?;
    if byte_range.start > 0 {
        file.seek(std::io::SeekFrom::Start(byte_range.start))
            .await
            .map_err(|_| StatusError::internal_server_error().brief("failed to seek file"))?;
    }

    let limited = file.take(range_len);
    res.stream(ReaderStream::new(limited));
    Ok(())
}

async fn serve_local_file(
    req: &mut Request,
    res: &mut Response,
    head_only: bool,
) -> Result<(), StatusError> {
    let path = req.query::<String>("path").unwrap_or_default();
    if path.trim().is_empty() {
        return Err(StatusError::bad_request().brief("missing path"));
    }

    let info = local_file::get_local_file_info(&path)
        .await
        .map_err(|_| StatusError::not_found().brief("file not found"))?;

    let has_range = req.headers().get(RANGE).is_some();
    let byte_range = if let Some(range_hdr) =
        req.headers().get(RANGE).and_then(|v| v.to_str().ok())
    {
        match local_file::parse_range_header(range_hdr, info.size) {
            Ok(Some(r)) => r,
            Ok(None) => ByteRange::full_file(info.size),
            Err(_) => {
                res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                if let Ok(v) =
                    HeaderValue::from_str(&format!("bytes */{}", info.size))
                {
                    res.headers_mut().insert(CONTENT_RANGE, v);
                }
                return Ok(());
            }
        }
    } else {
        ByteRange::full_file(info.size)
    };

    let resolved = local_file::resolve_local_file_path(&path)
        .await
        .map_err(|_| StatusError::not_found().brief("file not found"))?;

    let range_len = byte_range.content_len(info.size);
    let is_partial = has_range && info.size > 0;

    res.status_code(if is_partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    });

    res.headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    let disposition = format!("attachment; filename=\"{}\"", info.name.replace('"', "_"));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        res.headers_mut().insert(CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&range_len.to_string()) {
        res.headers_mut().insert(CONTENT_LENGTH, v);
    }
    if is_partial {
        let content_range =
            format!("bytes {}-{}/{}", byte_range.start, byte_range.end, info.size);
        if let Ok(v) = HeaderValue::from_str(&content_range) {
            res.headers_mut().insert(CONTENT_RANGE, v);
        }
    }

    if head_only {
        return Ok(());
    }

    let mut file = tokio::fs::File::open(&resolved)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("failed to open file"))?;
    if byte_range.start > 0 {
        file.seek(std::io::SeekFrom::Start(byte_range.start))
            .await
            .map_err(|_| StatusError::internal_server_error().brief("failed to seek file"))?;
    }

    let limited = file.take(range_len);
    res.stream(ReaderStream::new(limited));
    Ok(())
}

#[handler]
async fn handle_local_file_get(req: &mut Request, res: &mut Response) -> Result<(), StatusError> {
    serve_local_file(req, res, false).await
}

#[handler]
async fn handle_local_file_head(req: &mut Request, res: &mut Response) -> Result<(), StatusError> {
    serve_local_file(req, res, true).await
}

#[handler]
async fn handle_ft_file_get(req: &mut Request, res: &mut Response) -> Result<(), StatusError> {
    serve_ft_file(req, res, false).await
}

#[handler]
async fn handle_ft_file_head(req: &mut Request, res: &mut Response) -> Result<(), StatusError> {
    serve_ft_file(req, res, true).await
}

#[endpoint]
async fn handle_ft_upload(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    let upload_id = req.query::<String>("uploadId").unwrap_or_default();
    if upload_id.is_empty() || !upload_id.starts_with("ft_upload_") {
        return Err(StatusError::not_found().brief("Upload not found"));
    }

    let (size, already_uploaded) =
        match crate::hardware::usb::storage::get_ft_upload_progress(&upload_id).await {
            Ok((size, already)) => (size, already),
            Err(_) => return Err(StatusError::not_found().brief("Upload not found")),
        };

    let mut total_bytes_written = already_uploaded;
    let mut stream = req.take_body().into_stream();

    while let Some(item) = stream.next().await {
        let frame = match item {
            Ok(f) => f,
            Err(e) => {
                warn!("failed to read request body {}: {}", upload_id, e);
                if let Err(fin_err) =
                    crate::hardware::usb::storage::complete_ft_upload(&upload_id).await
                {
                    warn!("failed to finalize ft upload after read error {}: {}", upload_id, fin_err);
                }
                return Err(StatusError::internal_server_error().brief("Failed to read upload data"));
            }
        };

        match frame.into_data() {
            Ok(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                if let Err(e) =
                    crate::hardware::usb::storage::append_ft_upload_data(&upload_id, bytes.as_ref())
                        .await
                {
                    warn!("failed to write ft upload chunk {}: {}", upload_id, e);
                    if let Err(fin_err) =
                        crate::hardware::usb::storage::complete_ft_upload(&upload_id).await
                    {
                        warn!(
                            "failed to finalize ft upload after write error {}: {}",
                            upload_id, fin_err
                        );
                    }
                    return Err(StatusError::internal_server_error().brief("Failed to write upload data"));
                }
                total_bytes_written += bytes.len() as i64;
            }
            Err(_) => continue,
        }
    }

    if let Err(e) = crate::hardware::usb::storage::complete_ft_upload(&upload_id).await {
        warn!("failed to finalize ft upload {}: {}", upload_id, e);
        return Err(StatusError::internal_server_error().brief("Failed to finalize upload"));
    }

    if total_bytes_written < size {
        res.status_code(StatusCode::ACCEPTED);
        return Ok(Json(ApiResponse {
            message: format!("Upload partial: {total_bytes_written}/{size}"),
            csrf_token: None,
        }));
    }

    Ok(Json(ApiResponse { message: "Upload completed".to_string(), csrf_token: None }))
}

#[endpoint]
async fn handle_storage_upload(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
    // Query param: uploadId
    let upload_id = req.query::<String>("uploadId").unwrap_or_default();
    if upload_id.is_empty() {
        return Err(StatusError::not_found().brief("Upload not found"));
    }

    // Validate upload exists
    if crate::hardware::usb::storage::get_upload_progress(&upload_id).await.is_err() {
        return Err(StatusError::not_found().brief("Upload not found"));
    }

    let (size, already_uploaded) =
        match crate::hardware::usb::storage::get_upload_progress(&upload_id).await {
            Ok((size, already)) => (size, already),
            Err(_) => return Err(StatusError::not_found().brief("Upload not found")),
        };

    // Durable progress comes only from append/complete — never from request body length.
    let already_before = already_uploaded;
    let mut already = already_uploaded;
    let mut body_frames: u64 = 0;
    let mut body_bytes: u64 = 0;
    let mut stream = req.take_body().into_stream();

    while let Some(item) = stream.next().await {
        let frame = match item {
            Ok(f) => f,
            Err(e) => {
                warn!("failed to read request body {}: {}", upload_id, e);
                if let Err(fin_err) =
                    crate::hardware::usb::storage::complete_upload(&upload_id).await
                {
                    warn!("failed to finalize upload after read error {}: {}", upload_id, fin_err);
                }
                return Err(
                    StatusError::internal_server_error().brief("Failed to read upload data")
                );
            }
        };

        // Only accept DATA frames
        match frame.into_data() {
            Ok(bytes) => {
                if bytes.is_empty() {
                    // empty chunk is not EOF; continue
                    continue;
                }
                body_frames += 1;
                body_bytes += bytes.len() as u64;
                match crate::hardware::usb::storage::append_upload_data(
                    &upload_id,
                    bytes.as_ref(),
                )
                .await
                {
                    Ok((_size, next_already)) => {
                        already = next_already;
                    }
                    Err(e) => {
                        warn!("failed to write upload chunk {}: {}", upload_id, e);
                        if let Err(fin_err) =
                            crate::hardware::usb::storage::complete_upload(&upload_id).await
                        {
                            warn!(
                                "failed to finalize upload after write error {}: {}",
                                upload_id, fin_err
                            );
                        }
                        return Err(
                            StatusError::internal_server_error().brief("Failed to write upload data")
                        );
                    }
                }
            }
            Err(_non_data_frame) => {
                // Ignore non-DATA frames (e.g., trailers)
                continue;
            }
        }
    }

    let already_after_append = already;

    // Finalize: status uses durable on-disk length from complete_upload.
    match crate::hardware::usb::storage::complete_upload(&upload_id).await {
        Ok(durable) => {
            info!(
                "upload http {}: already_before={} body_frames={} body_bytes={} already_after_append={} durable={} size={}",
                upload_id,
                already_before,
                body_frames,
                body_bytes,
                already_after_append,
                durable,
                size
            );
            already = durable;
            if already == size {
                info!("Upload {} completed successfully: {}/{}", upload_id, already, size);
                Ok(Json(ApiResponse { message: "Upload completed".to_string(), csrf_token: None }))
            } else {
                warn!(
                    "Upload {} ended before complete file received: {}/{}",
                    upload_id, already, size
                );
                res.status_code(StatusCode::ACCEPTED);
                Ok(Json(ApiResponse {
                    message: format!("Upload partial: {already}/{size}"),
                    csrf_token: None,
                }))
            }
        }
        Err(e) => {
            warn!(
                "failed to finalize upload {}: {} (already_before={} body_frames={} body_bytes={} already_after_append={} size={})",
                upload_id, e, already_before, body_frames, body_bytes, already_after_append, size
            );
            Err(StatusError::internal_server_error().brief("Failed to finalize upload"))
        }
    }
}

#[endpoint]
async fn handle_metrics() -> &'static str {
    // TODO: Implement Prometheus metrics
    "# ArkKVM metrics not implemented"
}

#[endpoint]
async fn handle_webrtc_session(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<WebRTCSessionResponse>, StatusError> {
    info!("Received WebRTC session request");

    let request: WebRTCSessionRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    let role = parse_client_type(request.client_type.as_deref())
        .map_err(|e| StatusError::bad_request().brief(e))?;

    if let Some(cookie_role) = authenticated_role_from_depot(depot)
        && cookie_role != role
    {
        return Err(StatusError::forbidden().brief("clientType does not match auth cookie role"));
    }

    let app_state = get_global_app_state();

    let webrtc_api = webrtc::get_webrtc_api().await;
    let session_config = webrtc::SessionConfig {
        // LAN: is_cloud=false so new_session ignores ICE lists; still map
        // the wire field so the type stays consistent.
        ice_servers: webrtc::resolve_ice_servers(vec![], request.ice_servers.unwrap_or_default()),
        local_ip: request.ip.and_then(|ip| ip.parse().ok()),
        is_cloud: false,
        role,
        cloud_client_id: None,
    };

    let session =
        webrtc_api.new_session(session_config, Uuid::new_v4().to_string()).await.map_err(|e| {
            warn!("Failed to create WebRTC session: {}", e);
            StatusError::internal_server_error().brief(format!("Failed to create session: {e}"))
        })?;

    let answer = match session.exchange_offer(&request.sd).await {
        Ok(answer) => answer,
        Err(e) => {
            warn!("Failed to exchange offer: {}", e);
            crate::webrtc::remove_and_close_session(&app_state, &session.id).await;
            return Err(
                StatusError::internal_server_error().brief(format!("Failed to exchange offer: {e}"))
            );
        }
    };

    let session_id = session.id.clone();
    match crate::webrtc::claim_and_activate_session(app_state.clone(), session.clone())
        .await
        .map_err(|e| {
            StatusError::internal_server_error()
                .brief(format!("Failed to claim WebRTC session: {e}"))
        })? {
        ClaimRoleResult::Accepted { .. } => {}
        ClaimRoleResult::RejectedOccupied => {
            crate::webrtc::remove_and_close_session(&app_state, &session_id).await;
            return Err(StatusError::conflict().brief("Agent session slot is already occupied"));
        }
        ClaimRoleResult::RejectedMcpDisabled => {
            crate::webrtc::remove_and_close_session(&app_state, &session_id).await;
            return Err(StatusError::forbidden().brief(
                "MCP is disabled on the device; enable MCP before agent control",
            ));
        }
        ClaimRoleResult::NotRegistered => {
            crate::webrtc::remove_and_close_session(&app_state, &session_id).await;
            return Err(StatusError::internal_server_error().brief("Session is no longer registered"));
        }
    }

    info!("WebRTC session created successfully with id: {}", &session_id);

    Ok(Json(WebRTCSessionResponse { sd: answer }))
}

#[endpoint]
async fn handle_webrtc_signaling_client(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), StatusError> {
    let source = req.remote_addr().to_string();
    let connection_id = Uuid::new_v4().to_string();

    // Some(role) = explicit query; None = legacy (offer may omit clientType → user).
    let ws_role = match req.query::<String>("clientType") {
        Some(raw) => {
            let role = SessionRole::parse(&raw).ok_or_else(|| {
                StatusError::bad_request().brief(format!("Invalid clientType: {raw}"))
            })?;
            Some(role)
        }
        None => None,
    };

    // Password mode: cookie role must match effective signaling role (legacy omit ≡ user).
    if let Some(cookie_role) = authenticated_role_from_depot(depot) {
        let effective = ws_role.unwrap_or(SessionRole::User);
        if effective != cookie_role {
            return Err(
                StatusError::forbidden().brief("clientType does not match auth cookie role")
            );
        }
    }

    info!("WebRTC WebSocket connection from {}", source);

    let _ = WebSocketUpgrade::new()
        .upgrade(req, res, move |ws| async move {
            if let Err(e) = handle_webrtc_websocket(ws, connection_id, source, ws_role).await {
                warn!("WebSocket handler error: {}", e);
            }
        })
        .await;

    Ok(())
}

#[endpoint]
async fn handle_pprof_index() -> &'static str {
    // TODO: Implement pprof index page
    "ArkKVM pprof not implemented"
}

#[endpoint]
async fn handle_pprof_cmdline() -> &'static str {
    // TODO: Implement pprof cmdline
    "ArkKVM cmdline not implemented"
}

#[endpoint]
async fn handle_pprof_profile() -> &'static str {
    // TODO: Implement pprof CPU profile
    "ArkKVM profile not implemented"
}

#[endpoint]
async fn handle_pprof_symbol() -> &'static str {
    // TODO: Implement pprof symbol lookup
    "ArkKVM symbol not implemented"
}

#[endpoint]
async fn handle_pprof_create_symbol() -> &'static str {
    // TODO: Implement pprof symbol creation
    "ArkKVM symbol creation not implemented"
}

#[endpoint]
async fn handle_pprof_trace() -> &'static str {
    // TODO: Implement pprof execution trace
    "ArkKVM trace not implemented"
}

#[endpoint]
async fn handle_pprof_allocs() -> &'static str {
    // TODO: Implement pprof memory allocations
    "ArkKVM allocs not implemented"
}

#[endpoint]
async fn handle_pprof_block() -> &'static str {
    // TODO: Implement pprof blocking profile
    "ArkKVM block not implemented"
}

#[endpoint]
async fn handle_pprof_goroutine() -> &'static str {
    // TODO: Implement pprof goroutine profile (async tasks in Rust)
    "ArkKVM goroutine not implemented"
}

#[endpoint]
async fn handle_pprof_heap() -> &'static str {
    // TODO: Implement pprof heap profile
    "ArkKVM heap not implemented"
}

#[endpoint]
async fn handle_pprof_mutex() -> &'static str {
    // TODO: Implement pprof mutex profile
    "ArkKVM mutex not implemented"
}

#[endpoint]
async fn handle_pprof_threadcreate() -> &'static str {
    // TODO: Implement pprof thread creation profile
    "ArkKVM threadcreate not implemented"
}

// async fn handle_session_disconnect(
//     socket: sio::SocketRef,
//     sio::State(state): sio::State<Arc<AppState>>,
// ) {
//     info!("[sid={}] disconnected", socket.id);
//     let session_id = socket.id.to_string();
//     state.remove_session(&session_id).await;
// }

// async fn handle_socket_offer(
//     socket: sio::SocketRef,
//     sio::Data(data): sio::Data<String>,
//     sio::State(_state): sio::State<Arc<AppState>>,
//     ack: sio::AckSender,
// ) {
//     info!("[sid={}] received offer", socket.id);

//     match serde_json::from_str::<WebRTCSessionRequest>(&data) {
//         Ok(request) => {
//             let session_id = socket.id.to_string();

//             // Create session configuration
//             let config = webrtc::SessionConfig {
//                 ice_servers: request.ice_servers,
//                 local_ip: request.ip.and_then(|ip| ip.parse().ok()),
//                 is_cloud: false,
//             };

//             // Get WebRTC API and create session
//             match webrtc::get_webrtc_api().await.new_session(config, session_id.clone()).await {
//                 Ok(session) => {
//                     // Store the session IMMEDIATELY after creation to handle ICE candidates
//                     match session.exchange_offer(&request.sd).await {
//                         Ok(answer) => {
//                             let response = serde_json::json!({
//                                 "type": "answer",
//                                 "data": answer
//                             });
//                             match ack.send(&response.to_string()) {
//                                 Ok(_) => {
//                                     info!("[sid={}] Answer sent successfully via ack", socket.id);
//                                 }
//                                 Err(e) => {
//                                     error!("[sid={}] Failed to send answer via ack: {}", socket.id, e);
//                                 }
//                             }
//                             info!("[sid={}] WebRTC session created successfully", socket.id);
//                         }
//                         Err(e) => {
//                             warn!("[sid={}] Failed to exchange offer: {}", socket.id, e);
//                             let error_response = serde_json::json!({"error": e.to_string()}).to_string();
//                             match ack.send(&error_response) {
//                                 Ok(_) => {
//                                     warn!("[sid={}] Sent error response via ack", socket.id);
//                                 }
//                                 Err(ack_err) => {
//                                     error!("[sid={}] Failed to send error response via ack: {}", socket.id, ack_err);
//                                 }
//                             }
//                         }
//                     }
//                 }
//                 Err(e) => {
//                     warn!("[sid={}] Failed to create WebRTC session: {}", socket.id, e);
//                     let error_response = serde_json::json!({"error": e.to_string()}).to_string();
//                     match ack.send(&error_response) {
//                         Ok(_) => {
//                             warn!("[sid={}] Sent error response via ack", socket.id);
//                         }
//                         Err(ack_err) => {
//                             error!("[sid={}] Failed to send error response via ack: {}", socket.id, ack_err);
//                         }
//                     }
//                 }
//             }
//         }
//         Err(e) => {
//             warn!("[sid={}] Failed to parse offer: {}", socket.id, e);
//             let error_response = serde_json::json!({"error": format!("Invalid offer format: {}", e)}).to_string();
//             match ack.send(&error_response) {
//                 Ok(_) => {
//                     warn!("[sid={}] Sent error response via ack", socket.id);
//                 }
//                 Err(ack_err) => {
//                     error!("[sid={}] Failed to send error response via ack: {}", socket.id, ack_err);
//                 }
//             }
//         }
//     }
// }

// async fn handle_socket_ice_candidate(
//     socket: sio::SocketRef,
//     sio::Data(data): sio::Data<String>,
//     sio::State(state): sio::State<Arc<AppState>>,
// ) {
//     info!("[sid={}] received ICE candidate: {}", socket.id, data);

//     match serde_json::from_str::<serde_json::Value>(&data) {
//         Ok(message) => {
//             let session_id = socket.id.to_string();

//             // Check if this is a new-ice-candidate message type
//             if message.get("type").and_then(|v| v.as_str()) == Some("new-ice-candidate") {
//                 if let Some(candidate_data) = message.get("data") {
//                     // Get the session and add ICE candidate
//                     if let Some(session) = state.get_session_by_id(&session_id).await {
//                         // Convert candidate data to JSON string
//                         let candidate_json = serde_json::to_string(candidate_data)
//                             .map_err(|e| {
//                                 warn!("[sid={}] Failed to serialize candidate: {}", socket.id, e)
//                             })
//                             .unwrap_or_default();

//                         if !candidate_json.is_empty() {
//                             match session.add_ice_candidate(&candidate_json).await {
//                                 Ok(_) => {
//                                     info!("[sid={}] Successfully added remote ICE candidate", socket.id);
//                                 }
//                                 Err(e) => {
//                                     warn!("[sid={}] Failed to add ICE candidate: {}", socket.id, e);
//                                 }
//                             }
//                         } else {
//                             warn!("[sid={}] ICE candidate JSON is empty after serialization", socket.id);
//                         }
//                     } else {
//                         warn!("[sid={}] No session found for ICE candidate", socket.id);
//                     }
//                 } else {
//                     warn!(
//                         "[sid={}] Missing candidate data in new-ice-candidate message",
//                         socket.id
//                     );
//                 }
//             }
//         }
//         Err(e) => {
//             warn!("[sid={}] Failed to parse ICE candidate JSON: {}", socket.id, e);
//         }
//     }
// }

// async fn handle_session_connect(
//     socket: sio::SocketRef,
//     sio::State(state): sio::State<Arc<AppState>>,
// ) {
//     info!("[sid={}] connected", socket.id);
//     let session_id = socket.id.to_string();
//     socket.on_disconnect(handle_session_disconnect);

//     // Store the socket reference in AppState for ICE candidate sending
//     state.sockets.write().await.insert(session_id.clone(), socket.clone());

//     // Create a basic session for Socket.IO connection tracking
//     // let session = Session::new(session_id.clone());
//     // state.add_session(session).await;

//     // Handle WebRTC signaling messages
//     socket.on("offer", handle_socket_offer);
//     socket.on("ice-candidate", handle_socket_ice_candidate);
// }

async fn handle_webrtc_websocket(
    mut ws: WebSocket,
    connection_id: String,
    source: String,
    ws_role: Option<SessionRole>,
) -> anyhow::Result<()> {
    let device_metadata = serde_json::json!({
        "type": "device-metadata",
        "data": {
            "deviceVersion": common::get_app_version(false),
        }
    });

    ws.send(Message::text(device_metadata.to_string())).await?;
    info!("WebRTC WebSocket connection {} established", connection_id);

    let app_state = get_global_app_state();

    while let Some(msg) = ws.recv().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                warn!("WebRTC WebSocket connection {} closed by error: {}", connection_id, e);
                break;
            }
        };

        if msg.is_text() {
            if let Ok(text) = msg.as_str() {
                match handle_webrtc_websocket_message(
                    text,
                    &connection_id,
                    &source,
                    &app_state,
                    &mut ws,
                    ws_role,
                )
                .await
                {
                    Ok(should_close) => {
                        if should_close {
                            info!("[ws={}] Client requested WebSocket close", connection_id);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("[ws={}] Failed to handle WebRTC message: {}", connection_id, e);
                    }
                }
            } else {
                warn!("[ws={}] Failed to convert WebSocket message to string", connection_id);
            }
        } else if msg.is_close() {
            info!("WebRTC WebSocket connection {} closed by peer", connection_id);
            break;
        } else if msg.is_ping() {
            let ping_data = msg.as_bytes().to_vec();
            if ws.send(Message::pong(ping_data)).await.is_err() {
                break;
            }
        }

        // Send queued ICE candidates to client
        let ice_candidates = app_state.get_ice_candidates(&connection_id).await;
        for candidate in ice_candidates {
            if let Err(e) = ws.send(Message::text(candidate)).await {
                warn!("[ws={}] Failed to send queued ICE candidate: {}", connection_id, e);
                break;
            }
        }
    }

    if crate::webrtc::remove_and_close_session(&app_state, &connection_id).await {
        info!("Removed session {} on websocket close", connection_id);
    }

    {
        let mut sockets = app_state.sockets.write().await;
        sockets.remove(&connection_id);
    }
    {
        let mut ice_queue = app_state.websocket_ice_queue.write().await;
        ice_queue.remove(&connection_id);
    }

    Ok(())
}

async fn handle_webrtc_websocket_message(
    message: &str,
    connection_id: &str,
    source: &str,
    app_state: &Arc<AppState>,
    ws: &mut WebSocket,
    ws_role: Option<SessionRole>,
) -> anyhow::Result<bool> {
    if message == "ping" {
        ws.send(Message::text("pong")).await?;
        return Ok(false);
    }

    let parsed: serde_json::Value = match serde_json::from_str(message) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "[ws={}] Failed to parse WebSocket message as JSON: {} (message: {})",
                connection_id, e, message
            );
            return Err(anyhow::anyhow!("Invalid JSON: {}", e));
        }
    };

    let msg_type = parsed.get("type").and_then(|v| v.as_str());

    if let Some(msg_type) = msg_type {
        match msg_type {
            "offer" => {
                if let Some(data) = parsed.get("data") {
                    let request: WebRTCSessionRequest = serde_json::from_value(data.clone())?;

                    let effective_role = if let Some(bound) = ws_role {
                        let offer_role = request
                            .client_type
                            .as_deref()
                            .and_then(SessionRole::parse)
                            .ok_or_else(|| {
                                anyhow::anyhow!("offer clientType required and must match ws clientType")
                            })?;
                        if offer_role != bound {
                            let err = serde_json::json!({
                                "type": "error",
                                "data": {
                                    "code": "clientTypeMismatch",
                                    "message": "offer clientType must match WebSocket clientType"
                                }
                            });
                            let _ = ws.send(Message::text(err.to_string())).await;
                            return Ok(true);
                        }
                        bound
                    } else {
                        parse_client_type(request.client_type.as_deref())
                            .map_err(|e| anyhow::anyhow!(e))?
                    };

                    let config = webrtc::SessionConfig {
                        // LAN: is_cloud=false so new_session ignores ICE lists.
                        ice_servers: webrtc::resolve_ice_servers(
                            vec![],
                            request.ice_servers.unwrap_or_default(),
                        ),
                        local_ip: request.ip.and_then(|ip| ip.parse().ok()),
                        is_cloud: false,
                        role: effective_role,
                        cloud_client_id: None,
                    };

                    match webrtc::get_webrtc_api()
                        .await
                        .new_session(config, connection_id.to_string())
                        .await
                    {
                        Ok(session) => {
                            match session.exchange_offer(&request.sd).await {
                                Ok(answer) => {
                                    match crate::webrtc::claim_and_activate_session(
                                        app_state.clone(),
                                        session.clone(),
                                    )
                                    .await?
                                    {
                                        ClaimRoleResult::Accepted { .. } => {
                                            let response = serde_json::json!({
                                                "type": "answer",
                                                "data": answer
                                            });
                                            let response_str = response.to_string();

                                            if ws.send(Message::text(response_str)).await.is_err() {
                                                crate::webrtc::remove_and_close_session(
                                                    app_state,
                                                    connection_id,
                                                )
                                                .await;
                                                return Err(anyhow::anyhow!("Failed to send answer"));
                                            }
                                        }
                                        ClaimRoleResult::RejectedOccupied => {
                                            let err = serde_json::json!({
                                                "type": "error",
                                                "data": {
                                                    "code": "agentSlotOccupied",
                                                    "message": "Agent session slot is already occupied"
                                                }
                                            });
                                            let _ = ws.send(Message::text(err.to_string())).await;
                                            crate::webrtc::remove_and_close_session(
                                                app_state,
                                                connection_id,
                                            )
                                            .await;
                                            return Ok(true);
                                        }
                                        ClaimRoleResult::RejectedMcpDisabled => {
                                            let err = serde_json::json!({
                                                "type": "error",
                                                "data": {
                                                    "code": "mcpDisabled",
                                                    "message": "MCP is disabled on the device; enable MCP before agent control"
                                                }
                                            });
                                            let _ = ws.send(Message::text(err.to_string())).await;
                                            crate::webrtc::remove_and_close_session(
                                                app_state,
                                                connection_id,
                                            )
                                            .await;
                                            return Ok(true);
                                        }
                                        ClaimRoleResult::NotRegistered => {
                                            crate::webrtc::remove_and_close_session(
                                                app_state,
                                                connection_id,
                                            )
                                            .await;
                                            return Err(anyhow::anyhow!(
                                                "session is no longer registered"
                                            ));
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to exchange offer: {}", e);
                                    crate::webrtc::remove_and_close_session(
                                        app_state,
                                        connection_id,
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to create WebRTC session: {}", e);
                        }
                    }
                }
            }
            "new-ice-candidate" => {
                if let Some(data) = parsed.get("data") {
                    if let Some(session) = app_state.get_session_by_id(connection_id).await {
                        let candidate_json = serde_json::to_string(data)?;
                        if let Err(e) = session.add_ice_candidate(&candidate_json).await {
                            warn!("[ws={}] Failed to add ICE candidate: {}", connection_id, e);
                        }
                    }
                }
            }
            "client-closing" => {
                info!("[ws={}] Received client-closing from {}", connection_id, source);
                return Ok(true);
            }
            _ => {
                warn!(
                    "[ws={}] Unknown message type '{}' from {}. Full message: {}",
                    connection_id, msg_type, source, message
                );
            }
        }
    } else {
        warn!("[ws={}] WebSocket message missing 'type' field: {}", connection_id, message);
    }

    Ok(false)
}

/// Get the global application state
pub fn get_global_app_state() -> Arc<AppState> {
    GLOBAL_APP_STATE.get_or_init(|| Arc::new(AppState::new())).clone()
}

fn parse_client_type(raw: Option<&str>) -> Result<SessionRole, String> {
    match raw {
        None | Some("") => Ok(SessionRole::User),
        Some(s) => SessionRole::parse(s).ok_or_else(|| format!("Invalid clientType: {s}")),
    }
}

fn cookie_name_for_role(role: SessionRole) -> &'static str {
    match role {
        SessionRole::User => AUTH_COOKIE_USER,
        SessionRole::Agent => AUTH_COOKIE_AGENT,
    }
}

fn set_token(req: &mut Request, res: &mut Response, role: SessionRole, auth_token: &str) {
    let is_https = middleware::request_is_https(req);

    // Set auth cookie (7 days expiry, HttpOnly, SameSite)
    let cookie = Cookie::build((cookie_name_for_role(role), auth_token))
        .max_age(time::Duration::days(7))
        .path("/")
        .http_only(true)
        .secure(is_https)
        .same_site(SameSite::Strict)
        .build();
    let set_cookie_auth = cookie.to_string();

    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie_auth).unwrap());
}

/// Clear session cookie for a role. Emit both Secure and non-Secure variants so logout works
/// after HTTP-only or HTTPS-only logins (or switching between them on LAN).
fn clear_token(res: &mut Response, role: SessionRole) {
    let name = cookie_name_for_role(role);
    for secure in [false, true] {
        append_expired_cookie(res, name, secure);
    }
}

/// Expire a session cookie. `secure` must match how the cookie was originally set.
fn append_expired_cookie(res: &mut Response, name: &str, secure: bool) {
    let cookie = Cookie::build((name, ""))
        .max_age(time::Duration::seconds(-1))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .build();
    res.headers_mut().append(
        "Set-Cookie",
        salvo::http::HeaderValue::from_str(&cookie.to_string()).unwrap(),
    );
}