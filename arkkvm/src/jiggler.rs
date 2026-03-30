use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, RwLock};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::config::get_config_manager;
use crate::hardware::usb;
use crate::webrtc::get_current_session;

const INACTIVITY_LIMIT_MIN: u64 = 30;

lazy_static::lazy_static! {
    static ref JIGGLER: OnceCell<Jiggler> = OnceCell::new();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JigglerConfig {
    pub inactivity_limit_seconds: u64,
    pub jitter_percentage: u32,
    pub schedule_cron_tab: String,
    pub timezone: Option<String>,
}

impl Default for JigglerConfig {
    fn default() -> Self {
        Self {
            inactivity_limit_seconds: 60,
            jitter_percentage: 25,
            schedule_cron_tab: "0 * * * * *".to_owned(),
            timezone: Some("UTC".to_owned()),
        }
    }
}

pub async fn init() -> Result<()> {
    Ok(JIGGLER.set(Jiggler::new().await)?)
}

pub fn get_jiggler_enabled() -> Result<bool> {
    let Some(jiggler) = JIGGLER.get() else {
        error!("Jiggler not initialized");
        return Err(anyhow!("Jiggler not initialized"));
    };
    Ok(jiggler.get_enable())
}

pub async fn get_jiggler_config() -> Result<JigglerConfig> {
    let Some(jiggler) = JIGGLER.get() else {
        error!("Jiggler not initialized");
        return Err(anyhow!("Jiggler not initialized"));
    };
    Ok(jiggler.get_config().await)
}

pub async fn set_jigglers(enable: bool) -> Result<()> {
    let Some(jiggler) = JIGGLER.get() else {
        error!("Jiggler not initialized");
        return Err(anyhow!("Jiggler not initialized"));
    };
    jiggler.set_enable(enable).await
}

pub async fn set_jiggler_config(jiggler_config: JigglerConfig) -> Result<()> {
    if jiggler_config.inactivity_limit_seconds < INACTIVITY_LIMIT_MIN {
        return Err(anyhow!("Inactivity limit must be at least 30 seconds"));
    }

    let Some(jiggler) = JIGGLER.get() else {
        error!("Jiggler not initialized");
        return Err(anyhow!("Jiggler not initialized"));
    };

    jiggler.set_config(jiggler_config).await
}

#[derive(Debug)]
pub struct Jiggler {
    config: Arc<RwLock<JigglerConfig>>,
    enabled: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    config_update: Arc<AtomicBool>,
}

impl Jiggler {
    async fn new() -> Self {
        let config = get_config_manager();
        let jiggler_config = config.get_jiggler_config().await;
        let enabled = config.get_jiggler_enable().await;
        let jiggler = Self {
            config: Arc::new(RwLock::new(jiggler_config)),
            enabled: Arc::new(AtomicBool::new(enabled)),
            running: Arc::new(AtomicBool::new(false)),
            config_update: Arc::new(AtomicBool::new(false)),
        };
        if enabled {
            jiggler.run();
        }
        jiggler
    }

    async fn get_config(&self) -> JigglerConfig {
        self.config.read().await.clone()
    }

    async fn set_config(&self, jiggler_config: JigglerConfig) -> Result<()> {
        let mut config_lock = self.config.write().await;
        *config_lock = jiggler_config.clone();
        drop(config_lock);

        self.config_update.store(true, Ordering::Relaxed);

        let config = get_config_manager();
        config.set_jiggler_config(jiggler_config).await
    }

    fn get_enable(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    async fn set_enable(&self, enable: bool) -> Result<()> {
        self.enabled.store(enable, Ordering::Relaxed);
        if enable {
            self.run();
        }

        let config = get_config_manager();
        config.set_jiggler_enable(enable).await
    }

    fn run(&self) {
        if self.running.load(Ordering::Relaxed) {
            warn!("Jiggler already running");
            return;
        }

        let config = self.config.clone();
        let config_update = self.config_update.clone();

        let enabled = self.enabled.clone();
        let running = self.running.clone();
        tokio::spawn(async move {
            running.store(true, Ordering::Relaxed);

            let mut jiggler_config = { config.read().await.clone() };
            info!("Jiggler Startting with config: {:?}", &jiggler_config);

            let mut last_action_time = Instant::now();

            loop {
                // Check jiggler is enabled
                if !enabled.load(Ordering::Relaxed) {
                    warn!("Jiggler disabled");
                    break;
                }

                // Check jiggler config has changed
                if config_update.load(Ordering::Relaxed) {
                    jiggler_config = { config.read().await.clone() };
                    info!("Jiggler config updated: {:?}", &jiggler_config);
                    config_update.store(false, Ordering::Relaxed);
                }

                // Get last input time offset
                let mut input_offset = u64::MAX;
                let Some(hid) = usb::get_hid() else {
                    warn!("HID not initialized");
                    break;
                };
                
                if get_current_session().await.is_some() {
                    input_offset = hid.get_last_user_input_time_offset().await;
                }

                // Check inactivity
                let limit_time =
                    u64::min(jiggler_config.inactivity_limit_seconds, INACTIVITY_LIMIT_MIN);

                // Check if input offset is greater than inactivity limit
                if input_offset >= limit_time && last_action_time.elapsed().as_secs() >= limit_time
                {
                    info!("Jiggler: input offset is greater than inactivity limit, sending mouse report");

                    if let Err(e) = hid.abs_mouse_report(0, 0, 0u8, false).await {
                        error!("Failed to send abs mouse report: {:?}", e);
                        if let Err(e) = hid.rel_mouse_report(-1, -1, 0, false).await {
                            error!("Failed to send rel mouse report: {:?}", e);
                        }
                    };

                    tokio::time::sleep(Duration::from_millis(100)).await;

                    if let Err(e) = hid.abs_mouse_report(1, 1, 0u8, false).await {
                        error!("Failed to send abs mouse report: {:?}", e);
                        if let Err(e) = hid.rel_mouse_report(1, 1, 0, false).await {
                            error!("Failed to send rel mouse report: {:?}", e);
                        }
                    };
                    last_action_time = Instant::now();
                }

                tokio::time::sleep(Duration::from_secs(limit_time / 2)).await
            }

            running.store(false, Ordering::Relaxed);
            info!("Jiggler has stoped");
        });
    }
}
