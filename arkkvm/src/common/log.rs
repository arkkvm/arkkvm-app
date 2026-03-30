use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Registry, fmt, prelude::*, reload::{self, Handle}};
use file_rotate::{FileRotate, ContentLimit, suffix::AppendCount, compression::Compression};

const LOG_FILE_PATH: &str = "/userdata/arkkvm/logs";
const LOG_FILE_NAME: &str = "arkkvm_app.log";
const DEFAULT_LOG_LEVEL: &str = "info";
const MAX_LOG_FILE_SIZE: usize = 50 * 1024 * 1024;
const MAX_LOG_FILE_COUNT: usize = 5;

pub fn init_log() -> (WorkerGuard, Handle<EnvFilter, Registry>) {
    let _ = std::fs::create_dir_all(LOG_FILE_PATH);

    let rotator = FileRotate::new(
        format!("{}/{}", LOG_FILE_PATH, LOG_FILE_NAME).as_str(),
        AppendCount::new(MAX_LOG_FILE_COUNT),
        ContentLimit::Bytes(MAX_LOG_FILE_SIZE),
        Compression::None,
        None
    );

    let (nb_writer, guard) = tracing_appender::non_blocking(rotator);

    let console_layer = fmt::layer()
        .with_ansi(true)
        .with_level(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        // .with_file(true)
        .with_line_number(true)
        .compact()
        .pretty();

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_level(true)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        // .with_file(true)
        .with_line_number(true)
        .compact()
        .with_writer(nb_writer);

    let filter = EnvFilter::new(DEFAULT_LOG_LEVEL);
    let (filter_layer, log_level_handle) = reload::Layer::new(filter);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(console_layer)
        .with(file_layer)
        .init();

    (guard, log_level_handle)
}

/// Print application logo with version
pub fn print_logo() {
    let version = env!("CARGO_PKG_VERSION");
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