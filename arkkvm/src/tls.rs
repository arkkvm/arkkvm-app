use std::net::IpAddr;

use salvo::conn::rustls::{Keycert, RustlsConfig};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::cert;
use crate::config::get_config_manager;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TlsMode {
    Disabled,
    SelfSigned,
    Custom(String, String),
}

/// TLS state
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct TlsState {
    pub mode: String, // "disabled" | "custom" | "self-signed"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "privateKey")]
    pub private_key: Option<String>,
}

pub async fn init() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default provider");
    Ok(())
}

/// Initializes and returns a RustlsConfig with a self-signed certificate
///
/// # Returns
/// * `RustlsConfig` - The TLS configuration with self-signed certificate
///
/// # Implementation Details
/// - Creates a list of subject alternative names including:
///   - localhost
///   - 127.0.0.1
///   - Local IP address (if available)
/// - Generates a self-signed certificate with these names
/// - Extracts PEM-encoded certificate and private key
/// - Creates RustlsConfig from the certificate and key
///
/// # Errors
/// Will panic if:
/// - Certificate generation fails
/// - Converting PEM data to RustlsConfig fails
///
/// # Example
/// ```
/// let config = init_rustls_config(local_ip).await;
/// let server = axum_server::bind_rustls(addr, config);
/// ```
pub async fn init_rustls_config(
    local_ip: Option<IpAddr>,
    local_ip_v6: Option<IpAddr>,
    certificate: Option<String>,
    private_key: Option<String>,
) -> anyhow::Result<RustlsConfig> {
    let mut domains = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(), // IPv4 loopback
        "::1".to_string(),       // IPv6 loopback
    ];

    // Add mDNS hostname (e.g., arkkvm.local) to certificate
    let config = get_config_manager();
    let network_config = config.get().await.network_config;
    if let Some(hostname) = &network_config.hostname {
        let fqdn = if hostname.ends_with(".local.") {
            hostname.clone()
        } else {
            format!("{}.local.", hostname)
        };
        domains.push(fqdn);
    } else {
        // Default hostname if not configured
        domains.push("arkkvm.local.".to_string());
    }

    if let Some(local_ip) = local_ip {
        domains.push(local_ip.to_string());
    }

    if let Some(local_ip) = local_ip_v6 {
        domains.push(local_ip.to_string());
    }

    let (server_cert_pem, server_key_pem) =
        if let (Some(certificate), Some(private_key)) = (certificate, private_key) {
            if let Err(e) = validate_custom_certificate(&certificate, &private_key) {
                warn!("Custom certificate and private key validation failed: {:?}", e);
                get_default_certificate(domains)?
            }
            else {
                (certificate, private_key)
            }
        } else {
            get_default_certificate(domains)?
        };
    Ok(RustlsConfig::new(Keycert::new().cert(server_cert_pem).key(server_key_pem)))
}

/// Get current TLS state from configuration
pub async fn get_tls_state() -> TlsState {
    let mgr = get_config_manager();
    let cfg = mgr.get().await;
    get_tls_state_from_mode(&cfg.tls_mode)
}

pub async fn get_tls_mode() -> TlsMode {
    let mgr = get_config_manager();
    let cfg = mgr.get().await;
    cfg.tls_mode.clone()
}

/// Apply TLS state and persist configuration
/// For now we only persist the mode. Custom certificate storage can be added later.
pub async fn set_tls_state(tls_state: &TlsState) -> anyhow::Result<()> {
    let tls_mode = get_tls_mode_from_state(&tls_state)?;
    let config_manager = get_config_manager();
    config_manager
        .update(|cfg| {
            if cfg.tls_mode != tls_mode {
                cfg.tls_mode = tls_mode;
            }
        })
        .await?;
    Ok(())
}

fn get_tls_mode_from_state(state: &TlsState) -> anyhow::Result<TlsMode> {
    match state.mode.to_lowercase().trim() {
        "disabled" => Ok(TlsMode::Disabled),
        "self-signed" => Ok(TlsMode::SelfSigned),
        "custom" => {
            let certificate = state.certificate.clone().unwrap_or_default().trim().to_owned();
            let private_key = state.private_key.clone().unwrap_or_default().trim().to_owned();
            if certificate.is_empty() || private_key.is_empty() {
                anyhow::bail!("certificate and private key are required for custom mode");
            }

            // Validate certificate and private key using rustls
            validate_custom_certificate(&certificate, &private_key)?;

            Ok(TlsMode::Custom(certificate, private_key))
        }
        _ => anyhow::bail!(format!("invalid TLS mode: {}", state.mode)),
    }
}

fn get_tls_state_from_mode(mode: &TlsMode) -> TlsState {
    match mode {
        TlsMode::Disabled => {
            TlsState { mode: "disabled".to_string(), certificate: None, private_key: None }
        }

        TlsMode::SelfSigned => {
            TlsState { mode: "self-signed".to_string(), certificate: None, private_key: None }
        }

        TlsMode::Custom(certificate, private_key) => TlsState {
            mode: "custom".to_string(),
            certificate: Some(certificate.clone()),
            private_key: Some(private_key.clone()),
        },
    }
}

/// Validate custom certificate and private key
///
/// Uses rustls low-level API for validation. rustls automatically validates:
/// - Certificate format correctness
/// - Private key format correctness
/// - Certificate and private key matching
pub fn validate_custom_certificate(certificate: &str, private_key: &str) -> anyhow::Result<()> {
    // Parse certificate
    let pem = pem::parse(certificate.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse certificate PEM: {}", e))?;
    let cert_chain = vec![rustls::pki_types::CertificateDer::from(pem.contents().to_vec())];

    // Parse private key
    let pem = pem::parse(private_key.trim())
        .map_err(|e| anyhow::anyhow!("Failed to parse private key PEM: {}", e))?;
    let private_key = rustls::pki_types::PrivateKeyDer::try_from(pem.contents().to_vec())
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?;

    // Use rustls to validate certificate and private key (automatically validates format, matching, etc.)
    let _server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| anyhow::anyhow!("Certificate and private key validation failed: {}", e))?;

    info!("Custom certificate and private key validation passed");
    Ok(())
}

fn get_default_certificate(domains: Vec<String>) -> anyhow::Result<(String, String)> {
    let (ca_issuer, _) = cert::load_or_init_ca().expect("init/load CA failed");
    let (server_cert_pem, server_key_pem) = cert::generate_server_cert(&ca_issuer, domains)
        .expect("generate server certificate failed");
    Ok((server_cert_pem, server_key_pem))
}