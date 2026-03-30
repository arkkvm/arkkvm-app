use futures::{StreamExt, TryStreamExt};
use salvo::http::cookie::{Cookie, SameSite};
use salvo::prelude::*;
use salvo::serve_static::{StaticDir, static_embed};
use salvo::websocket::{Message, WebSocket, WebSocketUpgrade};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
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
    auth_middleware, csrf_middleware, developer_auth_middleware, public_middleware,
};
use crate::module::rtc_request_params::SettingSwitchParams;
use crate::ota;
use crate::state::AppState;
use crate::{
    tls::{self, TlsMode},
    util, webrtc,
};

static GLOBAL_APP_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebRTCSessionRequest {
    sd: String,
    #[serde(default)]
    ice_servers: Option<Vec<String>>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    #[serde(rename = "OidcGoogle")]
    oidc_google: Option<String>,
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
    // let app_state = get_global_app_state();

    // let (layer, io) = sio::SocketIo::builder().with_state(app_state.clone()).build_layer();

    // io.ns("/", handle_session_connect);

    let app = Router::new()
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
    let handle = if let Some(rustls_config) = rustls_config {
        info!("Starting HTTP server on {} and {}", http_addr, https_addr);
        let acceptor = TcpListener::new(https_addr)
            .rustls(rustls_config)
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

/// Build Rustls configuration based on TLS mode
async fn build_rustls_config(
    local_ip: Option<IpAddr>,
    local_ip_v6: Option<IpAddr>,
) -> anyhow::Result<Option<salvo::conn::rustls::RustlsConfig>> {
    let tls_mode = tls::get_tls_mode().await;
    let rustls_config = match tls_mode {
        TlsMode::Disabled => None,
        TlsMode::SelfSigned => {
            Some(tls::init_rustls_config(local_ip, local_ip_v6, None, None).await?)
        }
        TlsMode::Custom(certificate, private_key) => Some(
            tls::init_rustls_config(local_ip, local_ip_v6, Some(certificate), Some(private_key))
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

/// Protected routes - authentication required, CSRF required for write operations
async fn init_protected_routes() -> anyhow::Result<Router> {
    let router = Router::new()
        .hoop(auth_middleware)
        .hoop(csrf_middleware)
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
        println!("Serving frontend from: ARKKVM_SERVE_DIR({})", serve_dir);
        router.get(StaticDir::new(serve_dir).fallback("index.html"))
    } else if std::fs::exists(local_serve_dir).unwrap_or(false) {
        println!("Serving frontend from: local directory({})", local_serve_dir);
        router.get(StaticDir::new(local_serve_dir).fallback("index.html"))
    } else {
        println!("Serving frontend from: embedded assets");
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
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.local_auth_mode == "noPassword" {
        return Err(StatusError::bad_request().brief("Login is disabled in noPassword mode"));
    }

    let login_req: LoginRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    if !config_manager.validate_password(&login_req.password).await {
        return Err(StatusError::unauthorized().brief("Invalid password"));
    }

    let auth_token = Uuid::new_v4().to_string();
    let csrf_token = config_manager
        .set_auth_token(Some(auth_token.clone()))
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?
        .ok_or_else(|| {
            StatusError::internal_server_error().brief("Failed to save configuration")
        })?;

    let updated_config = config_manager.get().await;
    if updated_config.local_auth_token != Some(auth_token.clone()) {
        warn!("Auth token not properly saved to configuration");
        return Err(StatusError::internal_server_error().brief("Failed to save configuration"));
    }

    // Set auth cookie (7 days expiry, HttpOnly, SameSite)
    let cookie = Cookie::build(("authToken", auth_token.clone()))
        .max_age(time::Duration::days(7))
        .path("/")
        .http_only(true)
        .secure(false)
        .same_site(SameSite::Lax)
        .build();
    let set_cookie = cookie.to_string();
    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());

    // Return CSRF token in header and body so frontend can use it without a separate request
    res.headers_mut()
        .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());
    Ok(Json(ApiResponse { message: "Login successful".to_string(), csrf_token: Some(csrf_token) }))
}

#[endpoint]
async fn handle_logout(res: &mut Response) -> Result<Json<ApiResponse>, StatusError> {
    let config_manager = get_config_manager();

    config_manager
        .set_auth_token(None)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;

    // Clear auth cookie by setting it to expire immediately
    let cookie = Cookie::build(("authToken", ""))
        .max_age(time::Duration::seconds(-1))
        .path("/")
        .http_only(true)
        .secure(false)
        .same_site(SameSite::Lax)
        .build();
    let set_cookie = cookie.to_string();
    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());

    Ok(Json(ApiResponse { message: "Logout successful".to_string(), csrf_token: None }))
}

#[endpoint]
async fn create_password_local(
    req: &mut Request,
    res: &mut Response,
) -> Result<Json<ApiResponse>, StatusError> {
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
    let cookie = Cookie::build(("authToken", auth_token.clone()))
        .max_age(time::Duration::days(7))
        .path("/")
        .http_only(true)
        .secure(false)
        .same_site(SameSite::Lax)
        .build();
    let set_cookie = cookie.to_string();
    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());

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
        return Err(StatusError::unauthorized().brief("Incorrect old password"));
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
    let cookie = Cookie::build(("authToken", new_auth_token.clone()))
        .max_age(time::Duration::days(7))
        .path("/")
        .http_only(true)
        .secure(false)
        .same_site(SameSite::Lax)
        .build();
    let set_cookie = cookie.to_string();
    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());

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
        return Err(StatusError::unauthorized().brief("Incorrect password"));
    }

    config_manager
        .set_hashed_password(None)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;
    config_manager
        .set_auth_token(None)
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;
    config_manager
        .set_auth_mode("noPassword".to_string())
        .await
        .map_err(|_| StatusError::internal_server_error().brief("Failed to save configuration"))?;

    // Clear auth cookie
    let cookie = Cookie::build(("authToken", ""))
        .max_age(time::Duration::seconds(-1))
        .path("/")
        .http_only(true)
        .secure(false)
        .same_site(SameSite::Lax)
        .build();
    let set_cookie = cookie.to_string();
    res.headers_mut()
        .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());

    Ok(Json(ApiResponse {
        message: "Password disabled successfully".to_string(),
        csrf_token: None,
    }))
}

