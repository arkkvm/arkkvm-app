use std::path::{Path, PathBuf};

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use rsa::sha2::{Digest, Sha256};
use serde_json;
use tokio::fs;
use tracing::{debug, info, trace, warn};

use crate::config::types::Config;
use crate::hardware::hw;

/// Default configuration file path
const DEFAULT_CONFIG_PATH: &str = "/userdata/arkkvm/config";
/// Backup configuration file suffix
const BACKUP_SUFFIX: &str = ".bak";

/// Configuration persistence layer
#[derive(Debug, Clone)]
pub struct ConfigPersistence {
    config_path: PathBuf,
}

impl ConfigPersistence {
    /// Create new persistence layer with default path
    pub fn new() -> Self {
        Self { config_path: PathBuf::from(DEFAULT_CONFIG_PATH) }
    }

    /// Create new persistence layer with custom path
    pub fn with_path<P: AsRef<Path>>(path: P) -> Self {
        Self { config_path: path.as_ref().to_path_buf() }
    }

    /// Load configuration from file system
    pub async fn load(&self) -> Result<Config> {
        trace!("Loading configuration from {:?}", self.config_path);

        // Check if config file exists
        if !self.config_path.exists() {
            debug!("Configuration file doesn't exist, using defaults");
            return Ok(Config::default());
        }

        // Try to load from primary config file first
        let result = self.load_from_file(&self.config_path).await;
        
        // If primary file loading fails, try backup file
        if let Err(primary_err) = result {
            warn!("Failed to load primary config file: {}, trying backup", primary_err);
            
            let backup_path = self.get_backup_path();
            if backup_path.exists() {
                match self.load_from_file(&backup_path).await {
                    Ok(backup_config) => {
                        info!("Successfully loaded configuration from backup file: {:?}", backup_path);
                        
                        // Try to restore backup to primary location
                        if let Err(restore_err) = self.restore_backup().await {
                            warn!("Failed to restore backup to primary location: {}", restore_err);
                        }
                        
                        return Ok(backup_config);
                    }
                    Err(backup_err) => {
                        warn!("Failed to load backup config file: {}, using defaults", backup_err);
                        return Ok(Config::default());
                    }
                }
            } else {
                debug!("Backup config file doesn't exist, using defaults");
                return Ok(Config::default());
            }
        } else {
            result
        }
    }

    /// Save configuration to file system
    pub async fn save(&self, config: &Config) -> Result<()> {
        trace!("Saving configuration to {:?}", self.config_path);

        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create config directory: {:?}", parent))?;
        }

        // Create backup of existing config file if it exists
        if self.config_path.exists() {
            if let Err(err) = self.create_backup().await {
                warn!("Failed to create backup of config file: {}", err);
                // Continue with save operation even if backup fails
            }
        }

        // Serialize configuration to JSON
        let json_content = serde_json::to_string_pretty(config)
            .with_context(|| "Failed to serialize configuration to JSON")?;

        // Encrypt with device-bound key and write to file atomically
        let encoded = self.encode(&json_content)?;
        let temp_path = self.config_path.with_extension("tmp");
        fs::write(&temp_path, &encoded)
            .await
            .with_context(|| format!("Failed to write temporary config file: {:?}", temp_path))?;

        // Rename to final path (atomic operation on most filesystems)
        fs::rename(&temp_path, &self.config_path).await.with_context(|| {
            format!("Failed to rename config file: {:?} -> {:?}", temp_path, self.config_path)
        })?;

