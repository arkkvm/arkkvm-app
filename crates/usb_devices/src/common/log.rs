use anyhow::Result;
use common::log::LogBuilder;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Registry, reload::Handle};

const LOG_FILE_NAME: &str = "arkkvm_usb.log";

pub fn init_log() -> Result<(Option<WorkerGuard>, Handle<EnvFilter, Registry>)> {
    LogBuilder::default()
        .set_output_file(true)
        .set_file_name(LOG_FILE_NAME)
        .set_log_level("info")
        .build()
}

/// Print application logo with version
pub fn print_logo() {
    let version = env!("CARGO_PKG_VERSION");
    info!(
        r#"
=================================================

     ╔══════════════════════════════════════════╗
     ║                                          ║
     ║     ___    ____  __ ____  _______ ____   ║
     ║    /   |  / __ \/ //_/ / / / ___// __ )  ║
     ║   / /| | / /_/ / ,< / / / /\__ \/ __  |  ║
     ║  / ___ |/ _, _/ /| / /_/ /___/ / /_/ /   ║
     ║ /_/  |_/_/ |_/_/ |_\____//____/_____/    ║
     ║                                          ║
     ║   Version: {:<29} ║
     ║   Power By Ensurebit                     ║
     ║                                          ║
     ╚══════════════════════════════════════════╝

===== The ArkUSB application is starting... =====
"#,
        version
    );
}