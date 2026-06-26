//! Configuration management module
//!
//! This module provides a comprehensive configuration system with:
//! - Type-safe configuration structures
//! - Persistent storage with JSON serialization
//! - Thread-safe configuration management
//! - Validation and error handling
//! - Password hashing and authentication

use anyhow::{Result, anyhow};
use tokio::sync::OnceCell;
use tracing::info;

pub mod manager;
pub mod persistence;
pub mod types;

pub use manager::ConfigManager;
pub use persistence::ConfigPersistence;
pub use types::DevModeState;
// Re-export commonly used types for convenience (marked as allow unused for future API)
pub use types::{Config, KeyboardMacro, KeyboardMacroStep, WakeOnLanDevice};

/// Global configuration manager instance
static CONFIG_MANAGER: OnceCell<ConfigManager> = OnceCell::const_new();

/// Initialize the global configuration manager
pub async fn init_config() -> Result<()> {
    let manager = ConfigManager::new();
    manager.load().await?;

    if !manager.config_exists() {
        info!("Configuration file doesn't exist, creating with default values");
        manager.save().await?;
        info!("Default configuration saved to {:?}", manager.config_path());
    }

    CONFIG_MANAGER
        .set(manager)
        .map_err(|_| anyhow::anyhow!("Config manager already initialized"))?;

    info!("Configuration manager initialized");
    Ok(())
}

/// Initialize the global configuration manager with custom persistence
pub async fn init_config_with_persistence(persistence: ConfigPersistence) -> Result<()> {
    let manager = ConfigManager::with_persistence(persistence);
    manager.load().await?;

    CONFIG_MANAGER
        .set(manager)
        .map_err(|_| anyhow::anyhow!("Config manager already initialized"))?;

    info!("Configuration manager initialized with custom persistence");
    Ok(())
}

/// Get a reference to the global configuration manager
pub fn get_config_manager() -> &'static ConfigManager {
    CONFIG_MANAGER.get().expect("Config manager not initialized. Call init_config() first.")
}

/// Check developer mode state
pub async fn get_dev_mode_state() -> Result<DevModeState> {
    let path = "/userdata/arkkvm/devmode.enable";
    let enabled = std::path::Path::new(path).exists();
    Ok(DevModeState { enabled })
}

pub async fn set_dev_mode_state(enabled: bool) -> Result<()> {
    let path = "/userdata/arkkvm/devmode.enable";
    let p = std::path::Path::new(path);
    if enabled {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| anyhow!("mkdir failed: {}", e))?;
        }
        std::fs::write(p, []).map_err(|e| anyhow!("create file failed: {}", e))?;
    } else if p.exists() {
        std::fs::remove_file(p).map_err(|e| anyhow!("remove file failed: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn test_global_config_initialization() {
        // This test uses a separate CONFIG_MANAGER to avoid conflicts
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json");
        let persistence = ConfigPersistence::with_path(&config_path);

        // Test initialization with custom persistence
        let manager = ConfigManager::with_persistence(persistence);
        manager.load().await.unwrap();

        // Test basic functionality
        let config = manager.get().await;
        assert_eq!(config.local_auth_mode, "noPassword");
        assert_eq!(config.cloud_url, "https://api.arkkvm.com");
    }

    #[tokio::test]
    async fn test_dev_mode_state() {
        let dev_mode = get_dev_mode_state().await.unwrap();
        assert!(dev_mode.enabled);
    }

    #[tokio::test]
    async fn test_complete_config_workflow() {
        use types::*;

        // Create a temporary config file
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("workflow_config.json");

        let persistence = ConfigPersistence::with_path(&config_path);
        let manager = ConfigManager::with_persistence(persistence);

        // Load initial configuration
        manager.load().await.unwrap();

        // Test authentication workflow
        manager.set_auth_mode("password".to_string()).await.unwrap();
        let password = "test_workflow_password";
        let hashed = manager.hash_password(password).await.unwrap();
        manager.set_hashed_password(Some(hashed)).await.unwrap();

        // Test password validation
        assert!(manager.validate_password(password).await);
        assert!(!manager.validate_password("wrong_password").await);

        // Test cloud configuration
        manager
            .set_cloud_config(
                Some("https://test.example.com".to_string()),
                Some("test_token".to_string()),
            )
            .await
            .unwrap();

        // Test keyboard macro management
        let macro_step =
            KeyboardMacroStep { keys: vec!["ctrl+c".to_string()], modifiers: vec![], delay: 100 };

        let keyboard_macro = KeyboardMacro {
            id: "test_macro".to_string(),
            name: "Test Copy Macro".to_string(),
            steps: vec![macro_step],
            sort_order: Some(1),
        };

        manager.set_keyboard_macro(keyboard_macro).await.unwrap();

        // Test Wake-on-LAN device management
        let wol_device = WakeOnLanDevice {
            name: "Test Server".to_string(),
            mac_address: "00:11:22:33:44:55".to_string(),
        };

        manager.set_wake_on_lan_device(wol_device).await.unwrap();

        // Verify all changes
        let final_config = manager.get().await;
        assert_eq!(final_config.local_auth_mode, "password");
        assert_eq!(final_config.cloud_url, "https://test.example.com");
        assert_eq!(final_config.cloud_token, Some("test_token".to_string()));
        assert_eq!(final_config.keyboard_macros.len(), 1);
        assert_eq!(final_config.wake_on_lan_devices.len(), 1);

        // Verify persistence
        assert!(manager.config_exists());

        println!("Complete configuration workflow test passed!");
    }
}
