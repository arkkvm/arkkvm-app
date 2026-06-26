use anyhow::Result;
use file_rotate::{ContentLimit, FileRotate, compression::Compression, suffix::AppendCount};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Registry, fmt,
    prelude::*,
    reload::{self, Handle},
};

const LOG_FILE_PATH: &str = "/userdata/arkkvm/logs";
const DEFAULT_LOG_LEVEL: &str = "info";
const MAX_LOG_FILE_SIZE: usize = 50 * 1024 * 1024;
const MAX_LOG_FILE_COUNT: usize = 5;

macro_rules! console_fmt_layer {
    () => {
        fmt::layer()
            .with_ansi(true)
            .with_level(true)
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_line_number(true)
            .compact()
            .pretty()
    };
}

macro_rules! file_fmt_layer {
    ($path:expr, $file_name:expr, $max_bytes:expr, $max_files:expr) => {{
        let rotator = FileRotate::new(
            format!("{}/{}", $path, $file_name).as_str(),
            AppendCount::new($max_files),
            ContentLimit::Bytes($max_bytes),
            Compression::None,
            None,
        );
        let (nb_writer, guard) = tracing_appender::non_blocking(rotator);
        let file_layer = fmt::layer()
            .with_ansi(false)
            .with_level(true)
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_line_number(true)
            .compact()
            .with_writer(nb_writer);
        (guard, file_layer)
    }};
}

pub struct LogBuilder {
    log_file_path: String,
    log_file_name: String,
    log_file_size: usize,
    log_file_count: usize,
    log_level: String,
    log_console: bool,
    log_file: bool,
}

impl Default for LogBuilder {
    fn default() -> Self {
        Self {
            log_file_path: LOG_FILE_PATH.to_owned(),
            log_file_name: format!("{}.log", env!("CARGO_PKG_NAME")),
            log_file_size: MAX_LOG_FILE_SIZE,
            log_file_count: MAX_LOG_FILE_COUNT,
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            log_console: true,
            log_file: false,
        }
    }
}

impl LogBuilder {
    pub fn set_file_path(&mut self, path: &str) -> &mut Self {
        self.log_file_path = path.to_owned();
        self
    }

    pub fn set_file_name(&mut self, name: &str) -> &mut Self {
        self.log_file_name = name.to_owned();
        self
    }

    pub fn set_file_size(&mut self, size: usize) -> &mut Self {
        self.log_file_size = size;
        self
    }

    pub fn set_file_count(&mut self, count: usize) -> &mut Self {
        self.log_file_count = count;
        self
    }

    pub fn set_log_level(&mut self, level: &str) -> &mut Self {
        self.log_level = level.to_owned();
        self
    }

    pub fn set_output_console(&mut self, console: bool) -> &mut Self {
        self.log_console = console;
        self
    }

    pub fn set_output_file(&mut self, file: bool) -> &mut Self {
        self.log_file = file;
        self
    }

    pub fn build(&self) -> Result<(Option<WorkerGuard>, Handle<EnvFilter, Registry>)> {
        std::fs::create_dir_all(self.log_file_path.as_str())?;

        let filter = EnvFilter::new(self.log_level.as_str());
        let (filter_layer, log_level_handle) = reload::Layer::new(filter);
        let subscriber = tracing_subscriber::registry().with(filter_layer);

        let guard = match (self.log_console, self.log_file) {
            
            // Both console and file log are enabled
            (true, true) => {
                let (file_guard, file_layer) = file_fmt_layer!(
                    &self.log_file_path,
                    &self.log_file_name,
                    self.log_file_size,
                    self.log_file_count
                );
                subscriber
                    .with(console_fmt_layer!())
                    .with(file_layer)
                    .init();
                Some(file_guard)
            }

            // Only enable console log
            (true, false) => {
                subscriber.with(console_fmt_layer!()).init();
                None
            }

            // Only enable file log
            (false, true) => {
                let (file_guard, file_layer) = file_fmt_layer!(
                    &self.log_file_path,
                    &self.log_file_name,
                    self.log_file_size,
                    self.log_file_count
                );
                subscriber.with(file_layer).init();
                Some(file_guard)
            }

            // Both console and file log are disabled
            (false, false) => None,
        };

        Ok((guard, log_level_handle))
    }
}
