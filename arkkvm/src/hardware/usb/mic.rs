//! Virtual microphone PCM forwarding to `arkkvm_mic` via Zenoh.
//!
//! The `arkkvm_mic` subprocess lifecycle is managed by the `usb_devices` sidecar.
//! This module queues PCM in an mpsc channel; the sender task always reads frames
//! and only publishes when both sidecar reports the process running and a Zenoh peer
//! is present on the mic session.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use prost::Message;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zenoh::bytes::ZBytes;

use crate::proto::v1::{GetMicProcessStateRequest, GetMicProcessStateResponse, MicProcessStateEvent};
use crate::zenoh_bus::{self, KEY_EVENT_MIC_PROCESS, KEY_GET_MIC_PROCESS_STATE};

const MIC_DATA_KEY: &str = "arkkvm_mic/data";
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
const ZENOH_USB_QUERY_TIMEOUT: Duration = Duration::from_secs(3);

struct MicSinkState {
    sidecar_running: AtomicBool,
    peer_present: AtomicBool,
}

impl MicSinkState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sidecar_running: AtomicBool::new(false),
            peer_present: AtomicBool::new(false),
        })
    }

    fn is_ready(&self) -> bool {
        self.sidecar_running.load(Ordering::Acquire) && self.peer_present.load(Ordering::Acquire)
    }
}

async fn probe_peers(session: &zenoh::Session) -> bool {
    let peers: Vec<_> = session.info().peers_zid().await.collect();
    debug!(peer_count = peers.len(), ?peers, "mic session peers_zid probe");
    !peers.is_empty()
}

async fn query_mic_process_running() -> Option<bool> {
    let session = zenoh_bus::get_usb_session();
    let req = GetMicProcessStateRequest {};
    let mut buf = Vec::new();
    req.encode(&mut buf).ok()?;

    let replies = session
        .get(KEY_GET_MIC_PROCESS_STATE)
        .payload(ZBytes::from(buf))
        .timeout(ZENOH_USB_QUERY_TIMEOUT)
        .await
        .ok()?;

    let reply = replies.recv_async().await.ok()?;
    let sample = reply.into_result().ok()?;
    let bytes = sample.payload().to_bytes();
    let resp = GetMicProcessStateResponse::decode(bytes.as_ref()).ok()?;
    if resp.ok {
        Some(resp.running)
    } else {
        warn!(
            "get_mic_process_state rejected: {}",
            resp.error.unwrap_or_default()
        );
        None
    }
}

async fn sync_sidecar_running(state: &MicSinkState) {
    match query_mic_process_running().await {
        Some(running) => {
            state.sidecar_running.store(running, Ordering::Release);
            info!(running, "mic process state synced from sidecar query");
        }
        None => warn!("failed to query initial mic process state from sidecar"),
    }
}

fn spawn_mic_process_subscriber(state: Arc<MicSinkState>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let session = zenoh_bus::get_usb_session();
        let subscriber = match session.declare_subscriber(KEY_EVENT_MIC_PROCESS).await {
            Ok(sub) => sub,
            Err(e) => {
                warn!("failed to subscribe mic process state: {}", e);
                return;
            }
        };

        info!("subscribed to {}", KEY_EVENT_MIC_PROCESS);
        sync_sidecar_running(&state).await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("mic process state subscriber cancelled");
                    break;
                }
                sample = subscriber.recv_async() => {
                    let sample = match sample {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("mic process state subscriber closed: {}", e);
                            break;
                        }
                    };
                    let bytes = sample.payload().to_bytes();
                    match MicProcessStateEvent::decode(bytes.as_ref()) {
                        Ok(evt) => {
                            state.sidecar_running.store(evt.running, Ordering::Release);
                        }
                        Err(e) => warn!("invalid MicProcessStateEvent: {}", e),
                    }
                }
            }
        }
    });
}

struct AudioOutputInner {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
}

pub struct AudioOutput {
    inner: Arc<AudioOutputInner>,
}

impl AudioOutput {
    pub fn new() -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let state = MicSinkState::new();
        let cancel = CancellationToken::new();
        let cancel_sub = cancel.clone();
        let cancel_sender = cancel.clone();

        spawn_mic_process_subscriber(Arc::clone(&state), cancel_sub);

        tokio::spawn(async move {
            info!("mic sender started");
            let session = zenoh_bus::get_mic_session();
            let mut retry_interval = tokio::time::interval(RETRY_INTERVAL);
            retry_interval.tick().await;
            state
                .peer_present
                .store(probe_peers(&session).await, Ordering::Release);

            loop {
                tokio::select! {
                    _ = cancel_sender.cancelled() => {
                        info!("mic sender cancelled");
                        break;
                    }
                    data = rx.recv() => {
                        let Some(data) = data else {
                            break;
                        };
                        if state.is_ready() {
                            let value = ZBytes::from(data);
                            if let Err(e) = session.put(MIC_DATA_KEY, value).await {
                                warn!("mic data put failed: {}", e);
                                state.peer_present.store(false, Ordering::Release);
                            }
                        }
                    }
                    _ = retry_interval.tick() => {
                        let present = probe_peers(&session).await;
                        state.peer_present.store(present, Ordering::Release);
                    }
                }
            }
            info!("mic sender stopped");
        });

        Self {
            inner: Arc::new(AudioOutputInner { tx, cancel }),
        }
    }

    pub fn shutdown(&self) {
        self.inner.cancel.cancel();
    }

    pub fn send_data(&self, data: &[u8]) {
        if let Err(e) = self.inner.tx.try_send(data.to_vec()) {
            match e {
                TrySendError::Full(_) => {}
                TrySendError::Closed(_) => {
                    warn!("mic sender channel closed");
                }
            }
        }
    }
}
