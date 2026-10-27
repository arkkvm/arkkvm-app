//! PeerConnection event handler for webrtc 0.20+.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use parking_lot::RwLock;
use tracing::{debug, error, info, warn};
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCIceConnectionState,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};
use zenoh::bytes::ZBytes;

use crate::data_channel::DataChannelManager;
use crate::session::SessionRole;
use crate::web::get_global_app_state;
use crate::{audio, video, zenoh_bus};

use super::{
    MEDIA_LIFECYCLE_LOCK, attach_role_session_sinks_if_connected, cleanup_last_session_resources,
    close_removed_session, on_active_sessions_changed, send_ice_candidate,
};

/// Per-session event handler. Built before `PeerConnectionBuilder::build`, then
/// bound to the resulting `Arc<dyn PeerConnection>` via [`SessionHandler::attach_peer`].
pub struct SessionHandler {
    session_id: String,
    session_role: SessionRole,
    peer: RwLock<Option<Weak<dyn PeerConnection>>>,
    ice_connection_state: Arc<RwLock<RTCIceConnectionState>>,
}

impl SessionHandler {
    pub fn new(session_id: String, session_role: SessionRole) -> Self {
        Self {
            session_id,
            session_role,
            peer: RwLock::new(None),
            ice_connection_state: Arc::new(RwLock::new(RTCIceConnectionState::Unspecified)),
        }
    }

    pub fn attach_peer(&self, peer: Weak<dyn PeerConnection>) {
        *self.peer.write() = Some(peer);
    }

    pub fn ice_connection_state(&self) -> Arc<RwLock<RTCIceConnectionState>> {
        Arc::clone(&self.ice_connection_state)
    }

    fn peer_upgrade(&self) -> Option<Arc<dyn PeerConnection>> {
        self.peer.read().as_ref().and_then(Weak::upgrade)
    }
}

#[async_trait]
impl PeerConnectionEventHandler for SessionHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let session_id = self.session_id.clone();
        if let Err(e) = send_ice_candidate(&session_id, &event.candidate).await {
            warn!("Failed to send ICE candidate: {}", e);
        }
    }

    async fn on_ice_connection_state_change(&self, connection_state: RTCIceConnectionState) {
        *self.ice_connection_state.write() = connection_state;
        let session_id = self.session_id.clone();
        let session_role = self.session_role;
        let app_state = get_global_app_state();

        info!(
            "ICE Connection State has changed: {} for session: {}",
            connection_state, session_id
        );

        match connection_state {
            RTCIceConnectionState::Connected => {
                // Do not await media attach on the driver task — get_senders / sink
                // setup can stall RTP SenderRtp drains and cause write_sample timeouts.
                let ice_state = self.ice_connection_state.clone();
                let peer_weak = self.peer.read().clone();
                tokio::spawn(async move {
                    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                    let Some(callback_peer) = peer_weak.as_ref().and_then(Weak::upgrade) else {
                        return;
                    };
                    if !app_state.session_owns_peer(&session_id, &callback_peer).await {
                        debug!(
                            "Ignoring Connected callback from stale session {}",
                            session_id
                        );
                        return;
                    }
                    if app_state
                        .set_role_slot_if_none(session_role, session_id.clone())
                        .await
                    {
                        on_active_sessions_changed().await;
                    }
                    if !app_state.is_role_slot_holder(&session_id).await {
                        debug!(
                            "Skipping sink attach for non-slot-holder session {}",
                            session_id
                        );
                        return;
                    }
                    let current = *ice_state.read();
                    if !matches!(
                        current,
                        RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
                    ) {
                        return;
                    }
                    if let Some(session) = app_state.get_session_by_id(&session_id).await {
                        attach_role_session_sinks_if_connected(&app_state, &session, current)
                            .await;
                    }
                });
            }
            RTCIceConnectionState::Disconnected => {
                let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                let Some(callback_peer) = self.peer_upgrade() else {
                    return;
                };
                if !app_state.session_owns_peer(&session_id, &callback_peer).await {
                    debug!(
                        "Ignoring Disconnected callback from stale session {}",
                        session_id
                    );
                    return;
                }
                video::detach_if_session(session_role, &session_id).await;
                audio::detach_if_session(session_role, &session_id).await;
            }
            RTCIceConnectionState::Failed | RTCIceConnectionState::Closed => {
                let removed = {
                    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                    if let Some(peer_connection) = self.peer_upgrade() {
                        app_state
                            .remove_session_for_peer(&session_id, &peer_connection)
                            .await
                    } else {
                        None
                    }
                };
                if let Some(session) = removed {
                    info!("Removed session {} on ICE failure/close", session_id);
                    tokio::spawn(close_removed_session(session));
                    on_active_sessions_changed().await;
                    cleanup_last_session_resources(&app_state).await;
                }
            }
            _ => {}
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        info!(
            "PeerConnection state changed: {:?} for session: {}",
            state, self.session_id
        );
        // ICE Connected can fire before DTLS/SRTP is ready ("local_srtp_context is not
        // set yet"). Re-attempt sink attach once the PC is fully Connected.
        if state != RTCPeerConnectionState::Connected {
            return;
        }
        let session_id = self.session_id.clone();
        let ice_state = self.ice_connection_state.clone();
        let peer_weak = self.peer.read().clone();
        tokio::spawn(async move {
            let app_state = get_global_app_state();
            let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
            let Some(callback_peer) = peer_weak.as_ref().and_then(Weak::upgrade) else {
                return;
            };
            if !app_state.session_owns_peer(&session_id, &callback_peer).await {
                return;
            }
            if !app_state.is_role_slot_holder(&session_id).await {
                return;
            }
            let current = *ice_state.read();
            if let Some(session) = app_state.get_session_by_id(&session_id).await {
                attach_role_session_sinks_if_connected(&app_state, &session, current).await;
            }
        });
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let session_id = self.session_id.clone();
        // Must not await business work here — spawn so the driver stays responsive.
        tokio::spawn(async move {
            let label = data_channel
                .label()
                .await
                .unwrap_or_else(|_| "<unknown>".to_string());
            info!(
                "New DataChannel label='{}' id={} for session: {}",
                label,
                data_channel.id(),
                &session_id
            );
            let manager = DataChannelManager::new();
            manager.route_data_channel(data_channel, session_id).await;
        });
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        tokio::spawn(async move {
            let track_id = track.track_id().await;
            let track_kind = track.kind().await;
            info!(
                "Remote Track: {:?}, kind: {:?}",
                track_id, track_kind
            );

            use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
            let session = zenoh_bus::get_session();
            while let Some(event) = track.poll().await {
                match event {
                    TrackRemoteEvent::OnRtpPacket(rtp_packet) => match track_kind {
                        RtpCodecKind::Video => {
                            let buf = ZBytes::from(rtp_packet.payload);
                            if let Err(e) = session.put("webrtc/video/vcam", buf).await {
                                error!("Failed to send video RTP packet to zenoh: {:?}", e);
                            }
                        }
                        RtpCodecKind::Audio => {
                            let buf = ZBytes::from(rtp_packet.payload);
                            if let Err(e) = session.put("webrtc/audio/mic", buf).await {
                                error!("Failed to send audio RTP packet to zenoh: {:?}", e);
                            }
                        }
                        _ => {}
                    },
                    TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnEnding => break,
                    _ => {}
                }
            }
            info!("Remote track {:?} processing ended", track_id);
        });
    }
}
