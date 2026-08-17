use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Context;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine,
};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use zenoh::bytes::ZBytes;

use crate::data_channel::DataChannelManager;
use crate::hardware::usb::storage as storage_mod;
use crate::jsonrpc::PROCESSOR;
use crate::session::Session;
use crate::web::get_global_app_state;
use crate::{audio, video, zenoh_bus};

static MEDIA_LIFECYCLE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
const PEER_CONNECTION_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn wait_for_background_cleanup<F, T>(
    cleanup: F,
    wait_timeout: std::time::Duration,
) -> Option<Result<T, tokio::task::JoinError>>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut task = tokio::spawn(cleanup);
    tokio::time::timeout(wait_timeout, &mut task).await.ok()
}

pub(crate) async fn close_peer_connection(
    peer_connection: Arc<webrtc::peer_connection::RTCPeerConnection>,
    session_id: &str,
) {
    match wait_for_background_cleanup(
        async move { peer_connection.close().await },
        PEER_CONNECTION_CLOSE_TIMEOUT,
    )
    .await
    {
        Some(Ok(Ok(()))) => info!("Closed peer connection for session {}", session_id),
        Some(Ok(Err(e))) => {
            warn!("Failed to close peer connection for session {}: {}", session_id, e)
        }
        Some(Err(e)) => warn!(
            "Peer connection cleanup task failed for session {}: {}",
            session_id, e
        ),
        None => warn!(
            "Peer connection cleanup for session {} is still running after {:?}",
            session_id, PEER_CONNECTION_CLOSE_TIMEOUT
        ),
    }
}

async fn close_removed_session(session: Arc<Session>) {
    if let Some(peer_connection) = session.peer_connection.clone() {
        close_peer_connection(peer_connection, &session.id).await;
    }
}

async fn cleanup_last_session_resources(app_state: &crate::state::AppState) {
    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
    if app_state.session_count().await != 0 {
        return;
    }

    info!("Detaching video and audio sinks after last WebRTC session closed");
    video::detach_webrtc_sink().await;
    audio::detach_webrtc_sink().await;

    if let Some(st) = storage_mod::get_virtual_media_state()
        && matches!(st.source, storage_mod::VirtualMediaSource::WebRTC)
    {
        if let Err(e) = storage_mod::unmount_image().await {
            warn!("failed to auto unmount WebRTC media: {}", e);
        } else {
            info!("auto unmounted WebRTC virtual media on last session close");
        }
    }
}

async fn attach_current_session_sinks_if_connected(
    app_state: &crate::state::AppState,
    session_id: &str,
    connection_state: RTCIceConnectionState,
) {
    if app_state.get_current_session_id().await.as_deref() != Some(session_id) {
        return;
    }
    let Some(session) = app_state.get_session_by_id(session_id).await else {
        return;
    };
    if !matches!(
        connection_state,
        RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
    ) {
        return;
    }

    if let Some(track) = session.video_track.clone() {
        video::attach_webrtc_sink(track).await;
        info!("Video track attached to WebRTC session {}", session_id);
    }
    if let Some(track) = session.audio_track.clone() {
        audio::attach_webrtc_sink(track).await;
        info!("Audio track attached to WebRTC session {}", session_id);
    }
}

pub(crate) async fn remove_and_close_session(
    app_state: &crate::state::AppState,
    session_id: &str,
) -> bool {
    let Some(session) = app_state.remove_session(session_id).await else {
        return false;
    };

    close_removed_session(session).await;
    cleanup_last_session_resources(app_state).await;
    true
}

async fn remove_and_close_session_for_peer(
    app_state: &crate::state::AppState,
    session_id: &str,
    peer_connection: &Arc<webrtc::peer_connection::RTCPeerConnection>,
) -> bool {
    let Some(session) = app_state.remove_session_for_peer(session_id, peer_connection).await else {
        return false;
    };

    close_removed_session(session).await;
    cleanup_last_session_resources(app_state).await;
    true
}

/// Session configuration for WebRTC connections
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub ice_servers: Option<Vec<String>>,
    pub local_ip: Option<IpAddr>,
    pub is_cloud: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ice_servers: None,
            local_ip: None,
            is_cloud: false,
        }
    }
}

/// WebRTC API instance shared across sessions
pub struct WebRTCApi;