#[endpoint]
async fn handle_device() -> Result<Json<LocalDeviceResponse>, StatusError> {
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    Ok(Json(LocalDeviceResponse {
        auth_mode: Some(config.local_auth_mode),
        device_id: config.device_id,
        loopback_only: config.local_loopback_only,
    }))
}

#[endpoint]
async fn handle_device_status() -> Result<Json<DeviceStatusResponse>, StatusError> {
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

    // let usb_manager = get_usb_manager();
    // let mut usb_manager = usb_manager.write().await;
    // let Some(usb_manager) = usb_manager.as_mut() else {
    //     return Err(StatusError::bad_request().brief("USB manager not initialized"));
    // };

    // let _ = usb_manager.set_oobe_device_settings(&setup_req).await.map_err(|e| {
    //     StatusError::internal_server_error()
    //         .brief(format!("Failed to save virtual device configs: {}", e))
    // })?;

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

    let mut devices = config_manager.get_usb_devices().await;
    devices.camera = setup_req.camera_emulation.unwrap_or(false);
    devices.microphone = setup_req.microphone_emulation.unwrap_or(false);
    devices.mass_storage_ft = setup_req.file_transfer.unwrap_or(false);
    if let Err(e) = usb::reboot_usb_manager(None, Some(devices)).await {
        return Err(StatusError::internal_server_error()
            .brief(format!("Failed to config usb devices: {:?}", &e)));
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
        let cookie = Cookie::build(("authToken", auth_token.clone()))
            .max_age(time::Duration::days(7))
            .path("/")
            .http_only(true)
            .secure(false)
            .same_site(SameSite::Lax)
            .build();
        let set_cookie = cookie.to_string();
        res.headers_mut()
            .append("Set-Cookie", salvo::http::HeaderValue::from_str(&set_cookie).unwrap());
        res.headers_mut()
            .insert("X-CSRF-Token", salvo::http::HeaderValue::from_str(&csrf_token).unwrap());
    } else {
        config_manager
            .set_hashed_password(None)
            .await
            .map_err(|_| StatusError::internal_server_error().brief("Failed to save config"))?;
        config_manager
            .set_auth_token(None)
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
async fn handle_cloud_status() -> Result<Json<CloudStateResponse>, StatusError> {
    // let _app_state = GLOBAL_APP_STATE.get().ok_or_else(StatusError::internal_server_error)?;
    let cloud_manager = crate::cloud::CloudManager::new();

    let cloud_state = cloud_manager.get_cloud_state().await;

    Ok(Json(CloudStateResponse {
        connected: cloud_state.connected,
        url: cloud_state.url,
        app_url: cloud_state.app_url,
    }))
}

#[endpoint]
async fn handle_storage_upload(
    req: &mut Request,
    _res: &mut Response,
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

    let mut total_bytes_written = already_uploaded;
    let mut stream = req.take_body().into_stream();

    while let Some(item) = stream.next().await {
        let frame = match item {
            Ok(f) => f,
            Err(e) => {
                warn!("failed to read request body {}: {}", upload_id, e);
                // let _ = crate::hardware::usb::storage::complete_upload(&upload_id).await;
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
                if let Err(e) =
                    crate::hardware::usb::storage::append_upload_data(&upload_id, bytes.as_ref())
                        .await
                {
                    warn!("failed to write upload chunk {}: {}", upload_id, e);
                    // let _ = crate::hardware::usb::storage::complete_upload(&upload_id).await;
                    return Err(
                        StatusError::internal_server_error().brief("Failed to write upload data")
                    );
                }
                total_bytes_written += bytes.len() as i64;
            }
            Err(_non_data_frame) => {
                // Ignore non-DATA frames (e.g., trailers)
                continue;
            }
        }
    }

    // Finalize and integrity check
    match crate::hardware::usb::storage::complete_upload(&upload_id).await {
        Ok(_) => {
            if total_bytes_written == size {
                info!(
                    "Upload {} completed successfully: {}/{}",
                    upload_id, total_bytes_written, size
                );
            } else {
                warn!(
                    "Upload {} ended before complete file received: {}/{}",
                    upload_id, total_bytes_written, size
                );
            }
            Ok(Json(ApiResponse { message: "Upload completed".to_string(), csrf_token: None }))
        }
        Err(e) => {
            warn!("failed to finalize upload {}: {}", upload_id, e);
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
) -> Result<Json<WebRTCSessionResponse>, StatusError> {
    info!("Received WebRTC session request");

    let request: WebRTCSessionRequest = req
        .parse_json()
        .await
        .map_err(|e| StatusError::bad_request().brief(format!("Invalid JSON: {e}")))?;

    // Use global AppState instead of new default
    let app_state = GLOBAL_APP_STATE
        .get()
        .ok_or_else(|| StatusError::internal_server_error().brief("AppState not initialized"))?
        .clone();

    let webrtc_api = webrtc::get_webrtc_api().await;
    let session_config = webrtc::SessionConfig {
        ice_servers: request.ice_servers,
        local_ip: request.ip.and_then(|ip| ip.parse().ok()),
        is_cloud: false,
    };

    let session =
        webrtc_api.new_session(session_config, Uuid::new_v4().to_string()).await.map_err(|e| {
            warn!("Failed to create WebRTC session: {}", e);
            StatusError::internal_server_error().brief(format!("Failed to create session: {e}"))
        })?;

    let answer = session.exchange_offer(&request.sd).await.map_err(|e| {
        warn!("Failed to exchange offer: {}", e);
        StatusError::internal_server_error().brief(format!("Failed to exchange offer: {e}"))
    })?;

    let session_id = session.id.clone();
    info!("WebRTC session created successfully with id: {}", &session_id);
    // Takeover: close previous current session after 1s
    crate::webrtc::handle_session_takeover(app_state.clone(), &session_id).await;

    // app_state.add_session(session).await;
    // app_state.set_current_session_id(Some(session_id)).await;

    Ok(Json(WebRTCSessionResponse { sd: answer }))
}

#[endpoint]
async fn handle_webrtc_signaling_client(
    req: &mut Request,
    res: &mut Response,
) -> Result<(), StatusError> {
    let source = req.remote_addr().to_string();
    let connection_id = Uuid::new_v4().to_string();

    info!("WebRTC WebSocket connection from {}", source);

    let _ = WebSocketUpgrade::new()
        .upgrade(req, res, |ws| async move {
            if let Err(e) = handle_webrtc_websocket(ws, connection_id, source).await {
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
) -> anyhow::Result<()> {
    let device_metadata = serde_json::json!({
        "type": "device-metadata",
        "data": {
            "deviceVersion": env!("CARGO_PKG_VERSION")
        }
    });

    ws.send(Message::text(device_metadata.to_string())).await?;
    info!("WebRTC WebSocket connection {} established", connection_id);

    let app_state = GLOBAL_APP_STATE
        .get()
        .ok_or_else(|| anyhow::anyhow!("Global AppState not initialized"))?
        .clone();

    while let Some(msg) = ws.recv().await {
        let msg = if let Ok(msg) = msg {
            msg
        } else {
            info!("WebRTC WebSocket connection {} closed", connection_id);
            break;
        };

        if msg.is_text() {
            if let Ok(text) = msg.as_str() {
                if let Err(e) = handle_webrtc_websocket_message(
                    text,
                    &connection_id,
                    &source,
                    &app_state,
                    &mut ws,
                )
                .await
                {
                    warn!("[ws={}] Failed to handle WebRTC message: {}", connection_id, e);
                }
            } else {
                warn!("[ws={}] Failed to convert WebSocket message to string", connection_id);
            }
        } else if msg.is_close() {
            info!("WebRTC WebSocket connection {} closed", connection_id);
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

    if let Some(sess) = app_state.get_session_by_id(&connection_id).await {
        if let Some(pc) = sess.peer_connection.as_ref() {
            let _ = pc.close().await;
        }
        app_state.remove_session(&connection_id).await;
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
) -> anyhow::Result<()> {
    if message == "ping" {
        ws.send(Message::text("pong")).await?;
        return Ok(());
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

                    let config = webrtc::SessionConfig {
                        ice_servers: request.ice_servers,
                        local_ip: request.ip.and_then(|ip| ip.parse().ok()),
                        is_cloud: false,
                    };

                    match webrtc::get_webrtc_api()
                        .await
                        .new_session(config, connection_id.to_string())
                        .await
                    {
                        Ok(session) => {
                            match session.exchange_offer(&request.sd).await {
                                Ok(answer) => {
                                    let response = serde_json::json!({
                                        "type": "answer",
                                        "data": answer
                                    });
                                    let response_str = response.to_string();
                                    
                                    if ws.send(Message::text(response_str)).await.is_err() {
                                        return Err(anyhow::anyhow!("Failed to send answer"));
                                    }

                                    // Takeover: make this the current session, close the previous after 1s
                                    crate::webrtc::handle_session_takeover(
                                        app_state.clone(),
                                        connection_id,
                                    )
                                    .await;

                                    app_state
                                        .set_current_session_id(Some(connection_id.to_string()))
                                        .await;
                                }
                                Err(e) => {
                                    error!("Failed to exchange offer: {}", e);
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

    Ok(())
}

/// Get the global application state
pub fn get_global_app_state() -> Arc<AppState> {
    GLOBAL_APP_STATE.get_or_init(|| Arc::new(AppState::new())).clone()
}
