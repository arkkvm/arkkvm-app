use anyhow::Result;
use common::log::LogBuilder;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Registry, reload::Handle};

const LOG_FILE_NAME: &str = "arkkvm_app.log";

pub fn init_log() -> Result<(Option<WorkerGuard>, Handle<EnvFilter, Registry>)> {
    LogBuilder::default()
        .set_output_file(true)
        .set_file_name(LOG_FILE_NAME)
        .build()
}

/// Print application logo with version
pub fn print_logo() {
    let version = super::get_app_version(true);
    info!(
        r#"
========================================================

     ╔════════════════════════════════════════════╗
     ║                                            ║
     ║     ___       _     _  ___     ____  __    ║
     ║    / _ \ _ __| | __| |/ \ \   / /  \/  |   ║
     ║   | |_| | '__| |/ /| ' / \ \ / /| |\/| |   ║
     ║   |  _  | |  |   < | . \  \ V / | |  | |   ║
     ║   |_| |_|_|  |_|\_\|_|\_\  \_/  |_|  |_|   ║
     ║                                            ║
     ║   Version: {:<31} ║
     ║   Power By Ensurebit                       ║
     ║                                            ║
     ╚════════════════════════════════════════════╝

======== The ArkKVM application is starting... =========
"#,
        version
    );
}