impl WebRTCApi {
    /// Create new WebRTC API instance
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    /// Create new WebRTC session with the given configuration
    pub async fn new_session(
        &self,
        config: SessionConfig,
        session_id: String,
    ) -> anyhow::Result<Arc<Session>> {
        let setting_engine = SettingEngine::default();
        let mut ice_servers = vec![];

        info!("Creating new WebRTC session with id: {}", &session_id);

        if config.is_cloud {
            if let Some(servers) = &config.ice_servers {
                ice_servers = servers
                    .iter()
                    .map(|url| webrtc::ice_transport::ice_server::RTCIceServer {
                        urls: vec![url.clone()],
                        ..Default::default()
                    })
                    .collect();
                info!("Using ICE servers provided by cloud: {:?}", servers);
            } else {
                info!("ICE servers not provided by cloud");
            }

            // if let Some(local_ip) = config.local_ip {
            //     setting_engine.set_nat_1to1_ips(
            //         vec![local_ip.to_string()],
            //         webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType::Srflx,
            //     );
            //     info!("Setting NAT 1-to-1 IPs with local IP: {}", local_ip);
            // } else {
            //     info!("Local IP address not provided, won't set NAT 1-to-1 IPs");
            // }
        }

        let audio_channels = 2u16; // Stereo
        let audio_rate = audio::get_webrtc_clock_rate();

        // Build API with media engine (codecs) and default interceptors
        let mut media_engine = MediaEngine::default();
        // media_engine.register_default_codecs().context("register_default_codecs failed")?;
        self.set_video_codec(&mut media_engine)?;
        self.set_audio_codec(&mut media_engine, audio_rate, audio_channels)?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .context("register_default_interceptors failed")?;

        let api = APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .build();

        let configuration = RTCConfiguration { ice_servers, ..Default::default() };

        let peer_connection = Arc::new(api.new_peer_connection(configuration).await?);

        // Create video track
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability { mime_type: MIME_TYPE_H264.to_owned(), ..Default::default() },
            "video".to_owned(),
            "arkkvm".to_owned(),
        ));

        info!("Created video track: kind={}", video_track.kind());

        // Add video track to peer connection
        let _video_rtp_sender = match peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
        {
            Ok(sender) => sender,
            Err(e) => {
                close_peer_connection(peer_connection.clone(), &session_id).await;
                return Err(e.into());
            }
        };

        info!("Added video track to peer connection");

        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: audio_rate,
                channels: audio_channels,
                ..Default::default()
            },
            "audio".to_owned(),
            "arkkvm".to_owned(),
        ));

        info!("Created audio track: kind={}", audio_track.kind());

        let _audio_rtp_sender = match peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
        {
            Ok(sender) => sender,
            Err(e) => {
                close_peer_connection(peer_connection.clone(), &session_id).await;
                return Err(e.into());
            }
        };

        info!("Added audio track to peer connection");

        // Verify track was added by checking senders immediately
        let senders = peer_connection.get_senders().await;
        info!("Peer connection senders after adding video track: {}", senders.len());
        for (i, sender) in senders.iter().enumerate() {
            if let Some(track) = sender.track().await {
                info!("Sender {}: track_id={}, kind={}", i, track.id(), track.kind());
            } else {
                info!("Sender {}: no track", i);
            }
        }

        // Start video RTCP reading task
        // let video_rtp_sender_clone = Arc::clone(&video_rtp_sender);
        // tokio::spawn(async move {
        //     let mut rtcp_buf = vec![0u8; 1500];
        //     while let Ok((packet, attributes)) = video_rtp_sender_clone.read(&mut rtcp_buf).await {
        //         debug!("Received Video RTCP packet: {:?}, attributes: {:?}", packet, attributes);
        //     }
        // });

        // Start video RTCP reading task
        // let audio_rtp_sender_clone = Arc::clone(&audio_rtp_sender);
        // tokio::spawn(async move {
        //     let mut rtcp_buf = vec![0u8; 1500];
        //     while let Ok((packet, attributes)) = audio_rtp_sender_clone.read(&mut rtcp_buf).await {
        //         debug!("Received Audio RTCP packet: {:?}, attributes: {:?}", packet, attributes);
        //     }
        // });

        let mut session = Session::new_with_cloud(session_id, config.is_cloud);
        session.peer_connection = Some(Arc::clone(&peer_connection));
        session.video_track = Some(video_track);
        session.audio_track = Some(audio_track);

        // Set up connection state change handler
        let session_id_clone = session.id.clone();
        let peer_connection_for_state_callback = Arc::downgrade(&peer_connection);
        let video_track_clone = session.video_track.clone();

        peer_connection.on_ice_connection_state_change(Box::new(
            move |connection_state: RTCIceConnectionState| {
                let session_id = session_id_clone.clone();
                let video_track = video_track_clone.clone();
                let peer_connection = peer_connection_for_state_callback.clone();
                let app_state = get_global_app_state();
                Box::pin(async move {
                    info!(
                        "ICE Connection State has changed: {} for session: {}",
                        connection_state, session_id
                    );

                    match connection_state {
                        RTCIceConnectionState::Connected => {
                            let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                            let Some(callback_peer) = peer_connection.upgrade() else {
                                return;
                            };
                            if !app_state.session_owns_peer(&session_id, &callback_peer).await {
                                debug!("Ignoring Connected callback from stale session {}", session_id);
                                return;
                            }
                            // Signaling normally publishes the current session before ICE
                            // connects. Only fill an empty slot so a slower old connection
                            // cannot overwrite a newer takeover.
                            if app_state.set_current_session_if_none(session_id.clone()).await {
                                on_active_sessions_changed().await;
                            }
                            if app_state.get_current_session_id().await.as_deref()
                                != Some(session_id.as_str())
                            {
                                debug!("Skipping sink attach for non-current session {}", session_id);
                                return;
                            }

                            attach_current_session_sinks_if_connected(
                                &app_state,
                                &session_id,
                                connection_state,
                            )
                            .await;
                        }

                        RTCIceConnectionState::Disconnected => {
                            let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                            let Some(callback_peer) = peer_connection.upgrade() else {
                                return;
                            };
                            if !app_state.session_owns_peer(&session_id, &callback_peer).await {
                                debug!("Ignoring Disconnected callback from stale session {}", session_id);
                                return;
                            }
                            // Keep the current marker during a transient disconnect so
                            // takeover can still identify and close this predecessor.
                            // Do not unmount virtual media here — ICE may recover to Connected.
                            if app_state.get_current_session_id().await.as_deref()
                                != Some(session_id.as_str())
                            {
                                return;
                            }

                            if let Some(track) = video_track.clone() {
                                if video::equal_webrtc_sink(track).await {
                                    info!("Detaching video and audio sinks on ICE Disconnected");
                                    video::detach_webrtc_sink().await;
                                    audio::detach_webrtc_sink().await;
                                }
                            }
                        }

                        RTCIceConnectionState::Failed | RTCIceConnectionState::Closed => {
                            let removed = {
                                let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
                                if let Some(peer_connection) = peer_connection.upgrade() {
                                    app_state
                                        .remove_session_for_peer(&session_id, &peer_connection)
                                        .await
                                } else {
                                    None
                                }
                            };

                            // A failed ICE transport does not stop the peer connection's
                            // RTCP/interceptor tasks by itself. Explicitly close it after
                            // removing the session so failure callbacks cannot recurse.
                            if let Some(session) = removed {
                                info!("Removed session {} on ICE failure/close", session_id);
                                tokio::spawn(close_removed_session(session));
                                on_active_sessions_changed().await;
                                // Only the callback that removed the session owns teardown.
                                // close() may emit Closed after Failed and must not repeat it.
                                cleanup_last_session_resources(&app_state).await;
                            }
                        }
                        _ => {}
                    }
                })
            },
        ));

        // Set up ICE candidate handler
        let session_id_clone = session.id.clone();
        peer_connection.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let session_id = session_id_clone.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Err(e) = send_ice_candidate(&session_id, &candidate).await {
                        warn!("Failed to send ICE candidate: {}", e);
                    }
                }
            })
        }));

        // Set up data channel handler with proper routing
        let session_id_clone = session.id.clone();
        peer_connection.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
            let session_id = session_id_clone.clone();
            Box::pin(async move {
                info!(
                    "New DataChannel label='{}' id={} for session: {}",
                    data_channel.label(),
                    data_channel.id(),
                    &session_id
                );
                let manager = DataChannelManager::new();
                manager.route_data_channel(data_channel, session_id).await;
            })
        }));

        peer_connection.on_track(Box::new(|track, _, _| {
            info!("Got Remote Track: {:?}", track);
            Box::pin(async move {
                let track = track.clone();
                tokio::spawn(async move {
                    let track_id = track.id();
                    let track_kind = track.kind();
                    info!("Thread Remote Track: {}, kind: {:?}", track_id.as_str(), track_kind);

                    let mut buffer = vec![0u8; 1500];
                    match track_kind {
                        RTPCodecType::Video => {
                            info!("Starting video track processing for track: {}, codec: {:?}", track_id, track.codec());
                            let session = zenoh_bus::get_session();
                            while let Ok((rtp_packet, _)) = track.read(&mut buffer).await {
                                debug!("Received video RTP packet: payload_type={}, sequence_number={}, timestamp={}",
                                       rtp_packet.header.payload_type,
                                       rtp_packet.header.sequence_number,
                                       rtp_packet.header.timestamp);

                                // Send video RTP packet to zenoh bus
                                let buf = ZBytes::from(rtp_packet.payload);
                                if let Err(e) = session.put("webrtc/video/vcam", buf).await {
                                    error!("Failed to send video RTP packet to zenoh: {:?}", e);
                                };
                            }
                            info!("Video track {} processing ended", track_id);
                        }

                        RTPCodecType::Audio => {
                            info!("Starting audio track processing for track: {}, codec: {:?}", track_id, track.codec());
                            let session = zenoh_bus::get_session();
                            while let Ok((rtp_packet, _)) = track.read(&mut buffer).await {
                                debug!("Received audio RTP packet: {:?}", &rtp_packet.header);
                                let buf = ZBytes::from(rtp_packet.payload);
                                if let Err(e) = session.put("webrtc/audio/mic", buf).await {
                                    error!("Failed to send audio RTP packet to zenoh: {:?}", e);
                                };
                            }
                            info!("Audio track {} processing ended", track_id);
                        }

                        _ => {
                            warn!("Received unknown track type: {:?} for track: {}", track_kind, track_id);
                            while let Ok((rtp_packet, _)) = track.read(&mut buffer).await {
                                debug!("Received unknown RTP packet: payload_type={}, sequence_number={}",
                                       rtp_packet.header.payload_type,
                                       rtp_packet.header.sequence_number);
                            }
                        }
                    }
                });
            })
        }));

        let session = Arc::new(session);
        if let Some(previous) = get_global_app_state().add_session(session.clone()).await {
            // Old generation is owned here for close. Do not use remove_and_close:
            // the map still holds this session id, so last-session media cleanup
            // must not run.
            tokio::spawn(close_removed_session(previous));
        }
        Ok(session)
    }

    fn set_video_codec(&self, engine: &mut MediaEngine) -> anyhow::Result<()> {
        let video_rtcp_feedback = vec![
            RTCPFeedback { typ: "goog-remb".to_owned(), parameter: "".to_owned() },
            RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: "".to_owned() },
            RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
        ];

        for codec in vec![
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 96,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP9.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "profile-id=0".to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 98,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP9.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "profile-id=1".to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 100,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 102,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 127,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 125,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 108,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 127,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"
                            .to_owned(),
                    rtcp_feedback: video_rtcp_feedback.clone(),
                },
                payload_type: 123,
                ..Default::default()
            },
            // RTCRtpCodecParameters {
            //     capability: RTCRtpCodecCapability {
            //         mime_type: MIME_TYPE_HEVC.to_owned(),
            //         clock_rate: 90000,
            //         channels: 0,
            //         sdp_fmtp_line: "".to_owned(),
            //         rtcp_feedback: video_rtcp_feedback,
            //     },
            //     payload_type: 126,
            //     ..Default::default()
            // },
        ] {
            engine.register_codec(codec, RTPCodecType::Video)?;
        }
        Ok(())
    }

    fn set_audio_codec(
        &self,
        engine: &mut MediaEngine,
        webrtc_clock_rate: u32,
        audio_channels: u16,
    ) -> anyhow::Result<()> {
        for codec in vec![RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: webrtc_clock_rate,
                channels: audio_channels,
                // sdp_fmtp_line: "minptime=20;useinbandfec=1".to_owned(),
                sdp_fmtp_line: "minptime=20".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        }] {
            engine.register_codec(codec, RTPCodecType::Audio)?;
        }
        Ok(())
    }
}

