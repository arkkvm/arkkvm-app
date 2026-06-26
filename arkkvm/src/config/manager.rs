use std::sync::Arc;

use anyhow::{Result, bail};
use bcrypt::{DEFAULT_COST, hash, verify};
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

/// Auth and CSRF token validity duration (same as cookie max_age). 7 days in seconds.
const AUTH_TOKEN_VALIDITY_SECS: i64 = 7 * 24 * 3600;

use crate::config::persistence::ConfigPersistence;
use crate::config::types::{Config, NetworkConfig, UsbConfig, UsbDevices};
use crate::hardware::usb::storage::FileTransferTarget;
use crate::jiggler::JigglerConfig;
use crate::jsonrpc::handlers::UsbDevicesState;

/// Global configuration manager with thread-safe access
#[derive(Debug)]
pub struct ConfigManager {
    config: Arc<RwLock<Config>>,
    persistence: ConfigPersistence,
}

impl ConfigManager {
    /// Create new configuration manager with default persistence
    pub fn new() -> Self {
        Self {
            config: Arc::new(RwLock::new(Config::default())),
            persistence: ConfigPersistence::new(),
        }
    }

    /// Create new configuration manager with custom persistence
    pub fn with_persistence(persistence: ConfigPersistence) -> Self {
        Self { config: Arc::new(RwLock::new(Config::default())), persistence }
    }

    /// Load configuration from persistent storage
    pub async fn load(&self) -> Result<()> {
        let mut loaded_config = self.persistence.load().await?;
        let migrated = loaded_config.migrate_microphone_emulation();
        *self.config.write().await = loaded_config;
        if migrated {
            self.save().await?;
            info!("migrated microphone_emulation from usb_devices.microphone");
        }
        info!("Configuration loaded successfully");
        Ok(())
    }

    /// Save current configuration to persistent storage
    pub async fn save(&self) -> Result<()> {
        let config = self.config.read().await;
        self.persistence.save(&config).await?;
        info!("Configuration saved successfully");
        Ok(())
    }

