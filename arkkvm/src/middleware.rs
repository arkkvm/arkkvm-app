use base64::Engine as _;
use base64::engine::general_purpose;
use reqwest::Method;
use salvo::http::header::HeaderValue;
use salvo::prelude::*;
use serde_json::json;
use tracing::{debug, warn};

use crate::config::{get_config_manager, get_dev_mode_state};

/// Extract Basic Auth credentials from Authorization header
fn extract_basic_auth(auth_header: &str) -> Option<(String, String)> {
    if !auth_header.starts_with("Basic ") {
        return None;
    }

    let encoded = &auth_header[6..];
    if let Ok(decoded_bytes) = general_purpose::STANDARD.decode(encoded)
        && let Ok(decoded_str) = String::from_utf8(decoded_bytes)
        && let Some((username, password)) = decoded_str.split_once(':')
    {
        return Some((username.to_string(), password.to_string()));
    }
    None
}

/// Authentication middleware for protected routes
#[handler]
pub async fn auth_middleware(
    req: &mut Request,
    _depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    // If noPassword mode, allow access
    if config.local_auth_mode.is_empty() || config.local_auth_mode == "noPassword" {
        debug!("Authentication bypassed: noPassword mode");
        return;
    }

    // Check for auth token in cookies
    if let Some(auth_cookie) = req.cookie("authToken") {
        let auth_token = auth_cookie.value();

        // Validate auth token using config manager
        if config_manager.validate_auth_token(auth_token).await {
            debug!("Authentication successful: valid auth token");
            return;
        }
    }

    // Authentication failed
    warn!("Authentication failed: invalid or missing auth token");
    res.status_code(StatusCode::UNAUTHORIZED);
    res.render(Json(json!({"error": "Unauthorized"})));
    ctrl.skip_rest();
}

/// Developer mode authentication middleware
#[handler]
pub async fn developer_auth_middleware(
    req: &mut Request,
    _depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    // Check if developer mode is enabled
    match get_dev_mode_state().await {
        Ok(dev_state) => {
            if !dev_state.enabled {
                warn!("Developer mode access denied: developer mode not enabled");
                res.status_code(StatusCode::UNAUTHORIZED);
                res.render(Json(json!({"error": "Developer mode is not enabled"})));
                ctrl.skip_rest();
                return;
            }
        }
        Err(e) => {
            warn!("Failed to check developer mode state: {}", e);
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({"error": "Failed to get developer mode state"})));
            ctrl.skip_rest();
            return;
        }
    }

    // Check if noPassword mode (not allowed for developer routes)
    if config.local_auth_mode == "noPassword" {
        warn!("Developer mode access denied: noPassword mode");
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(json!({"error": "The resource is not available in noPassword mode"})));
        ctrl.skip_rest();
        return;
    }

    // Check for Basic Auth header
    if let Some(auth_header) = req.headers().get("Authorization")
        && let Ok(auth_str) = auth_header.to_str()
        && let Some((_, password)) = extract_basic_auth(auth_str)
    {
        // Validate password using config manager
        if config_manager.validate_password(&password).await {
            debug!("Developer authentication successful: valid Basic Auth");
            return;
        }
    }

    // Authentication failed - request Basic Auth
    warn!("Developer authentication failed: invalid or missing Basic Auth");
    res.headers_mut()
        .insert("WWW-Authenticate", HeaderValue::from_static("Basic realm=\"ArkKVM\""));
    res.status_code(StatusCode::UNAUTHORIZED);
    res.render(Json(json!({"error": "Basic auth is required"})));
    ctrl.skip_rest();
}

/// CSRF token validation middleware for protected write operations.
/// Must run after auth_middleware. Skips for noPassword mode and for GET/HEAD/OPTIONS.
#[handler]
pub async fn csrf_middleware(
    req: &mut Request,
    _depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    let config_manager = get_config_manager();
    let config = config_manager.get().await;

    if config.local_auth_mode.is_empty() || config.local_auth_mode == "noPassword" {
        return;
    }

    let token: String = if is_websocket_request(req) {
        req.query::<String>("csrf-token").unwrap_or_default().trim().to_string()
    } else {
        req.headers()
            .get("X-CSRF-Token")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    if !config_manager.validate_csrf_token(token.as_str()).await {
        warn!("CSRF validation failed: missing or invalid X-CSRF-Token");
        res.status_code(StatusCode::UNAUTHORIZED);
        res.render(Json(json!({"error": "Invalid or missing CSRF token"})));
        ctrl.skip_rest();
    }
}

fn is_websocket_request(req: &Request) -> bool {
    // HTTP/1.1: Upgrade: websocket
    if req.headers()
        .get("Upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_lowercase() == "websocket")
        .unwrap_or(false)
    {
        return true;
    }
    // HTTP/2 (RFC 8441): CONNECT + :protocol = websocket
    // Note: :protocol is in req.extensions() as hyper::ext::Protocol, NOT in headers
    if req.method() == Method::CONNECT {
        if let Some(protocol) = req.extensions().get::<salvo::hyper::ext::Protocol>() {
            if protocol.as_str().trim().to_lowercase() == "websocket" {
                return true;
            }
        }
    }
    false
}

/// Public route middleware (no authentication required)
/// Can be used to add logging or other processing for public routes
#[handler]
pub async fn public_middleware(
    req: &mut Request,
    _depot: &mut Depot,
    _res: &mut Response,
    _ctrl: &mut FlowCtrl,
) {
    debug!("Public route accessed: {}", req.uri().path());
    // No authentication required, just continue
}