/// Global WebRTC API instance
static WEBRTC_API: tokio::sync::OnceCell<WebRTCApi> = tokio::sync::OnceCell::const_new();

/// Initialize global WebRTC API
pub async fn init_webrtc_api() -> anyhow::Result<()> {
    let api = WebRTCApi::new()?;
    WEBRTC_API.set(api).map_err(|_| anyhow::anyhow!("WebRTC API already initialized"))?;
    info!("WebRTC API initialized");
    Ok(())
}

/// Get global WebRTC API instance
pub async fn get_webrtc_api() -> &'static WebRTCApi {
    WEBRTC_API.get().expect("WebRTC API not initialized")
}

/// Trigger video state update
pub async fn trigger_video_state_update() {
    info!("Triggering video state update");
    // Call video module's internal update function
    video::trigger_video_state_update_rpc().await;
}

/// Trigger USB state update
pub async fn trigger_usb_state_update() {
    info!("Triggering USB state update");

    let usb_state = crate::services::usb::ensure_usb_state().await;
    crate::jsonrpc::broadcast_usb_state(usb_state).await;

    if let Some(session) = get_global_app_state().get_current_session().await {
        let usb_devices = crate::jsonrpc::handlers::get_usb_devices().await;
        let usb_emulation = match crate::services::get_usb() {
            Some(usb) => usb.get_usb_emulation_state().await.unwrap_or(false),
            None => false,
        };

        let params = serde_json::json!({
            "devices": usb_devices,
            "emulationEnabled": usb_emulation
        });

        if let Err(e) = PROCESSOR.send_event("usbStateChanged", Some(params), session).await {
            warn!("Failed to send USB state update: {}", e);
        }
    }
}

