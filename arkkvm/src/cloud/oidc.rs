use std::time::Duration;

use anyhow::{Result, anyhow};
use id_token_verifier::cache::JwksCacheConfig;
use id_token_verifier::client::*;
use id_token_verifier::validation::*;
use id_token_verifier::*;
use tokio::time::timeout;
use tracing::{debug, warn};

/// OIDC verification configuration
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer_metadata_url: String,
    pub allowed_iss: Vec<String>,
    pub default_allowed_aud: Option<Vec<String>>,
    pub cache_expiration: Duration,
    pub cache_background_refresh: Option<Duration>,
    pub backoff_initial_delay: Duration,
    pub backoff_factor: f32,
    pub backoff_max_delay: Duration,
    pub verify_timeout: Duration,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            issuer_metadata_url: "https://accounts.google.com/.well-known/openid-configuration"
                .to_string(),
            allowed_iss: vec![
                "accounts.google.com".to_string(),
                "https://accounts.google.com".to_string(),
            ],
            default_allowed_aud: None,
            cache_expiration: Duration::from_secs(60 * 5),
            cache_background_refresh: Some(Duration::from_secs(60)),
            backoff_initial_delay: Duration::from_millis(500),
            backoff_factor: 2.0f32,
            backoff_max_delay: Duration::from_secs(8),
            verify_timeout: Duration::from_secs(10),
        }
    }
}

/// OIDC authenticator backed by `id_token_verifier`
pub struct OidcAuthenticator {
    client_config: JwksClientConfig,
    cache_config: JwksCacheConfig,
    allowed_iss: Vec<Iss>,
    default_allowed_aud: Option<Vec<Aud>>,
    verify_timeout: Duration,
}

impl OidcAuthenticator {
    /// Creates a new OIDC authenticator instance
    pub async fn new_with_config(config: OidcConfig) -> Result<Self> {
        let client_config = JwksClientConfig::builder()
            .jwks_url(JwksUrl::discover(&config.issuer_metadata_url)?)
            .backoff(
                backoff_config::ExponentialBackoffConfig {
                    initial_delay: config.backoff_initial_delay,
                    factor: config.backoff_factor,
                    max_delay: config.backoff_max_delay,
                    ..Default::default()
                }
                .into(),
            )
            .build();

        let cache_config = match config.cache_background_refresh {
            Some(interval) => JwksCacheConfig::builder()
                .expiration_duration(config.cache_expiration)
                .background_refresh_interval(interval)
                .reload_on_jwk_not_found(true)
                .build(),
            None => JwksCacheConfig::builder()
                .expiration_duration(config.cache_expiration)
                .reload_on_jwk_not_found(true)
                .build(),
        };

        let allowed_iss = config.allowed_iss.into_iter().map(Iss::new).collect();
        let default_allowed_aud =
            config.default_allowed_aud.map(|v| v.into_iter().map(Aud::new).collect());

        Ok(Self {
            client_config,
            cache_config,
            allowed_iss,
            default_allowed_aud,
            verify_timeout: config.verify_timeout,
        })
    }

    /// Creates a new OIDC authenticator with defaults
    pub async fn new() -> Result<Self> {
        Self::new_with_config(OidcConfig::default()).await
    }

    /// Verifies an OIDC token with a specific client ID
    pub async fn verify_token_with_client_id(
        &self,
        token: &str,
        client_id: &str,
    ) -> Result<String> {
        debug!("Verifying OIDC token with client ID: {}", client_id);
        self.verify_token_internal(token, Some(&[client_id.to_string()])).await
    }

    /// Verifies an OIDC token using default allowed audiences (if configured)
    pub async fn verify_token_skip_client_id(&self, token: &str) -> Result<String> {
        debug!("Verifying OIDC token (using default audiences)");
        let default_aud = self
            .default_allowed_aud
            .as_ref()
            .map(|v| v.iter().map(|a| a.0.clone()).collect::<Vec<_>>());
        self.verify_token_internal(token, default_aud.as_deref()).await
    }

    /// Verifies that the provided token matches the expected Google identity
    pub async fn verify_identity_match(&self, token: &str, expected_identity: &str) -> Result<()> {
        debug!("Verifying OIDC token identity match");
        let google_identity = self.verify_token_skip_client_id(token).await?;
        if google_identity != expected_identity {
            warn!(
                "Google identity mismatch: expected '{}' , got '{}'",
                expected_identity, google_identity
            );
            return Err(anyhow!("Google identity mismatch"));
        }
        Ok(())
    }

    async fn verify_token_internal(
        &self,
        token: &str,
        allowed_aud: Option<&[String]>,
    ) -> Result<String> {
        let auds: Vec<Aud> = match allowed_aud {
            Some(a) => a.iter().cloned().map(Aud::new).collect(),
            None => Vec::new(),
        };

        let validation_config = ValidationConfig::builder()
            .allowed_iss(self.allowed_iss.clone())
            .allowed_aud(auds)
            .build();

        let verifier = {
            let config = IdTokenVerifierConfig::builder()
                .client(self.client_config.clone())
                .validation(validation_config)
                .cache(self.cache_config.clone())
                .build();
            IdTokenVerifierDefault::new(config, reqwest::Client::new())
        };

        let fut = async {
            let value = verifier.verify::<serde_json::Value>(token).await?;
            let sub = value
                .get("sub")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing 'sub' claim"))?;
            let aud_str = match value.get("aud") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .next()
                    .ok_or_else(|| anyhow!("Invalid 'aud' claim"))?
                    .to_string(),
                _ => return Err(anyhow!("Missing 'aud' claim")),
            };
            Ok::<String, anyhow::Error>(format!("{}:{}", aud_str, sub))
        };

        match timeout(self.verify_timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(anyhow!("OIDC token verification timed out")),
        }
    }
}
