use arkkvm::hardware::hdmi::edid;
use arkkvm::hardware::usb::storage::{self, FileTransferTarget};
use arkkvm::hardware::{self, usb};
use arkkvm::{network, audio, cloud, common, config, jiggler, ota, services, tls, video, web, webrtc, zenoh_bus};
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 6)]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let (guard, log_level_handle) = match common::log::init_log() {
        Ok(result) => (result.0, result.1),
        Err(e) => {
            println!("Failed to initialize log: {:?}", e);
            return Err(e);
        }
    };
    
    // Print application logo
    common::log::print_logo();

    // Initialize exception handlers (panic + signal handlers) BEFORE anything else
    // This ensures we capture all crashes and panics
    common::panic_handler::init_exception_handlers();

    config::init_config().await?;
    let config = config::get_config_manager();

    let log_level = config.get_log_level().await;
    log_level_handle.modify(|filter| {
        *filter = EnvFilter::new(log_level);
    })?;

    if let Err(e) = network::ssh::init_ssh_key().await {
        error!("Failed to initialize SSH key: {:?}", e);
    }

    network::settings::init_network_settings().await;

    tls::init().await?;
    webrtc::init_webrtc_api().await?;

    zenoh_bus::init().await?;
    services::init_audio_service(config.get_audio_quality().await).await?;
    hardware::init().await?;
    services::init_usb_service().await?;
    services::init_gui_pipeline()?;

    let usb_config = config.get_usb_config().await;
    let devices = config.get_usb_devices().await;
    if let Err(e) = usb::reconcile_usb_runtime(usb_config, devices, "init").await {
        error!("Initial usb runtime sync failed: {:?}", e);
    } else {
        let state = services::usb::ensure_usb_state().await;
        arkkvm::jsonrpc::broadcast_usb_state(state).await;
    }

    // Initialize virtual mic service when enabled in saved config
    let _ = services::init_virtual_mic_from_config().await;

    // Initialize display (lvgl rotation, static contents, backlight tickers)
    // display::init_display().await?;

    video::init_video_state_updater().await?;

    // Initialize EDID
    info!("Initializing EDID...");
    edid::init_edid().await?;

    network::mdns::init_mdns().await?;

    let web_handle = web::init().await?;

    // Start cloud connection loop
    tokio::spawn(async {
        arkkvm::time_sync::sync_time(config).await;
        ota::power_on_ota_update().await;

        let cloud_manager = cloud::manager::get_cloud_manager();
        if let Err(e) = cloud_manager.start_connection_loop().await {
            error!("Cloud connection loop failed: {}", e);
        }
    });

    tokio::spawn(async {
        ota::check_ota_update_finished().await;
    });

    // Start File Transter
    tokio::spawn(async {
        let config = config::get_config_manager();
        if config.get_emulation_file_transfer().await {
            match config.get_ft_mount_target().await {
                FileTransferTarget::Kvm => {
                    if let Err(e) = storage::load_with_file_img().await {
                        error!("Failed to load file image: {:?}", e);
                    }
                }

                FileTransferTarget::RemoteUsb => {
                    if let Err(e) = storage::mount_with_file_img().await {
                        error!("Failed to mount file image: {:?}", e);
                    }
                }

                _ => {}
            }
        }
    });

    if let Err(e) = jiggler::init().await {
        error!("The Jiggler failed to Create: {:?}", e);
    }

    info!("ArkkVM system initialized successfully. Waiting for shutdown signal...");

    // Wait for shutdown signal
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("Failed to create SIGTERM signal handler");

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down...");
        }
        _ = web_handle => {
            info!("Web server stopped");
        }
    }

    // Graceful shutdown
    info!("Starting graceful shutdown...");
    video::shutdown_video_pipeline().await;
    audio::shutdown_native_audio().await;
    let _ = zenoh_bus::uninit().await;
    let _ = services::uninit_gui_pipeline();
    info!("Shutdown complete");
    drop(guard);
    Ok(())
}