/// Trigger keyboard LED state update from cached sidecar state
pub async fn trigger_keyboard_led_state_update() {
    info!("Triggering keyboard LED state update");
    let state = crate::jsonrpc::handlers::get_keyboard_led_state().unwrap_or_default();
    crate::jsonrpc::broadcast_keyboard_led_state(state).await;
}

/// Handle first session connected
pub async fn on_first_session_connected() {
    const MEDIA_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;

    info!("First WebRTC session connected - starting media");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    if get_global_app_state().session_count().await == 0 {
        debug!("Skip delayed media start: no active sessions");
        return;
    }

    match tokio::time::timeout(MEDIA_START_TIMEOUT, video::start_native_video()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("Failed to start native video: {}", e),
        Err(_) => error!(
            "Timed out starting native video after {:?}",
            MEDIA_START_TIMEOUT
        ),
    }

    match tokio::time::timeout(MEDIA_START_TIMEOUT, audio::start_native_audio()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("Failed to start native audio: {}", e),
        Err(_) => warn!(
            "Timed out starting native audio after {:?}",
            MEDIA_START_TIMEOUT
        ),
    }
}

/// Handle last session disconnected
pub async fn on_last_session_disconnected() {
    const MEDIA_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;

    if get_global_app_state().session_count().await > 0 {
        debug!("Skip media stop: active sessions still present");
        return;
    }

    info!("Last WebRTC session disconnected - stopping media");
    if tokio::time::timeout(MEDIA_STOP_TIMEOUT, video::stop_native_video()).await.is_err() {
        error!(
            "Timed out stopping native video after {:?}",
            MEDIA_STOP_TIMEOUT
        );
    }
    if tokio::time::timeout(MEDIA_STOP_TIMEOUT, audio::stop_native_audio()).await.is_err() {
        error!(
            "Timed out stopping native audio after {:?}",
            MEDIA_STOP_TIMEOUT
        );
    }
}

