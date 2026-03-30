use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use rand::Rng;
use serde::{Deserialize, Serialize};
use base64::Engine;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use rsa::sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::cloud::manager::get_cloud_manager;
use crate::cloud::{CloudManager, OtaInfo};
use crate::config::get_config_manager;
use crate::hardware::usb;
use crate::jsonrpc::handlers::RebootParams;
use crate::jsonrpc::{self, handlers};
use crate::module::rtc_response_params::OtaState;
use crate::webrtc::get_current_session;

const OTA_PACKAGE_PATH: &str = "/userdata/arkkvm/ota/arkkvm_ota.tar";
const OTA_PACKAGE_ROOT_PATH: &str = "/ota/img";
const OTA_CLI: &str = "rk_ota";
const OTA_WAIT_TIME: Duration = Duration::from_mins(20);
const SLEEP_TIME: Duration = Duration::from_mins(1);

const PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvWvPLrqK9fvwcLPHnik2
x73lcC9Nmv3rAain35tp9G/ULUTQeEOfQa5wT/Dlx8P61s3ZmFmvn3FopYK8CH85
87dILdljOIXbPayeUEO/QWyLnfCqedx0hFHk7zbPNinVT1T6HFWGBKcJfvC6Slbh
OysBeW2hzQlTRb23iz3nGiFiOSvl+fSTRV4ohicVzaNXWMA3fdbUEVW7n8JFk9F1
80W/UfHcaC3wRrsF9bw6jBhPeT9z86hb7Kl6/JXSYjXslSfYfLZRsisFwRUTwApv
M5VOWHvyVDm8Ks/e32smN+p4b79ktnNkMLOluier4j3kn9eU/ka95GK6Vnt98lqK
wQIDAQAB
-----END PUBLIC KEY-----";

lazy_static::lazy_static! {
    static ref OTA_INFO: Arc<RwLock<Option<OtaInfo>>> = Arc::new(RwLock::new(None));

    // the switch of auto update
    static ref AUTO_UPDATE_SWITCH: AtomicBool = AtomicBool::new(false);

    //waitting for next check update
    static ref AUTO_UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

    //waitting for user idle
    static ref AUTO_UPDATE_WAITTING: AtomicBool = AtomicBool::new(false);

    // the tag of updateing process
    static ref OTA_UPDATEING: AtomicBool = AtomicBool::new(false);

    // Switch to user update when AUTO_UPDATE_WAITTING task is in progress
    pub static ref UPDATE_BY_USER: AtomicBool = AtomicBool::new(false);

    static ref DOWNLOAD_FAILED_TIMES: AtomicUsize = AtomicUsize::new(0);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub app_version: String,
    pub system_version: String,
    pub web_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub local: VersionInfo,
    pub update: VersionInfo,
    pub app_update_available: bool,
    pub system_update_available: bool,
}

pub async fn get_auto_update() -> bool {
    let config = get_config_manager();
    config.get_auto_update().await
}