        info!("Configuration saved successfully to {:?}", self.config_path);
        Ok(())
    }

    /// Check if configuration file exists
    pub fn exists(&self) -> bool {
        self.config_path.exists()
    }

    /// Get the configuration file path
    pub fn path(&self) -> &Path {
        &self.config_path
    }

    /// Get backup file path (same directory as source file)
    fn get_backup_path(&self) -> PathBuf {
        let mut backup_path = self.config_path.clone();
        let file_name = backup_path.file_name().unwrap_or_default().to_string_lossy();
        backup_path.set_file_name(format!("{}{}", file_name, BACKUP_SUFFIX));
        backup_path
    }

    /// Load configuration from a specific file
    async fn load_from_file(&self, file_path: &Path) -> Result<Config> {
        trace!("Loading configuration from file: {:?}", file_path);

        // Read file as binary
        let contents = fs::read(file_path)
            .await
            .with_context(|| format!("Failed to read config file: {:?}", file_path))?;

        if contents.is_empty() {
            debug!("Configuration file is empty, using defaults");
            return Ok(Config::default());
        }

        // Decrypt with device-bound key
        let json_str = self.decode(&contents)?;

        // Parse JSON and merge with defaults
        let mut loaded_config = self.parse_and_merge_config(&json_str)?;

        // Validate the loaded configuration
        if let Err(errors) = loaded_config.validate() {
            warn!("Configuration validation failed: {:?}", errors);
            // Return default config if validation fails
            return Ok(Config::default());
        }

        info!("Configuration loaded successfully from {:?}", file_path);
        Ok(loaded_config)
    }

    /// Create backup of current config file
    async fn create_backup(&self) -> Result<()> {
        let backup_path = self.get_backup_path();
        
        info!("Starting backup of config file: {:?} -> {:?}", self.config_path, backup_path);
        
        fs::copy(&self.config_path, &backup_path)
            .await
            .with_context(|| format!("Failed to create backup: {:?} -> {:?}", self.config_path, backup_path))?;
            
        info!("Backup created successfully: {:?}", backup_path);
        Ok(())
    }

    /// Restore backup file to primary location
    async fn restore_backup(&self) -> Result<()> {
        let backup_path = self.get_backup_path();
        
        if !backup_path.exists() {
            return Err(anyhow::anyhow!("Backup file does not exist: {:?}", backup_path));
        }
        
        info!("Starting backup restore: {:?} -> {:?}", backup_path, self.config_path);
        
        fs::copy(&backup_path, &self.config_path)
            .await
            .with_context(|| format!("Failed to restore backup: {:?} -> {:?}", backup_path, self.config_path))?;
            
        info!("Backup restored successfully: {:?} -> {:?}", backup_path, self.config_path);
        Ok(())
    }

    /// Parse JSON and merge with default configuration
    fn parse_and_merge_config(&self, contents: &str) -> Result<Config> {
        // Parse the loaded configuration
        let mut loaded_config: Config =
            serde_json::from_str(contents).with_context(|| "Failed to parse configuration JSON")?;

        // Get default configuration for fallback values
        let default_config = Config::default();

        // Merge critical fields that might be missing in loaded config
        if loaded_config.device_id.is_empty() {
            loaded_config.device_id = hw::get_device_id();
        }

        if loaded_config.cloud_url.is_empty() {
            loaded_config.cloud_url = default_config.cloud_url;
        }

        if loaded_config.cloud_app_url.is_empty() {
            loaded_config.cloud_app_url = default_config.cloud_app_url;
        }

        if loaded_config.default_log_level.is_empty() {
            loaded_config.default_log_level = default_config.default_log_level;
        }

        Ok(loaded_config)
    }

    fn get_code(&self) -> [u8; 32] {
        let input = format!("jianguo_{}", hw::get_device_id());
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let result = hasher.finalize();
        let result = &result[16..];
        let input = format!("{}_zxw_{}", hex::encode(result), hw::get_device_id());
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let result = hasher.finalize();
        result.into()
    }

    fn encode(&self, plain: &str) -> Result<Vec<u8>> {
        let key = self.get_code();
        let cipher = Aes256Gcm::new_from_slice(&key).context("AES-GCM key init")?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plain.as_bytes())
            .map_err(|e| anyhow::anyhow!("config encrypt: {}", e))?;
        let mut out = nonce.as_slice().to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decode(&self, encrypted: &[u8]) -> Result<String> {
        const NONCE_LEN: usize = 12;
        if encrypted.len() < NONCE_LEN {
            anyhow::bail!("config decode: payload too short");
        }
        let (nonce_slice, sealed) = encrypted.split_at(NONCE_LEN);
        let key = self.get_code();
        let cipher = Aes256Gcm::new_from_slice(&key).context("AES-GCM key init")?;
        let nonce_arr: [u8; NONCE_LEN] = nonce_slice
            .try_into()
            .map_err(|_| anyhow::anyhow!("nonce len"))?;
        let nonce = Nonce::from(nonce_arr);
        let plain = cipher
            .decrypt(&nonce, sealed)
            .map_err(|e| anyhow::anyhow!("config decrypt: {}", e))?;
        String::from_utf8(plain).context("config decrypt: invalid UTF-8")
    }
}