/// Handle session count change
pub async fn on_active_sessions_changed() {
    let app_state = get_global_app_state();
    let count = app_state.session_count().await;
    info!("Active sessions count changed to: {}", count);

    if let Some(session) = app_state.get_current_session().await {
        let params = serde_json::json!({
            "activeSessionCount": count,
            "hasActiveSessions": count > 0
        });

        if let Err(e) = PROCESSOR.send_event("sessionCountChanged", Some(params), session).await {
            warn!("Failed to send session count update: {}", e);
        }
    }
}

/// Get current session ID
pub async fn get_current_session() -> Option<String> {
    // CURRENT_SESSION.read().await.clone()
    get_global_app_state().get_current_session_id().await
}

/// Set current session ID
pub async fn set_current_session(session_id: Option<String>) {
    // *CURRENT_SESSION.write().await = session_id.clone();
    get_global_app_state().set_current_session_id(session_id).await;
    on_active_sessions_changed().await;
}

/// Send ICE candidate through appropriate signaling channel based on session type
async fn send_ice_candidate(
    session_id: &str,
    candidate: &RTCIceCandidate,
) -> anyhow::Result<()> {
    use serde_json::json;

    let mut candidate_init = candidate.to_json()?;
    let app_state = get_global_app_state();
    // Get session once to avoid multiple lookups and potential deadlocks
    let session_opt = app_state.get_session_by_id(session_id).await;
    let is_cloud_session = session_opt.as_ref().map(|s| s.is_cloud).unwrap_or(false);

    // Ensure usernameFragment is included in the candidate
    // Use cached usernameFragment from session to avoid blocking on local_description()
    if candidate_init.username_fragment.is_none() {
        if let Some(session) = &session_opt {
            let cached_ufrag = session.username_fragment.read().await.clone();
            if let Some(ufrag) = cached_ufrag {
                candidate_init.username_fragment = Some(ufrag);
            }
        }
    }

    // Serialize candidate_init to JSON for sending
    let candidate_json = serde_json::to_value(&candidate_init)?;
    let message = json!({
        "type": "new-ice-candidate",
        "data": candidate_json
    });

    if is_cloud_session {
        // Cloud session: send via cloud WebSocket
        if let Err(e) = crate::cloud::websocket::cloud_ws_send_json(&message).await {
            warn!("Failed to send ICE candidate to cloud for session {}: {}", session_id, e);
            return Err(e);
        }
    } else {
        // Local session: try Socket.IO first, then fall back to local WebSocket queue
        if let Some(socket) = app_state.sockets.read().await.get(session_id) {
            if let Err(_) = socket.emit("ice-candidate", &message) {
                app_state.queue_ice_candidate(session_id, message.to_string()).await;
            }
        } else {
            // No Socket.IO connection, queue for local WebSocket
            app_state.queue_ice_candidate(session_id, message.to_string()).await;
        }
    }

    Ok(())
}