pub async fn set_auto_update(enabled: bool) -> Result<()> {
    let config = get_config_manager();
    config.set_auto_update(enabled).await?;
    if enabled {
        AUTO_UPDATE_SWITCH.store(true, std::sync::atomic::Ordering::Relaxed);
        check_update(false).await?;
    } else {
        AUTO_UPDATE_SWITCH.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

pub async fn on_power_on() {
    if Path::new(OTA_PACKAGE_ROOT_PATH).exists() {
        info!("OTA Update Finished, Try Lock System");
        match Command::new(OTA_CLI).args(["--misc=now"]).output().await {
            Ok(output) => {
                if output.status.success() {
                    info!("Lock System successfully");
                } else {
                    error!("Failed to execute Lock System command: {:?}", &output.stderr);
                };
            }
            Err(err) => {
                error!("Failed to execute Lock System command: {:?}", &err);
            }
        };

        if let Err(e) = tokio::fs::remove_file(OTA_PACKAGE_PATH).await {
            error!("Failed to remove OTA package: {:?}", &e);
        }

        if let Err(e) = tokio::fs::remove_dir_all(OTA_PACKAGE_ROOT_PATH).await {
            error!("Failed to remove OTA package root: {:?}", &e);
        }
        info!("Finished to Lock System");
    }
}

pub async fn check_update(by_user: bool) -> Result<UpdateInfo> {
    let auto_update = get_auto_update().await;
    if !by_user && !auto_update {
        return Err(anyhow!("Auto update is disabled"));
    }

    let current_version = get_current_version().await?;
    let manager = get_cloud_manager();
    let ota_info = match manager
        .get_ota_info(current_version.system_version.as_str(), current_version.app_version.as_str())
        .await
    {
        Ok(info) => info,
        Err(e) => {
            error!("Failed to get OTA Info: {:?}", e);
            OtaInfo::default()
        }
    };

    let mut info = OTA_INFO.write().await;
    *info = Some(ota_info.clone());
    drop(info);
    debug!("OTA Info: {:?}", ota_info);

    let has_update = ota_info.system_url.is_some();
    if !by_user && auto_update {
        if has_update {
            ready_update(false);
        } else {
            run_auto_update();
        }
    }

    Ok(UpdateInfo {
        local: current_version,
        update: VersionInfo {
            app_version: ota_info.app_version.unwrap_or_default(),
            system_version: ota_info.system_version.unwrap_or_default(),
            web_version: String::new(),
        },
        app_update_available: ota_info.app_url.is_some(),
        system_update_available: has_update,
    })
}

pub async fn try_update_by_user() -> Result<()> {
    if AUTO_UPDATE_WAITTING.load(Ordering::Relaxed) {
        UPDATE_BY_USER.store(true, Ordering::Relaxed);
    } else {
        UPDATE_BY_USER.store(true, Ordering::Relaxed);
        ready_update(true);
    }
    Ok(())
}

pub async fn get_current_version() -> Result<VersionInfo> {
    let system_out = Command::new("cat").args(["/etc/version/system"]).output().await?;
    let app_out = Command::new("cat").args(["/etc/version/app"]).output().await?;
    let system_version = String::from_utf8(system_out.stdout)?.replace("\n", "");
    let app_version = String::from_utf8(app_out.stdout)?.replace("\n", "");
    let web_version = CloudManager::get_web_version_info().await.unwrap_or_default();
    Ok(VersionInfo { app_version, system_version, web_version })
}

/// Calculate SHA256 hash of a file
async fn calculate_file_sha256(path: &str) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192]; // 8KB buffer

    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let hash_result = hasher.finalize();
    Ok(hex::encode(hash_result))
}

fn run_auto_update() {
    if AUTO_UPDATE_RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    run_loop();
}

