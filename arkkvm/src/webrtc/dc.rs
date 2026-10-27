//! DataChannel event-loop helpers for webrtc 0.20+.
//!
//! 0.20 replaced `on_open` / `on_message` / `on_close` callbacks with
//! `DataChannel::poll()`. Every channel — remote (`on_data_channel`) or local
//! (`create_data_channel`) — must run a poll loop or it will never see events.
//!
//! Only one task may call `poll()` on a given channel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelMessage, RTCDataChannelState};

/// Callbacks for a DataChannel poll loop. All fields are optional.
#[derive(Default)]
pub struct DcCallbacks {
    pub on_open: Option<Box<dyn FnOnce() + Send + 'static>>,
    pub on_message: Option<Box<dyn Fn(RTCDataChannelMessage) + Send + Sync + 'static>>,
    pub on_close: Option<Box<dyn FnOnce() + Send + 'static>>,
    pub on_buffered_amount_low: Option<Box<dyn Fn() + Send + Sync + 'static>>,
}

/// Spawn a background task that drains `channel.poll()` until close/error.
pub fn spawn_dc_events(channel: Arc<dyn DataChannel>, mut cbs: DcCallbacks) {
    tokio::spawn(async move {
        let label = channel
            .label()
            .await
            .unwrap_or_else(|_| "<unknown>".to_string());
        let id = channel.id();
        debug!("DC poll loop started label={} id={}", label, id);

        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    if let Some(cb) = cbs.on_open.take() {
                        cb();
                    }
                }
                DataChannelEvent::OnMessage(msg) => {
                    if let Some(ref cb) = cbs.on_message {
                        cb(msg);
                    }
                }
                DataChannelEvent::OnBufferedAmountLow => {
                    if let Some(ref cb) = cbs.on_buffered_amount_low {
                        cb();
                    }
                }
                DataChannelEvent::OnClose | DataChannelEvent::OnClosing => {
                    if let Some(cb) = cbs.on_close.take() {
                        cb();
                    }
                    break;
                }
                DataChannelEvent::OnError => {
                    warn!("DC error on label={} id={}", label, id);
                    if let Some(cb) = cbs.on_close.take() {
                        cb();
                    }
                    break;
                }
                DataChannelEvent::OnBufferedAmountHigh => {}
            }
        }
        // poll() ended without OnClose/OnError — still run close callback once.
        if let Some(cb) = cbs.on_close.take() {
            cb();
        }
        debug!("DC poll loop ended label={} id={}", label, id);
    });
}

/// Wait until `ready_state` is Open. Does **not** call `poll()` (so it never
/// steals events from the channel's dedicated poll loop).
pub async fn wait_dc_open(
    channel: &Arc<dyn DataChannel>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        match channel.ready_state().await {
            Ok(RTCDataChannelState::Open) => return Ok(()),
            Ok(RTCDataChannelState::Closed) => {
                return Err(anyhow::anyhow!("data channel closed before open"));
            }
            Ok(_) | Err(_) => {}
        }
        if start.elapsed() >= timeout {
            return Err(anyhow::anyhow!("timed out waiting for data channel open"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