/// Handle session takeover: send otherSessionConnected to old session and close it after delay
pub async fn handle_session_takeover(
    app_state: std::sync::Arc<crate::state::AppState>,
    new_session: Arc<Session>,
) -> anyhow::Result<()> {
    let new_session_id = new_session.id.clone();
    let maybe_old = {
        let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
        let previous = app_state
            .promote_session_if_registered(&new_session)
            .await
            .map_err(|()| anyhow::anyhow!("session {} is no longer registered", new_session_id))?;
        on_active_sessions_changed().await;
        let connection_state = app_state
            .get_session_by_id(&new_session_id)
            .await
            .and_then(|registered| registered.peer_connection.clone())
            .map(|peer| peer.ice_connection_state())
            .unwrap_or(RTCIceConnectionState::Unspecified);
        attach_current_session_sinks_if_connected(
            &app_state,
            &new_session_id,
            connection_state,
        )
        .await;
        previous
    };

    if let Some(old_session) = maybe_old
        && !Arc::ptr_eq(&old_session, &new_session)
    {
        let old_id = old_session.id.clone();
        // Capture the old peer generation now. The delayed cleanup must not
        // close a replacement that later reuses the same signaling ID.
        if let Err(e) = PROCESSOR
            .send_event("otherSessionConnected", None, old_session.clone())
            .await
        {
            tracing::warn!("Failed to send otherSessionConnected to session {}: {}", old_id, e);
        } else {
            tracing::info!("Sent otherSessionConnected to old session {}", old_id);
        }

        if let Some(old_peer) = old_session.peer_connection.clone() {
            tokio::spawn(close_previous_session_after_delay(
                app_state.clone(),
                old_id,
                old_peer,
                tokio::time::Duration::from_secs(1),
            ));
        }
    }

    Ok(())
}