fn run_loop() {
    info!("run_loop: Starting auto update loop");
    AUTO_UPDATE_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async {
        let time = {
            let mut rng = rand::rng();
            (Local::now().date_naive() + chrono::Duration::days(1))
                .and_hms_opt(
                    rng.random_range(0..=24),
                    rng.random_range(0..=60),
                    rng.random_range(0..=60),
                )
                .unwrap()
        };
        let time = Local.from_local_datetime(&time).unwrap();
        let now = Local::now();
        let duration_until = time.signed_duration_since(now);

        info!(
            "run_loop: Scheduled next check at {} (current: {}, wait duration: {:?})",
            time.format("%Y-%m-%d %H:%M:%S"),
            now.format("%Y-%m-%d %H:%M:%S"),
            duration_until
        );

        loop {
            if !AUTO_UPDATE_SWITCH.load(std::sync::atomic::Ordering::Relaxed) {
                warn!("run_loop: AUTO_UPDATE_SWITCH is false, stopping loop");
                break;
            }

            let now = Local::now();
            if now >= time {
                info!(
                    "run_loop: Target time reached ({} >= {}), triggering check_update",
                    now.format("%Y-%m-%d %H:%M:%S"),
                    time.format("%Y-%m-%d %H:%M:%S")
                );
                let _ = check_update(false).await;
                break;
            } else {
                let remaining = time.signed_duration_since(now);
                warn!("run_loop: Still waiting, time remaining: {:?}", remaining);
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        info!("run_loop: Loop finished, resetting AUTO_UPDATE_RUNNING flag");
        AUTO_UPDATE_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}

fn ready_update(by_user: bool) {
    if AUTO_UPDATE_WAITTING.load(std::sync::atomic::Ordering::Relaxed) {
        warn!("Auto update is waiting");
        return;
    }

    info!("ready_update: Starting ready update process");
    AUTO_UPDATE_WAITTING.store(true, std::sync::atomic::Ordering::Relaxed);

    tokio::spawn(async move {
        info!("ready_update: Spawned async task");

        let (os_url, os_hash_signature) = {
            let info_guard = OTA_INFO.read().await;
            let Some(info) = info_guard.as_ref() else {
                warn!("ready_update: OTA_INFO is None, aborting");
                AUTO_UPDATE_WAITTING.store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            };
            let Some(os_url) = info.system_url.as_ref() else {
                warn!("ready_update: system_url is None, aborting");
                AUTO_UPDATE_WAITTING.store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            };

            let Some(os_hash_signature) = info.system_hash_signature.as_ref() else {
                warn!("ready_update: system_hash_signature is None, aborting");
                AUTO_UPDATE_WAITTING.store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            };

            (os_url.clone(), os_hash_signature.clone())
        };

        // Check if the target file already exists; skip download if hash matches, otherwise delete the file
        let mut has_package = true;
        if need_download(OTA_PACKAGE_PATH, os_hash_signature.as_str()).await {
            info!("ready_update: Starting download from {}", os_url);
            let manager = get_cloud_manager();
            match manager.download_file(&os_url, OTA_PACKAGE_PATH, &UPDATE_BY_USER).await {
                Ok(hex) => match verify_package(os_hash_signature.as_str(), hex.as_str()) {
                    Ok(()) => {
                        info!("verification successful! File integrity confirmed");
                        jsonrpc::broadcast_ota_state(OtaState::system_verified(
                            100,
                            UPDATE_BY_USER.load(Ordering::Relaxed),
                        ))
                        .await;
                    }
                    Err(e) => {
                        error!("verification failed: {:?}", &e);
                        if let Err(e) = tokio::fs::remove_file(OTA_PACKAGE_PATH).await {
                            warn!("Failed to delete verification failed file: {}", e);
                        }
                        has_package = false;
                        jsonrpc::broadcast_ota_state(OtaState::error(e.to_string())).await;
                    }
                },

                Err(e) => {
                    error!("ready_update: Failed to download OS update: {:?}", &e);
                    has_package = false;
                    jsonrpc::broadcast_ota_state(OtaState::error(e.to_string())).await;

                    if !by_user && !UPDATE_BY_USER.load(Ordering::Relaxed) {
                        let times = DOWNLOAD_FAILED_TIMES.load(Ordering::Relaxed);
                        info!(
                            "ready_update: Failed to download OS update, retrying... (times: {})",
                            times
                        );
                        if times < 3 {
                            DOWNLOAD_FAILED_TIMES.store(times + 1, Ordering::Relaxed);
                            let _ = tokio::spawn(async {
                                let _ = tokio::time::sleep(Duration::from_secs(1));
                                warn!("ready_update: Retrying to download OS update");
                                ready_update(false);
                            });
                        } else {
                            DOWNLOAD_FAILED_TIMES.store(0, Ordering::Relaxed);
                            let _ = tokio::spawn(async {
                                let _ = tokio::time::sleep(Duration::from_secs(1));
                                warn!(
                                    "ready_update: Failed to download OS update, running auto update"
                                );
                                run_auto_update();
                            });
                        }
                    }
                }
            }
        }

        if has_package {
            info!("ready_update: Entering wait loop for suitable update time");
            loop {
                if by_user || UPDATE_BY_USER.load(Ordering::Relaxed) {
                    info!("ready_update: User update requested");
                    tokio::spawn(try_install());
                    break;
                } else {
                    {
                        if !AUTO_UPDATE_SWITCH.load(std::sync::atomic::Ordering::Relaxed) {
                            warn!("ready_update: AUTO_UPDATE_SWITCH is false, breaking loop");
                            break;
                        }

                        let mut do_update = true;
                        if get_current_session().await.is_some() {
                            info!("ready_update: Active session detected, checking USB input time");
                            let elapsed = match usb::get_hid() {
                                Some(hid) => hid.get_last_user_input_time_offset().await,
                                None => u64::MAX,
                            };
                            info!(
                                "ready_update: Last input msg was {:?} ago (wait time: {:?})",
                                elapsed, OTA_WAIT_TIME
                            );
                            if elapsed < OTA_WAIT_TIME.as_secs() {
                                do_update = false;
                            }
                        }

                        if do_update {
                            info!("ready_update: Triggering update now");
                            tokio::spawn(try_install());
                            break;
                        }
                    }

                    tokio::time::sleep(SLEEP_TIME).await;
                }
            }
        }

        info!("ready_update: Finished, resetting AUTO_UPDATE_WAITTING flag");
        AUTO_UPDATE_WAITTING.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}

async fn need_download(download_path: &str, os_hash_signature: &str) -> bool {
    if tokio::fs::metadata(download_path).await.is_ok() {
        info!("Detected existing update file: {}", download_path);
        match calculate_file_sha256(download_path).await {
            Ok(existing_hash) => match verify_package(os_hash_signature, existing_hash.as_str()) {
                Ok(()) => {
                    info!("File hash matches, skipping download");
                    false
                }
                Err(e) => {
                    warn!("File hash mismatch, deleting old file: {:?}", e);
                    if let Err(e) = tokio::fs::remove_file(download_path).await {
                        error!("Failed to delete old file: {:?}", e);
                    }
                    true
                }
            },
            Err(e) => {
                warn!("Failed to calculate existing file hash: {}, deleting file", e);
                if let Err(e) = tokio::fs::remove_file(download_path).await {
                    error!("Failed to delete file: {}", e);
                }
                true
            }
        }
    } else {
        true
    }
}

fn verify_package(system_hash_signature: &str, original: &str) -> Result<()> {
    let pub_key = RsaPublicKey::from_public_key_pem(PUBLIC_KEY)
        .map_err(|e| anyhow!("PUBLIC_KEY parse: {}", e))?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(system_hash_signature.trim())
        .map_err(|e| anyhow!("signature base64 decode: {}", e))?;

    let sig = Signature::try_from(sig_bytes.as_slice())
        .map_err(|e| anyhow!("signature decode: {}", e))?;
    let verifying_key = VerifyingKey::<Sha256>::new(pub_key);
    verifying_key
        .verify(original.as_bytes(), &sig)
        .map_err(|e| anyhow!("failed to verify: {}", e))?;
    Ok(())
}

async fn try_install() -> Result<()> {
    info!("try_install: Attempting to start update");

    if OTA_UPDATEING.load(Ordering::Relaxed) {
        warn!("try_install: Update already in progress, aborting");
        return Err(anyhow!("Update already in progress"));
    }

    OTA_UPDATEING.store(true, Ordering::Relaxed);

    tokio::spawn(async {
        warn!("try_install: Spawned update task");

        let mut has_path = true;
        if !Path::new(OTA_PACKAGE_ROOT_PATH).exists() {
            if let Err(e) = tokio::fs::create_dir_all(OTA_PACKAGE_ROOT_PATH).await {
                error!("try_install: Failed to create OTA package root directory: {:?}", e);
                has_path = false;
                jsonrpc::broadcast_ota_state(OtaState::error(format!(
                    "Failed to create OTA package root directory: {:?}",
                    &e
                )))
                .await;
            }
        }

        if has_path {
            let tar_path_arg = format!("--tar_path={}", OTA_PACKAGE_PATH);
            let save_dir_arg = format!("--save_dir={}", OTA_PACKAGE_ROOT_PATH);
            let args = ["--misc=update", tar_path_arg.as_str(), save_dir_arg.as_str()];
            info!("try_install: Run OTA command");
            jsonrpc::broadcast_ota_state(OtaState::system_update(50)).await;
            match Command::new(OTA_CLI).args(args).output().await {
                Ok(output) => {
                    if output.status.success() {
                        jsonrpc::broadcast_ota_state(OtaState::system_update(100)).await;
                        info!("try_install: Update successful, system should reboot soon");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let _ = handlers::reboot(RebootParams { force: false });
                    } else {
                        let error = String::from_utf8_lossy(&output.stderr);
                        error!("try_install: Failed to execute OTA command: {:?}", &error);
                        jsonrpc::broadcast_ota_state(OtaState::error(error.to_string())).await;

                        if AUTO_UPDATE_SWITCH.load(Ordering::Relaxed) {
                            run_auto_update();
                        }
                    };
                }
                Err(err) => {
                    error!("try_install: Failed to execute OTA command: {:?}", &err);
                    jsonrpc::broadcast_ota_state(OtaState::error(err.to_string())).await;

                    if AUTO_UPDATE_SWITCH.load(Ordering::Relaxed) {
                        run_auto_update();
                    }
                }
            };
        }

        info!("try_install: Resetting OTA_UPDATEING flag");
        OTA_UPDATEING.store(false, Ordering::Relaxed);
    });

    info!("try_install: Update task spawned successfully");
    Ok(())
}