impl Default for ConfigPersistence {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn test_save_and_load() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Create test configuration
        let mut config = Config::default();
        config.local_auth_mode = "password".to_string();
        config.display_max_brightness = 100;

        // Save configuration
        persistence.save(&config).await.unwrap();

        // Load configuration
        let loaded_config = persistence.load().await.unwrap();

        // Verify loaded configuration matches saved configuration
        assert_eq!(loaded_config.local_auth_mode, "password");
        assert_eq!(loaded_config.display_max_brightness, 100);
        assert_eq!(loaded_config.cloud_url, config.cloud_url); // Should maintain defaults
    }

    #[tokio::test]
    async fn test_load_nonexistent_file() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("nonexistent.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Loading non-existent file should return default config
        let config = persistence.load().await.unwrap();
        assert_eq!(config.local_auth_mode, "noPassword");
        assert_eq!(config.cloud_url, "https://api.jetkvm.com");
    }

    #[tokio::test]
    async fn test_load_invalid_json() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("invalid.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Write invalid JSON
        fs::write(&config_path, "{ invalid json }").await.unwrap();

        // Loading invalid JSON should return error
        assert!(persistence.load().await.is_err());
    }

    #[tokio::test]
    async fn test_exists() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // File doesn't exist initially
        assert!(!persistence.exists());

        // Save configuration
        let config = Config::default();
        persistence.save(&config).await.unwrap();

        // File should exist now
        assert!(persistence.exists());
    }

    #[tokio::test]
    async fn test_backup_restore_mechanism() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Create initial configuration
        let mut config = Config::default();
        config.local_auth_mode = "password".to_string();
        persistence.save(&config).await.unwrap();

        // Corrupt the primary config file
        fs::write(&config_path, "invalid json content").await.unwrap();

        // Loading should fallback to backup and restore it
        let loaded_config = persistence.load().await.unwrap();
        assert_eq!(loaded_config.local_auth_mode, "password");

        // Verify backup file exists and is in the same directory
        let backup_path = persistence.get_backup_path();
        assert!(backup_path.exists());
        assert_eq!(backup_path.parent(), config_path.parent());

        // Verify primary file was restored from backup (same encrypted content as backup)
        let primary_content = fs::read_to_string(&config_path).await.unwrap();
        let backup_content = fs::read_to_string(&backup_path).await.unwrap();
        assert_eq!(primary_content, backup_content);
    }

    #[tokio::test]
    async fn test_backup_creation() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Create initial configuration
        let config = Config::default();
        persistence.save(&config).await.unwrap();

        // Verify backup was created in the same directory
        let backup_path = persistence.get_backup_path();
        assert!(backup_path.exists());
        assert_eq!(backup_path.parent(), config_path.parent());

        // Verify backup content matches original
        let original_content = fs::read_to_string(&config_path).await.unwrap();
        let backup_content = fs::read_to_string(&backup_path).await.unwrap();
        assert_eq!(original_content, backup_content);
    }

    #[tokio::test]
    async fn test_backup_path_generation() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        let backup_path = persistence.get_backup_path();
        let expected_backup_path = temp_dir.path().join("config.json.bak");
        
        assert_eq!(backup_path, expected_backup_path);
        assert_eq!(backup_path.parent(), config_path.parent());
    }
}