    /// Get a copy of the current configuration
    pub async fn get(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Update configuration with validation and persistence
    pub async fn update<F>(&self, updater: F) -> Result<()>
    where
        F: FnOnce(&mut Config),
    {
        {
            let mut config = self.config.write().await;
            updater(&mut config);

            // Validate the updated configuration
            if let Err(errors) = config.validate() {
                warn!("Configuration validation failed: {:?}", errors);
                bail!("Configuration validation failed: {:?}", errors);
            }
        }

        self.save().await?;
        Ok(())
    }

    /// Set authentication mode
    pub async fn set_auth_mode(&self, mode: String) -> Result<()> {
        self.update(|config| {
            config.local_auth_mode = mode;
        })
        .await
    }

    /// Set hashed password
    pub async fn set_hashed_password(&self, hashed_password: Option<String>) -> Result<()> {
        self.update(|config| {
            config.hashed_password = hashed_password;
        })
        .await
    }

    /// Set authentication token. When setting Some(token), also generates and stores a CSRF token
    /// and sets expiry (same as cookie, 7 days). Returns the new CSRF token for the response.
    /// When setting None, clears auth, CSRF token and expiry.
    pub async fn set_auth_token(&self, token: Option<String>) -> Result<Option<String>> {
        let (csrf, expires_at) = if token.is_some() {
            let exp = Utc::now().timestamp() + AUTH_TOKEN_VALIDITY_SECS;
            (Some(Uuid::new_v4().to_string()), Some(exp))
        } else {
            (None, None)
        };
        self.update(|config| {
            config.local_auth_token = token;
            config.local_csrf_token = csrf.clone();
            config.local_auth_token_expires_at = expires_at;
        })
        .await?;
        Ok(csrf)
    }

    /// Get current CSRF token (for the active session). None if no password mode or not set.
    pub async fn get_csrf_token(&self) -> Option<String> {
        let config = self.get().await;
        config.local_csrf_token.clone()
    }

    /// Validate CSRF token: must match stored token and not be expired (same expiry as auth token).
    pub async fn validate_csrf_token(&self, token: &str) -> bool {
        let config = self.get().await;
        if config.local_auth_mode.is_empty() || config.local_auth_mode == "noPassword" {
            return true;
        }
        let now = Utc::now().timestamp();
        let not_expired = config
            .local_auth_token_expires_at
            .map(|exp| exp > now)
            .unwrap_or(false);
        let token_ok = config
            .local_csrf_token
            .as_ref()
            .map(|t| !token.is_empty() && t == token)
            .unwrap_or(false);
        not_expired && token_ok
    }

    /// Set cloud configuration
    pub async fn set_cloud_config(
        &self,
        cloud_url: Option<String>,
        cloud_token: Option<String>,
    ) -> Result<()> {
        self.update(|config| {
            if let Some(url) = cloud_url {
                config.cloud_url = url;
            }
            config.cloud_token = cloud_token;
        })
        .await
    }

    /// Add or update a keyboard macro
    pub async fn set_keyboard_macro(
        &self,
        mut macro_item: crate::config::types::KeyboardMacro,
    ) -> Result<()> {
        // Validate the macro before adding
        macro_item.validate()?;

        self.update(|config| {
            // Check if macro exists (by ID)
            if let Some(existing) =
                config.keyboard_macros.iter_mut().find(|m| m.id == macro_item.id)
            {
                *existing = macro_item.clone();
            } else {
                config.keyboard_macros.push(macro_item);
            }
        })
        .await
    }

    /// Remove a keyboard macro by ID
    pub async fn remove_keyboard_macro(&self, macro_id: &str) -> Result<()> {
        self.update(|config| {
            config.keyboard_macros.retain(|m| m.id != macro_id);
        })
        .await
    }

    /// Add or update a wake-on-LAN device
    pub async fn set_wake_on_lan_device(
        &self,
        device: crate::config::types::WakeOnLanDevice,
    ) -> Result<()> {
        self.update(|config| {
            // Check if device exists (by name)
            if let Some(existing) =
                config.wake_on_lan_devices.iter_mut().find(|d| d.name == device.name)
            {
                *existing = device.clone();
            } else {
                config.wake_on_lan_devices.push(device);
            }
        })
        .await
    }

    /// Remove a wake-on-LAN device by name
    pub async fn remove_wake_on_lan_device(&self, device_name: &str) -> Result<()> {
        self.update(|config| {
            config.wake_on_lan_devices.retain(|d| d.name != device_name);
        })
        .await
    }

    /// Set display configuration
    pub async fn set_display_config(
        &self,
        rotation: Option<String>,
        max_brightness: Option<u32>,
        dim_after_sec: Option<u32>,
        off_after_sec: Option<u32>,
    ) -> Result<()> {
        self.update(|config| {
            if let Some(r) = rotation {
                config.display_rotation = r;
            }
            if let Some(b) = max_brightness {
                config.display_max_brightness = b;
            }
            if let Some(d) = dim_after_sec {
                config.display_dim_after_sec = d;
            }
            if let Some(o) = off_after_sec {
                config.display_off_after_sec = o;
            }
        })
        .await
    }

    pub async fn set_oobe_settings(
        &self,
        microphone_emulation: bool,
        camera_emulation: bool,
        file_transfer: bool,
        audio_playback: bool,
    ) -> Result<()> {
        self.update(|config| {
            config.usb_devices.microphone = microphone_emulation;
            config.microphone_emulation = Some(microphone_emulation);
            config.usb_devices.camera = camera_emulation;
            config.usb_devices.mass_storage_ft = file_transfer;
            config.audio_playback = audio_playback;
        })
        .await
    }

    // pub async fn set_emulation_microphone(&self, enabled: bool) -> Result<()> {
    //     self.update(|config| {
    //         config.usb_devices.microphone = enabled;
    //     })
    //     .await
    // }

    // pub async fn set_emulation_camera(&self, enabled: bool) -> Result<()> {
    //     self.update(|config| {
    //         config.usb_devices.camera = enabled;
    //     })
    //     .await
    // }

    // pub async fn set_emulation_file_transfer(&self, enabled: bool) -> Result<()> {
    //     self.update(|config| {
    //         config.usb_devices.mass_storage_ft = enabled;
    //     })
    //     .await
    // }

    pub async fn set_emulation_audio_playback(&self, enabled: bool) -> Result<bool> {
        let mut has_changed = false;
        self.update(|config| {
            if config.audio_playback != enabled {
                has_changed = true;
                config.audio_playback = enabled;
            }
        })
        .await?;
        Ok(has_changed)
    }

    pub async fn set_dev_channel_state(&self, enabled: bool) -> Result<()> {
        self.update(|config| {
            config.dev_channel_enabled = enabled;
        })
        .await
    }

    pub async fn set_usb_devices_state(&self, state: &UsbDevicesState) -> Result<()> {
        self.update(|config| {
            config.usb_devices.keyboard = state.keyboard;
            config.usb_devices.relative_mouse = state.keyboard;
            config.usb_devices.absolute_mouse = state.absolute_mouse;
            config.usb_devices.mass_storage_vm = state.mass_storage;
            config.usb_devices.microphone = state.microphone;
        })
        .await
    }

    pub async fn get_microphone_emulation(&self) -> bool {
        self.get().await.effective_microphone_emulation()
    }

    pub async fn set_microphone_emulation(&self, enabled: bool) -> Result<()> {
        self.update(|config| {
            config.microphone_emulation = Some(enabled);
        })
        .await
    }

    pub async fn set_auto_update(&self, enabled: bool) -> Result<()> {
        self.update(|config| {
            config.auto_update_enabled = enabled;
        })
        .await
    }

    pub async fn set_usb_devices(&self, devices: &UsbDevices) -> Result<()> {
        self.update(|config| {
            config.usb_devices.keyboard = devices.keyboard;
            config.usb_devices.relative_mouse = devices.keyboard;
            config.usb_devices.absolute_mouse = devices.absolute_mouse;
            config.usb_devices.mass_storage_vm = devices.mass_storage_vm;
            config.usb_devices.mass_storage_ft = devices.mass_storage_ft;
            config.usb_devices.microphone = devices.microphone;
            config.usb_devices.camera = devices.camera;
        })
        .await
    }

    pub async fn get_usb_devices(&self) -> UsbDevices {
        let config = self.get().await;
        config.usb_devices.clone()
    }

    pub async fn get_auto_update(&self) -> bool {
        self.get().await.auto_update_enabled
    }

    pub async fn get_usb_devices_state(&self) -> UsbDevicesState {
        let config = self.get().await;
        UsbDevicesState {
            keyboard: config.usb_devices.keyboard,
            absolute_mouse: config.usb_devices.absolute_mouse,
            relative_mouse: config.usb_devices.keyboard,
            mass_storage: config.usb_devices.mass_storage_vm,
            microphone: config.usb_devices.microphone,
        }
    }

    pub async fn get_dev_channel_state(&self) -> bool {
        self.get().await.dev_channel_enabled
    }

    pub async fn get_emulation_microphone(&self) -> bool {
        self.get_microphone_emulation().await
    }

    pub async fn get_emulation_camera(&self) -> bool {
        self.get().await.usb_devices.camera
    }

    pub async fn get_emulation_file_transfer(&self) -> bool {
        self.get().await.usb_devices.mass_storage_ft
    }

    pub async fn get_emulation_audio_playback(&self) -> bool {
        self.get().await.audio_playback
    }

    pub async fn get_network_config(&self) -> NetworkConfig {
        self.get().await.network_config.clone()
    }

    pub async fn set_ft_mount_target(&self, target: FileTransferTarget) -> Result<()> {
        self.update(|config| {
            config.file_transfer_target = target;
        })
        .await
    }

    pub async fn get_ft_mount_target(&self) -> FileTransferTarget {
        self.get().await.file_transfer_target
    }

    pub async fn get_usb_config(&self) -> UsbConfig {
        self.get().await.usb_config.clone()
    }

    pub async fn set_usb_config(&self, usb_config: UsbConfig) -> Result<()> {
        self.update(|config| config.usb_config = usb_config).await
    }

    pub async fn get_jiggler_config(&self) -> JigglerConfig {
        self.get().await.jiggler_config.clone()
    }

    pub async fn set_jiggler_config(&self, jiggler_config: JigglerConfig) -> Result<()> {
        self.update(|config| config.jiggler_config = jiggler_config).await
    }

    pub async fn set_jiggler_enable(&self, enabled: bool) -> Result<()> {
        self.update(|config| {
            config.jiggler_enabled = enabled;
        })
        .await
    }

    pub async fn get_jiggler_enable(&self) -> bool {
        self.get().await.jiggler_enabled
    }

    pub async fn get_log_level(&self) -> String {
        self.get().await.default_log_level.to_lowercase()
    }

    pub async fn set_video_quality(&self, quality: f32) -> Result<()> {
        self.update(|config| {
            config.video_quality = quality;
        })
        .await
    }

    pub async fn set_audio_quality(&self, quality: f32) -> Result<()> {
        self.update(|config| {
            config.audio_quality = quality;
        })
        .await
    }

    pub async fn get_video_quality(&self) -> f32 {
        self.get().await.video_quality
    }

    pub async fn get_audio_quality(&self) -> f32 {
        self.get().await.audio_quality
    }

    /// Check if device is set up (has auth mode configured)
    pub async fn is_setup(&self) -> bool {
        let config = self.get().await;
        !config.is_setup_required()
    }

    /// Validate authentication token: must match stored token and not be expired (7 days from issue).
    pub async fn validate_auth_token(&self, token: &str) -> bool {
        let config = self.get().await;

        if config.local_auth_mode.is_empty() || config.local_auth_mode == "noPassword" {
            return true;
        }

        let now = Utc::now().timestamp();
        let not_expired = config
            .local_auth_token_expires_at
            .map(|exp| exp > now)
            .unwrap_or(false);
        let token_ok = config
            .local_auth_token
            .as_ref()
            .map(|t| !token.is_empty() && token == t)
            .unwrap_or(false);
        not_expired && token_ok
    }

    /// Validate password using bcrypt
    pub async fn validate_password(&self, password: &str) -> bool {
        let config = self.get().await;

        if let Some(ref hashed_password) = config.hashed_password {
            let password = password.to_string();
            let hashed_password = hashed_password.clone();

            // Use spawn_blocking for CPU-intensive bcrypt verification
            match tokio::task::spawn_blocking(move || verify(password, &hashed_password)).await {
                Ok(Ok(is_valid)) => is_valid,
                Ok(Err(e)) => {
                    warn!("Password verification failed: {}", e);
                    false
                }
                Err(e) => {
                    warn!("Task join error during password verification: {}", e);
                    false
                }
            }
        } else {
            false
        }
    }

    /// Hash password using bcrypt
    pub async fn hash_password(&self, password: &str) -> Result<String> {
        let password = password.to_string();

        // Use spawn_blocking for CPU-intensive bcrypt hashing
        let hashed = tokio::task::spawn_blocking(move || hash(password, DEFAULT_COST)).await??;

        Ok(hashed)
    }

    /// Get configuration file path
    pub fn config_path(&self) -> &std::path::Path {
        self.persistence.path()
    }

    /// Check if configuration file exists
    pub fn config_exists(&self) -> bool {
        self.persistence.exists()
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::config::types::{KeyboardMacro, KeyboardMacroStep, WakeOnLanDevice};

    #[tokio::test]
    async fn test_config_manager_lifecycle() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);
        let manager = ConfigManager::with_persistence(persistence);

        // Load initial configuration
        manager.load().await.unwrap();
        let initial_config = manager.get().await;
        assert_eq!(initial_config.local_auth_mode, "noPassword");

        // Update configuration
        manager.set_auth_mode("password".to_string()).await.unwrap();

        // Verify update
        let updated_config = manager.get().await;
        assert_eq!(updated_config.local_auth_mode, "password");

        // Verify persistence
        assert!(manager.config_exists());
    }

