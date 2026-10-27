//! Follow `/dev/kmsg` into the app tracing log (`target: "sys"`).
//! Only compiled with `env_dev`.

use std::time::Duration;

use futures_util::StreamExt;
use rmesg::entry::LogLevel;
use rmesg::{logs_stream, Backend};
use tracing::{debug, error, info, warn};

const TARGET: &str = "sys";
const OPEN_RETRY_MAX: u32 = 10;
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(500);

pub async fn follow() {
    let mut stream = None;
    for attempt in 1..=OPEN_RETRY_MAX {
        match logs_stream(Backend::DevKMsg, false, false).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                warn!(
                    target: TARGET,
                    error = %e,
                    attempt,
                    max_attempts = OPEN_RETRY_MAX,
                    "kmsg follow: failed to open /dev/kmsg"
                );
                if attempt < OPEN_RETRY_MAX {
                    tokio::time::sleep(OPEN_RETRY_INTERVAL).await;
                }
            }
        }
    }
    let Some(mut stream) = stream else {
        return;
    };

    info!(target: TARGET, "kmsg follow started");

    while let Some(item) = stream.next().await {
        match item {
            Ok(entry) => emit(&entry.message, entry.level),
            Err(e) => {
                warn!(target: TARGET, error = %e, "kmsg follow: read error, stopping");
                return;
            }
        }
    }

    warn!(target: TARGET, "kmsg follow: stream ended");
}

fn emit(message: &str, level: Option<LogLevel>) {
    match level {
        Some(LogLevel::Emergency | LogLevel::Alert | LogLevel::Critical | LogLevel::Error) => {
            error!(target: TARGET, "{message}");
        }
        Some(LogLevel::Warning) => {
            warn!(target: TARGET, "{message}");
        }
        Some(LogLevel::Notice | LogLevel::Info) | None => {
            info!(target: TARGET, "{message}");
        }
        Some(LogLevel::Debug) => {
            debug!(target: TARGET, "{message}");
        }
    }
}
