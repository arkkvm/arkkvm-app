use tokio::signal::{self, unix::SignalKind};
use tracing::{error, info};

use arkkvm_usb::control::{open_client, serve};
use arkkvm_usb::manager::UsbDeviceManager;

#[cfg(test)]
use arkkvm_usb::test;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let (guard, _log_level_handle) = match arkkvm_usb::common::log::init_log() {
        Ok(result) => (result.0, result.1),
        Err(e) => {
            println!("Failed to initialize log: {:?}", e);
            return;
        }
    };

    arkkvm_usb::common::log::print_logo();

    #[cfg(test)]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() > 1 && args[1].eq_ignore_ascii_case("test") {
            println!("match test");
            test::test_task().await;
            return;
        }
    }

    let manager = match UsbDeviceManager::new(None) {
        Ok(manager) => manager,
        Err(e) => {
            error!("Failed to initialize USB device manager: {:?}", e);
            return;
        }
    };

    let (control, _control_task) = arkkvm_usb::control::spawn_control_service(manager);

    let session = match open_client().await {
        Ok(session) => session,
        Err(e) => {
            error!("Failed to open zenoh session  {:?}", e);
            return;
        }
    };

    let zenoh_task = {
        let control = control.clone();
        let session = session.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(session, control).await {
                error!("zenoh serve exited: {:?}", e);
            }
        })
    };

    info!(
        "usb_devices ready; zenoh keys: {}, {}, {}, {}, {}, {}, {}, {}",
        arkkvm_usb::control::KEY_APPLY,
        arkkvm_usb::control::KEY_GET,
        arkkvm_usb::control::zenoh::KEY_GET_UDC_STATUS,
        arkkvm_usb::control::zenoh::KEY_EVENT_UDC_STATE,
        arkkvm_usb::control::zenoh::KEY_GET_USB_EMULATION_STATE,
        arkkvm_usb::control::zenoh::KEY_SET_USB_EMULATION_STATE,
        arkkvm_usb::control::zenoh::KEY_SET_MIC_PROCESS,
        arkkvm_usb::control::zenoh::KEY_GET_MIC_PROCESS_STATE,
    );

    let mut sigterm =
        signal::unix::signal(SignalKind::terminate()).expect("Failed to create SIGTERM handler");

    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down...");
        }
    }

    zenoh_task.abort();
    let _ = session.close().await;
    drop(guard);
    info!("Shutdown complete");
}
