use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::time::sleep;
use tracing::{info, warn};
use zenoh::config::ZenohId;

static ZENOH_SESSION: once_cell::sync::OnceCell<zenoh::Session> = once_cell::sync::OnceCell::new();

/// Fixed connect retry so sock appearance is noticed within arkkvm's init sync window.
const CONNECT_RETRY_PERIOD_MS: i64 = 100;
/// How often to warn while still waiting for a peer.
const PEER_WAIT_WARN_INTERVAL: Duration = Duration::from_secs(2);
const PEER_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn get_session() -> zenoh::Session {
    session_from(&ZENOH_SESSION).expect("Zenoh session not initialized")
}

pub fn try_get_session() -> Result<zenoh::Session> {
    session_from(&ZENOH_SESSION)
}

fn session_from(cell: &once_cell::sync::OnceCell<zenoh::Session>) -> Result<zenoh::Session> {
    cell.get()
        .cloned()
        .ok_or_else(|| anyhow!("Zenoh session not initialized"))
}

pub async fn init() -> Result<()> {
    // initiate logging
    zenoh::init_log_from_env_or("debug");

    info!("Opening session...");
    let mut config = zenoh::Config::default();

    if let Err(e) = config.insert_json5("listen/endpoints", r#"[]"#) {
        return Err(anyhow!("Failed to insert listen/endpoints: {e:?}"));
    }

    if let Err(e) = config.insert_json5("scouting/multicast/enabled", &json!(false).to_string()) {
        return Err(anyhow!("Failed to disable multicast scouting: {e:?}"));
    }

    if let Err(e) =
        config.insert_json5("connect/endpoints", r#"["unixsock-stream//tmp/zenoh_usb_devices.sock"]"#)
    {
        return Err(anyhow!("Failed to set connect/endpoints: {e:?}"));
    }

    // Override default 1s→4s exponential backoff; sock must be noticed within ~100ms.
    let retry = format!(
        r#"{{ period_init_ms: {}, period_max_ms: {}, period_increase_factor: 1.0 }}"#,
        CONNECT_RETRY_PERIOD_MS, CONNECT_RETRY_PERIOD_MS
    );
    if let Err(e) = config.insert_json5("connect/retry", &retry) {
        return Err(anyhow!("Failed to set connect/retry: {e:?}"));
    }

    if let Err(e) = config.insert_json5("transport/shared_memory/enabled", &json!(true).to_string())
    {
        return Err(anyhow!("Failed to insert shared memory config: {e:?}"));
    }
    let session = zenoh::open(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to open zenoh session: {}", e))?;

    let info = session.info();
    info!("zid: {}", info.zid().await);
    info!(
        "routers zid: {:?}",
        info.routers_zid().await.collect::<Vec<ZenohId>>()
    );

    wait_until_peer_connected(&session).await;

    if let Err(e) = ZENOH_SESSION.set(session) {
        return Err(anyhow!("Failed to set zenoh session: {e:?}"));
    }

    Ok(())
}

async fn wait_until_peer_connected(session: &zenoh::Session) {
    let info = session.info();
    let mut waited = Duration::ZERO;
    let mut since_warn = Duration::ZERO;

    loop {
        let peers: Vec<ZenohId> = info.peers_zid().await.collect();
        if !peers.is_empty() {
            info!(
                peers = ?peers,
                waited_ms = waited.as_millis(),
                "usb_devices zenoh peer connected"
            );
            return;
        }

        if since_warn >= PEER_WAIT_WARN_INTERVAL {
            warn!(
                waited_ms = waited.as_millis(),
                endpoint = "/tmp/zenoh_usb_devices.sock",
                "usb_devices waiting for zenoh peer (arkkvm listen not ready yet)"
            );
            since_warn = Duration::ZERO;
        }

        sleep(PEER_WAIT_POLL_INTERVAL).await;
        waited += PEER_WAIT_POLL_INTERVAL;
        since_warn += PEER_WAIT_POLL_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use once_cell::sync::OnceCell;

    #[test]
    fn session_lookup_before_initialization_returns_error() {
        let cell = OnceCell::<zenoh::Session>::new();

        let result = super::session_from(&cell);

        assert!(result.is_err());
    }
}