async fn close_previous_session_after_delay(
    app_state: Arc<crate::state::AppState>,
    old_id: String,
    old_peer: Arc<webrtc::peer_connection::RTCPeerConnection>,
    delay: tokio::time::Duration,
) {
    tokio::time::sleep(delay).await;
    if remove_and_close_session_for_peer(&app_state, &old_id, &old_peer).await {
        tracing::info!("Closed previous session {}", old_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::{Barrier, Notify, oneshot};
    use webrtc::api::APIBuilder;
    use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

    #[tokio::test]
    async fn concurrent_session_cleanup_closes_registered_peer_once() {
        let api = APIBuilder::new().build();
        let peer_connection = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("peer connection should be created"),
        );
        let app_state = crate::state::AppState::new();
        let mut session = crate::session::Session::new("test-session".to_string());
        session.peer_connection = Some(peer_connection.clone());
        {
            let mut sessions = app_state.sessions.write().await;
            sessions.insert(session.id.clone(), Arc::new(session));
            sessions.insert(
                "sentinel-session".to_string(),
                Arc::new(crate::session::Session::new("sentinel-session".to_string())),
            );
        }
        let (first_removal, second_removal) = tokio::join!(
            super::remove_and_close_session(&app_state, "test-session"),
            super::remove_and_close_session(&app_state, "test-session"),
        );

        assert_ne!(first_removal, second_removal);
        assert_eq!(
            peer_connection.connection_state(),
            RTCPeerConnectionState::Closed
        );
    }

    #[tokio::test]
    async fn same_id_replace_returns_previous_for_close() {
        let api = APIBuilder::new().build();
        let old_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("old peer connection should be created"),
        );
        let new_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("new peer connection should be created"),
        );
        let app_state = crate::state::AppState::new();

        let mut old_session = crate::session::Session::new("reused-id".to_string());
        old_session.peer_connection = Some(old_peer.clone());
        let old_session = Arc::new(old_session);
        assert!(app_state.add_session(old_session.clone()).await.is_none());
        app_state.set_current_session_id(Some("reused-id".to_string())).await;

        let mut replacement = crate::session::Session::new("reused-id".to_string());
        replacement.peer_connection = Some(new_peer.clone());
        let replacement = Arc::new(replacement);
        let previous = app_state
            .add_session(replacement.clone())
            .await
            .expect("same-id replace should return the previous session");
        assert!(Arc::ptr_eq(&previous, &old_session));
        assert_eq!(
            app_state.get_current_session_id().await.as_deref(),
            Some("reused-id"),
            "same-id replace must keep the current-session marker"
        );

        let registered = app_state
            .get_session_by_id("reused-id")
            .await
            .expect("replacement should be registered");
        assert!(Arc::ptr_eq(&registered, &replacement));
        assert!(!app_state.session_owns_peer("reused-id", &old_peer).await);
        assert!(app_state.session_owns_peer("reused-id", &new_peer).await);

        super::close_peer_connection(old_peer, "reused-id-old").await;
        super::close_peer_connection(new_peer, "reused-id-new").await;
    }

    #[tokio::test]
    async fn mismatched_peer_callback_cannot_remove_replaced_session() {
        let api = APIBuilder::new().build();
        let active_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("active peer connection should be created"),
        );
        let stale_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("stale peer connection should be created"),
        );
        let app_state = crate::state::AppState::new();

        let mut active_session = crate::session::Session::new("reused-id".to_string());
        active_session.peer_connection = Some(active_peer.clone());
        let active_session = Arc::new(active_session);
        {
            let mut sessions = app_state.sessions.write().await;
            sessions.insert(active_session.id.clone(), active_session.clone());
            sessions.insert(
                "sentinel-session".to_string(),
                Arc::new(crate::session::Session::new("sentinel-session".to_string())),
            );
        }
        app_state.set_current_session_id(Some("reused-id".to_string())).await;

        assert!(
            app_state
                .remove_session_for_peer("reused-id", &stale_peer)
                .await
                .is_none()
        );
        assert_eq!(
            app_state.get_current_session_id().await.as_deref(),
            Some("reused-id"),
            "a mismatched callback must not clear the replacement's current marker"
        );

        let registered = app_state
            .get_session_by_id("reused-id")
            .await
            .expect("active session should remain registered");
        assert!(Arc::ptr_eq(&registered, &active_session));
        assert!(
            app_state
                .remove_session_for_peer("reused-id", &active_peer)
                .await
                .is_some()
        );
        assert!(
            app_state
                .remove_session_for_peer("reused-id", &active_peer)
                .await
                .is_none(),
            "a Failed callback followed by Closed must have only one cleanup owner"
        );

        super::close_peer_connection(active_peer, "reused-id").await;
        super::close_peer_connection(stale_peer, "reused-id-stale").await;
    }

    #[tokio::test]
    async fn concurrent_current_session_swaps_preserve_every_predecessor() {
        let app_state = crate::state::AppState::new();
        let original = Arc::new(crate::session::Session::new("original".to_string()));
        let first = Arc::new(crate::session::Session::new("first".to_string()));
        let second = Arc::new(crate::session::Session::new("second".to_string()));
        {
            let mut sessions = app_state.sessions.write().await;
            sessions.insert(original.id.clone(), original);
            sessions.insert(first.id.clone(), first.clone());
            sessions.insert(second.id.clone(), second.clone());
        }
        app_state.set_current_session_id(Some("original".to_string())).await;

        let (first_previous, second_previous) = tokio::join!(
            app_state.promote_session_if_registered(&first),
            app_state.promote_session_if_registered(&second),
        );

        let mut observed = vec![
            first_previous
                .expect("first promotion should succeed")
                .expect("first promotion should return a predecessor")
                .id
                .clone(),
            second_previous
                .expect("second promotion should succeed")
                .expect("second promotion should return a predecessor")
                .id
                .clone(),
            app_state
                .get_current_session_id()
                .await
                .expect("one replacement should remain current"),
        ];
        observed.sort();
        assert_eq!(observed, vec!["first", "original", "second"]);
    }

    #[tokio::test]
    async fn promotion_rejects_a_session_removed_before_takeover() {
        let app_state = crate::state::AppState::new();
        let current = Arc::new(crate::session::Session::new("current".to_string()));
        let removed_candidate = Arc::new(crate::session::Session::new("removed".to_string()));
        app_state
            .sessions
            .write()
            .await
            .insert(current.id.clone(), current);
        app_state.set_current_session_id(Some("current".to_string())).await;

        assert!(
            app_state
                .promote_session_if_registered(&removed_candidate)
                .await
                .is_err()
        );
        assert_eq!(app_state.get_current_session_id().await.as_deref(), Some("current"));
    }

    #[tokio::test]
    async fn delayed_takeover_cleanup_does_not_close_reused_session_id() {
        let api = APIBuilder::new().build();
        let old_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("old peer connection should be created"),
        );
        let replacement_peer = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("replacement peer connection should be created"),
        );
        let app_state = Arc::new(crate::state::AppState::new());

        let mut old_session = crate::session::Session::new("old-id".to_string());
        old_session.peer_connection = Some(old_peer.clone());
        let mut replacement_session = crate::session::Session::new("old-id".to_string());
        replacement_session.peer_connection = Some(replacement_peer.clone());
        let replacement_session = Arc::new(replacement_session);
        {
            let mut sessions = app_state.sessions.write().await;
            sessions.insert("old-id".to_string(), Arc::new(old_session));
            sessions.remove("old-id");
            sessions.insert("old-id".to_string(), replacement_session.clone());
            sessions.insert(
                "new-current".to_string(),
                Arc::new(crate::session::Session::new("new-current".to_string())),
            );
        }

        super::close_previous_session_after_delay(
            app_state.clone(),
            "old-id".to_string(),
            old_peer.clone(),
            tokio::time::Duration::ZERO,
        )
        .await;

        let registered = app_state
            .get_session_by_id("old-id")
            .await
            .expect("replacement should remain registered");
        assert!(Arc::ptr_eq(&registered, &replacement_session));
        assert!(!app_state.session_owns_peer("old-id", &old_peer).await);
        assert!(app_state.session_owns_peer("old-id", &replacement_peer).await);
        assert_ne!(
            replacement_peer.connection_state(),
            RTCPeerConnectionState::Closed
        );

        super::close_peer_connection(old_peer, "old-id-old-generation").await;
        super::close_peer_connection(replacement_peer, "old-id-replacement").await;
    }

    #[tokio::test]
    async fn same_track_id_does_not_match_another_session_generation() {
        let _guard = super::MEDIA_LIFECYCLE_LOCK.lock().await;
        let old_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability::default(),
            "video".to_string(),
            "arkkvm".to_string(),
        ));
        let replacement_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability::default(),
            "video".to_string(),
            "arkkvm".to_string(),
        ));

        crate::video::attach_webrtc_sink(replacement_track.clone()).await;
        assert!(!crate::video::equal_webrtc_sink(old_track).await);
        assert!(crate::video::equal_webrtc_sink(replacement_track).await);
        crate::video::detach_webrtc_sink().await;
    }

    #[tokio::test]
    async fn takeover_attaches_session_that_connected_before_publication() {
        let _guard = super::MEDIA_LIFECYCLE_LOCK.lock().await;
        let app_state = crate::state::AppState::new();
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability::default(),
            "video".to_string(),
            "arkkvm".to_string(),
        ));
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability::default(),
            "audio".to_string(),
            "arkkvm".to_string(),
        ));
        let mut session = crate::session::Session::new("already-connected".to_string());
        session.video_track = Some(video_track.clone());
        session.audio_track = Some(audio_track);
        app_state
            .sessions
            .write()
            .await
            .insert(session.id.clone(), Arc::new(session));
        app_state
            .set_current_session_id(Some("already-connected".to_string()))
            .await;

        super::attach_current_session_sinks_if_connected(
            &app_state,
            "already-connected",
            RTCIceConnectionState::Connected,
        )
        .await;

        assert!(crate::video::equal_webrtc_sink(video_track).await);
        crate::video::detach_webrtc_sink().await;
        crate::audio::detach_webrtc_sink().await;
    }

    #[tokio::test]
    async fn reconnect_registration_wins_before_serialized_last_session_cleanup() {
        let app_state = Arc::new(crate::state::AppState::new());
        let lifecycle_guard = super::MEDIA_LIFECYCLE_LOCK.lock().await;
        let cleanup_started = Arc::new(Barrier::new(2));
        let cleanup_started_in_task = cleanup_started.clone();
        let app_state_in_task = app_state.clone();

        let cleanup_task = tokio::spawn(async move {
            cleanup_started_in_task.wait().await;
            super::cleanup_last_session_resources(&app_state_in_task).await;
        });
        cleanup_started.wait().await;
        tokio::task::yield_now().await;
        assert!(!cleanup_task.is_finished(), "cleanup should wait for the lifecycle lock");

        app_state.sessions.write().await.insert(
            "reconnected".to_string(),
            Arc::new(crate::session::Session::new("reconnected".to_string())),
        );
        drop(lifecycle_guard);
        cleanup_task.await.expect("cleanup task should finish");

        assert!(app_state.get_session_by_id("reconnected").await.is_some());
    }

    #[tokio::test]
    async fn cleanup_continues_after_wait_timeout() {
        let (release_cleanup, wait_for_release) = oneshot::channel();
        let cleanup_finished = Arc::new(Notify::new());
        let cleanup_finished_in_task = cleanup_finished.clone();

        let wait_result = super::wait_for_background_cleanup(
            async move {
                let _ = wait_for_release.await;
                cleanup_finished_in_task.notify_one();
            },
            std::time::Duration::from_millis(1),
        )
        .await;

        assert!(wait_result.is_none());
        release_cleanup.send(()).expect("cleanup task should still be running");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            cleanup_finished.notified(),
        )
        .await
        .expect("detached cleanup should finish after the caller stops waiting");
    }
}