    #[tokio::test]
    async fn test_keyboard_macro_management() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);
        let manager = ConfigManager::with_persistence(persistence);

        // Create a test macro
        let mut macro_item = KeyboardMacro {
            id: "test_macro".to_string(),
            name: "Test Macro".to_string(),
            steps: vec![KeyboardMacroStep {
                keys: vec!["a".to_string()],
                modifiers: vec![],
                delay: 100,
            }],
            sort_order: Some(1),
        };

        // Add macro
        manager.set_keyboard_macro(macro_item.clone()).await.unwrap();

        // Verify macro was added
        let config = manager.get().await;
        assert_eq!(config.keyboard_macros.len(), 1);
        assert_eq!(config.keyboard_macros[0].id, "test_macro");

        // Update macro
        macro_item.name = "Updated Macro".to_string();
        manager.set_keyboard_macro(macro_item).await.unwrap();

        // Verify macro was updated
        let config = manager.get().await;
        assert_eq!(config.keyboard_macros.len(), 1);
        assert_eq!(config.keyboard_macros[0].name, "Updated Macro");

        // Remove macro
        manager.remove_keyboard_macro("test_macro").await.unwrap();

        // Verify macro was removed
        let config = manager.get().await;
        assert_eq!(config.keyboard_macros.len(), 0);
    }

    #[tokio::test]
    async fn test_wake_on_lan_device_management() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);
        let manager = ConfigManager::with_persistence(persistence);

        // Create a test device
        let device = WakeOnLanDevice {
            name: "Test Device".to_string(),
            mac_address: "00:11:22:33:44:55".to_string(),
        };

        // Add device
        manager.set_wake_on_lan_device(device.clone()).await.unwrap();

        // Verify device was added
        let config = manager.get().await;
        assert_eq!(config.wake_on_lan_devices.len(), 1);
        assert_eq!(config.wake_on_lan_devices[0].name, "Test Device");

        // Remove device
        manager.remove_wake_on_lan_device("Test Device").await.unwrap();

        // Verify device was removed
        let config = manager.get().await;
        assert_eq!(config.wake_on_lan_devices.len(), 0);
    }

    #[tokio::test]
    async fn test_password_hashing_and_validation() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);
        let manager = ConfigManager::with_persistence(persistence);
        let password = "test_password";

        // Hash password
        let hashed = manager.hash_password(password).await.unwrap();
        assert_ne!(hashed, password);

        // Set hashed password
        manager.set_hashed_password(Some(hashed)).await.unwrap();

        // Validate correct password
        assert!(manager.validate_password(password).await);

        // Validate incorrect password
        assert!(!manager.validate_password("wrong_password").await);
    }
}
