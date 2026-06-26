use anyhow::{Result, anyhow};
use serde_json::json;
use tracing::info;
use zenoh::config::ZenohId;

static ZENOH_SESSION: once_cell::sync::OnceCell<zenoh::Session> = once_cell::sync::OnceCell::new();

pub fn get_session() -> zenoh::Session {
    ZENOH_SESSION
        .get()
        .expect("Zenoh session not initialized")
        .clone()
}

pub async fn init() -> Result<()> {
    // initiate logging
    zenoh::init_log_from_env_or("debug");

    info!("Opening session...");
    let mut config = zenoh::Config::default();

    if let Err(e) =
        config.insert_json5("listen/endpoints", r#"[]"#)
    {
        return Err(anyhow!("Failed to insert listen/endpoints: {e:?}"));
    }

    if let Err(e) = config.insert_json5("scouting/multicast/enabled", &json!(false).to_string()) {
        return Err(anyhow!("Failed to disable multicast scouting: {e:?}"));
    }

    if let Err(e) = config.insert_json5("connect/endpoints", r#"["unixsock-stream//tmp/zenoh_usb_devices.sock"]"#) {
        return Err(anyhow!("Failed to set connect/endpoints: {e:?}"));
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
    info!(
        "peers zid: {:?}",
        info.peers_zid().await.collect::<Vec<ZenohId>>()
    );

    if let Err(e) = ZENOH_SESSION.set(session) {
        return Err(anyhow!("Failed to set zenoh session: {e:?}"));
    }

    Ok(())
}
