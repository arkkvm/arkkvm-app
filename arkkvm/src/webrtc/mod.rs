mod dc;
mod handler;

pub use dc::{DcCallbacks, spawn_dc_events, wait_dc_open};
pub(crate) use handler::SessionHandler;

use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Context;
use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, RTCConfigurationBuilder,
    RTCIceCandidate, RTCIceConnectionState, RTCIceServer, Registry, SettingEngine,
    register_default_interceptors,
};

use crate::hardware::usb::storage as storage_mod;
use crate::jsonrpc::PROCESSOR;
use crate::session::{Session, SessionRole};
use crate::state::ClaimRoleResult;
use crate::web::get_global_app_state;
use crate::{audio, video};

pub(crate) static MEDIA_LIFECYCLE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
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
    peer_connection: Arc<dyn PeerConnection>,
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

pub(crate) async fn close_removed_session(session: Arc<Session>) {
    // Own the terminal lifecycle: stop before/with PC close so Stopping is not
    // solely dependent on DataChannel OnClose delivery timing.
    session.stop_terminal().await;
    if let Some(peer_connection) = session.peer_connection.clone() {
        close_peer_connection(peer_connection, &session.id).await;
    }
}

pub(crate) async fn cleanup_last_session_resources(app_state: &crate::state::AppState) {
    let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
    if app_state.session_count().await != 0 {
        return;
    }

    info!("Detaching video and audio sinks after last WebRTC session closed");
    video::detach_all_webrtc_sinks().await;
    audio::detach_all_webrtc_sinks().await;

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

async fn negotiated_payload_type(
    peer_connection: &Arc<dyn PeerConnection>,
    kind: RtpCodecKind,
) -> Option<u8> {
    // Prefer the codec we actually encode (H264 / Opus). `.codecs.first()` is unsafe:
    // MediaEngine registers VP8/VP9 before H264, and sender params may surface PT 98 (VP9).
    let preferred_mime = match kind {
        RtpCodecKind::Video => MIME_TYPE_H264,
        RtpCodecKind::Audio => MIME_TYPE_OPUS,
        _ => return None,
    };
    for sender in peer_connection.get_senders().await {
        if sender.track().kind().await != kind {
            continue;
        }
        let codecs = sender
            .get_parameters()
            .await
            .ok()?
            .rtp_parameters
            .codecs;
        if let Some(pt) = codecs
            .iter()
            .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(preferred_mime))
            .map(|c| c.payload_type)
        {
            return Some(pt);
        }
        return codecs.first().map(|c| c.payload_type);
    }
    None
}

pub(crate) async fn attach_role_session_sinks_if_connected(
    app_state: &crate::state::AppState,
    session: &Arc<Session>,
    connection_state: RTCIceConnectionState,
) {
    if !app_state.is_role_slot_holder(&session.id).await {
        return;
    }
    if !matches!(
        connection_state,
        RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
    ) {
        return;
    }

    let peer_connection = session.peer_connection.as_ref();
    if let (Some(track), Some(ssrc)) = (session.video_track.clone(), session.video_ssrc) {
        let payload_type = if let Some(peer) = peer_connection {
            negotiated_payload_type(peer, RtpCodecKind::Video)
                .await
                .unwrap_or(102)
        } else {
            102
        };
        video::attach_webrtc_sink(
            session.role,
            session.id.clone(),
            track,
            ssrc,
            payload_type,
        )
        .await;
        info!(
            "Video track attached to WebRTC session {} role {} pt={}",
            session.id, session.role, payload_type
        );
    }
    if let (Some(track), Some(ssrc)) = (session.audio_track.clone(), session.audio_ssrc) {
        let payload_type = if let Some(peer) = peer_connection {
            negotiated_payload_type(peer, RtpCodecKind::Audio)
                .await
                .unwrap_or(111)
        } else {
            111
        };
        audio::attach_webrtc_sink(
            session.role,
            session.id.clone(),
            track,
            ssrc,
            payload_type,
        )
        .await;
        info!(
            "Audio track attached to WebRTC session {} role {} pt={}",
            session.id, session.role, payload_type
        );
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
    peer_connection: &Arc<dyn PeerConnection>,
) -> bool {
    let Some(session) = app_state.remove_session_for_peer(session_id, peer_connection).await else {
        return false;
    };

    close_removed_session(session).await;
    cleanup_last_session_resources(app_state).await;
    true
}

/// One ICE server entry that may carry TURN username/credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// Prefer credentialed entries when the cloud provided `iceServersCred`;
/// otherwise fall back to bare STUN URL strings (one entry per URL).
pub fn resolve_ice_servers(
    credentialed: Vec<IceServerConfig>,
    bare_urls: Vec<String>,
) -> Option<Vec<IceServerConfig>> {
    if !credentialed.is_empty() {
        return Some(credentialed);
    }
    if bare_urls.is_empty() {
        return None;
    }
    Some(
        bare_urls
            .into_iter()
            .map(|url| IceServerConfig {
                urls: vec![url],
                username: String::new(),
                credential: String::new(),
            })
            .collect(),
    )
}

fn is_turn_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("turn:") || lower.starts_with("turns:")
}

/// Map resolved ICE config into webrtc-rs servers. Skip TURN URLs that lack
/// credentials so one bad entry cannot fail the whole peer connection setup.
fn rtc_ice_servers_from_config(servers: &[IceServerConfig]) -> Vec<RTCIceServer> {
    let mut out = Vec::with_capacity(servers.len());
    for entry in servers {
        let has_creds = !entry.username.is_empty() && !entry.credential.is_empty();
        let urls: Vec<String> = entry
            .urls
            .iter()
            .filter(|url| {
                if is_turn_url(url) && !has_creds {
                    warn!(
                        "Skipping TURN URL without credentials: {}",
                        url
                    );
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        if urls.is_empty() {
            continue;
        }
        out.push(RTCIceServer {
            urls,
            username: entry.username.clone(),
            credential: entry.credential.clone(),
        });
    }
    out
}

/// Session configuration for WebRTC connections
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub ice_servers: Option<Vec<IceServerConfig>>,
    pub local_ip: Option<IpAddr>,
    pub is_cloud: bool,
    pub role: SessionRole,
    /// Echoed on cloud answer/ICE/error when set by the relay.
    pub cloud_client_id: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            ice_servers: None,
            local_ip: None,
            is_cloud: false,
            role: SessionRole::User,
            cloud_client_id: None,
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
        let mut ice_servers = Vec::new();

        info!("Creating new WebRTC session with id: {}", &session_id);

        if config.is_cloud {
            if let Some(servers) = &config.ice_servers {
                let credentialed = servers
                    .iter()
                    .filter(|s| !s.username.is_empty() && !s.credential.is_empty())
                    .count();
                ice_servers = rtc_ice_servers_from_config(servers);
                info!(
                    "Using ICE servers provided by cloud: {} entries ({} credentialed, {:?} urls)",
                    ice_servers.len(),
                    credentialed,
                    ice_servers.iter().map(|s| s.urls.join(",")).collect::<Vec<String>>(),
                );
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

        let mut media_engine = MediaEngine::default();
        self.set_video_codec(&mut media_engine)?;
        self.set_audio_codec(&mut media_engine, audio_rate, audio_channels)?;

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .context("register_default_interceptors failed")?;

        let handler = Arc::new(SessionHandler::new(session_id.clone(), config.role));
        let config_rtc = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();
        let peer_connection = PeerConnectionBuilder::new()
            .with_configuration(config_rtc)
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler.clone())
            // Temporary: run PC driver on the app multi-thread runtime to unwind
            // reactor/Handle confusion from recent experiments. Re-evaluate vs f4c5f63.
            .with_dedicated_reactor_thread(false)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await?;
        let peer_connection: Arc<dyn PeerConnection> = Arc::new(peer_connection);
        handler.attach_peer(Arc::downgrade(&peer_connection));

        let video_ssrc = rand::random::<u32>();
        let video_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_owned(),
            rtcp_feedback: vec![],
        };
        let video_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
            "arkkvm".to_owned(),
            "video".to_owned(),
            "video".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(video_ssrc),
                    ..Default::default()
                },
                codec: video_codec,
                ..Default::default()
            }],
        ))?);
        if let Err(e) = peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
            .await
        {
            close_peer_connection(peer_connection.clone(), &session_id).await;
            return Err(e.into());
        }

        let audio_ssrc = rand::random::<u32>();
        let audio_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: audio_rate,
            channels: audio_channels,
            sdp_fmtp_line: "minptime=20".to_owned(),
            rtcp_feedback: vec![],
        };
        let audio_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
            "arkkvm".to_owned(),
            "audio".to_owned(),
            "audio".to_owned(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(audio_ssrc),
                    ..Default::default()
                },
                codec: audio_codec,
                ..Default::default()
            }],
        ))?);
        if let Err(e) = peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await
        {
            close_peer_connection(peer_connection.clone(), &session_id).await;
            return Err(e.into());
        }

        let mut session = Session::new_with_role(session_id, config.role, config.is_cloud);
        session.cloud_client_id = config.cloud_client_id.clone();
        session.peer_connection = Some(Arc::clone(&peer_connection));
        session.video_track = Some(video_track);
        session.audio_track = Some(audio_track);
        session.video_ssrc = Some(video_ssrc);
        session.audio_ssrc = Some(audio_ssrc);
        session.ice_connection_state = handler.ice_connection_state();

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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
                rtp_codec: RTCRtpCodec {
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
            //     rtp_codec: RTCRtpCodec {
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
            engine.register_codec(codec, RtpCodecKind::Video)?;
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
            rtp_codec: RTCRtpCodec {
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
            engine.register_codec(codec, RtpCodecKind::Audio)?;
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

    if let Some(session) = get_global_app_state()
        .get_session_for_role(SessionRole::User)
        .await
    {
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

    if let Some(session) = app_state.get_session_for_role(SessionRole::User).await {
        let params = serde_json::json!({
            "activeSessionCount": count,
            "hasActiveSessions": count > 0
        });

        if let Err(e) = PROCESSOR.send_event("sessionCountChanged", Some(params), session).await {
            warn!("Failed to send session count update: {}", e);
        }
    }
}

/// True if any WebRTC session is registered (user or agent).
pub async fn has_any_session() -> bool {
    get_global_app_state().has_any_session().await
}

/// Get current user-role session ID (events / legacy callers).
pub async fn get_current_session() -> Option<String> {
    get_global_app_state()
        .get_role_slot_id(SessionRole::User)
        .await
}

/// Send ICE candidate through appropriate signaling channel based on session type
pub(crate) async fn send_ice_candidate(
    session_id: &str,
    candidate: &RTCIceCandidate,
) -> anyhow::Result<()> {
    use serde_json::json;

    let mut candidate_init = candidate.to_json()?;
    let candidate_sdp = candidate_init.candidate.trim();
    if (candidate.address.is_empty() && candidate.foundation.is_empty())
        || candidate_sdp.is_empty()
        || candidate_sdp == "candidate:"
    {
        debug!("Skipping end-of-candidates marker for session {}", session_id);
        return Ok(());
    }
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
    let message = if is_cloud_session {
        let client_id = session_opt.as_ref().and_then(|s| s.cloud_client_id.as_deref());
        crate::cloud::signaling_frame::build_cloud_ice_frame(
            session_id,
            candidate_json,
            client_id,
        )
    } else {
        json!({
            "type": "new-ice-candidate",
            "data": candidate_json
        })
    };

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

/// Claim role slot and activate sinks. On user takeover, kick the previous session.
/// Returns `Ok(ClaimRoleResult)` so callers can refuse answer on agent reject.
pub async fn claim_and_activate_session(
    app_state: std::sync::Arc<crate::state::AppState>,
    new_session: Arc<Session>,
) -> anyhow::Result<ClaimRoleResult> {
    // ARKKVM-619: MCP OFF blocks agent claim (no slot write, no answer).
    if new_session.role == crate::session::SessionRole::Agent
        && !crate::config::get_config_manager().get_mcp_enabled().await
    {
        tracing::info!(
            session_id = %new_session.id,
            "reject agent claim: MCP disabled"
        );
        return Ok(ClaimRoleResult::RejectedMcpDisabled);
    }

    let new_session_id = new_session.id.clone();
    let claim = {
        let _guard = MEDIA_LIFECYCLE_LOCK.lock().await;
        let claim = app_state.claim_role_slot(&new_session).await;
        match &claim {
            ClaimRoleResult::Accepted { previous } => {
                on_active_sessions_changed().await;
                let connection_state = app_state
                    .get_session_by_id(&new_session_id)
                    .await
                    .map(|registered| *registered.ice_connection_state.read())
                    .unwrap_or(RTCIceConnectionState::Unspecified);
                attach_role_session_sinks_if_connected(
                    &app_state,
                    &new_session,
                    connection_state,
                )
                .await;

                if let Some(old_session) = previous
                    && !Arc::ptr_eq(old_session, &new_session)
                {
                    let old_id = old_session.id.clone();
                    if let Err(e) = PROCESSOR
                        .send_event("otherSessionConnected", None, old_session.clone())
                        .await
                    {
                        tracing::warn!(
                            "Failed to send otherSessionConnected to session {}: {}",
                            old_id,
                            e
                        );
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
            }
            ClaimRoleResult::RejectedOccupied
            | ClaimRoleResult::RejectedMcpDisabled
            | ClaimRoleResult::NotRegistered => {}
        }
        claim
    };

    // Control lease updates happen outside MEDIA_LIFECYCLE_LOCK.
    if let ClaimRoleResult::Accepted { previous } = &claim {
        let prev = previous
            .as_ref()
            .filter(|old| !Arc::ptr_eq(old, &new_session));
        crate::control_lease::on_role_claimed(&new_session, prev).await;
    }

    Ok(claim)
}

/// Backward-compatible alias: claim + activate; errors on reject / not registered.
pub async fn handle_session_takeover(
    app_state: std::sync::Arc<crate::state::AppState>,
    new_session: Arc<Session>,
) -> anyhow::Result<()> {
    match claim_and_activate_session(app_state, new_session).await? {
        ClaimRoleResult::Accepted { .. } => Ok(()),
        ClaimRoleResult::RejectedOccupied => {
            Err(anyhow::anyhow!("agent session slot is already occupied"))
        }
        ClaimRoleResult::RejectedMcpDisabled => {
            Err(anyhow::anyhow!("MCP is disabled on the device"))
        }
        ClaimRoleResult::NotRegistered => {
            Err(anyhow::anyhow!("session is no longer registered"))
        }
    }
}

async fn close_previous_session_after_delay(
    app_state: Arc<crate::state::AppState>,
    old_id: String,
    old_peer: Arc<dyn PeerConnection>,
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

    use async_trait::async_trait;
    use rtc::media_stream::MediaStreamTrack;
    use rtc::rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
    };
    use tokio::sync::{Barrier, Notify, oneshot};
    use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
    use webrtc::peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceConnectionState,
    };

    struct DummyHandler;

    #[async_trait]
    impl PeerConnectionEventHandler for DummyHandler {}

    async fn test_pc() -> Arc<dyn PeerConnection> {
        let pc = PeerConnectionBuilder::new()
            .with_handler(Arc::new(DummyHandler))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .expect("peer connection should be created");
        Arc::new(pc)
    }

    fn test_sample_track(kind: RtpCodecKind, track_id: &str, ssrc: u32) -> Arc<TrackLocalStaticSample> {
        let mime = match kind {
            RtpCodecKind::Audio => "audio/opus",
            _ => "video/H264",
        };
        let clock_rate = if kind == RtpCodecKind::Audio { 48000 } else { 90000 };
        let channels = if kind == RtpCodecKind::Audio { 2 } else { 0 };
        Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "arkkvm".to_owned(),
                track_id.to_owned(),
                track_id.to_owned(),
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: RTCRtpCodec {
                        mime_type: mime.to_owned(),
                        clock_rate,
                        channels,
                        ..Default::default()
                    },
                    ..Default::default()
                }],
            ))
            .expect("track"),
        )
    }

    #[tokio::test]
    async fn concurrent_session_cleanup_closes_registered_peer_once() {
        let peer_connection = test_pc().await;
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
        let _ = peer_connection.close().await;
    }

    #[tokio::test]
    async fn same_id_replace_returns_previous_for_close() {
        let old_peer = test_pc().await;
        let new_peer = test_pc().await;
        let app_state = crate::state::AppState::new();

        let mut old_session = crate::session::Session::new("reused-id".to_string());
        old_session.peer_connection = Some(old_peer.clone());
        let old_session = Arc::new(old_session);
        assert!(app_state.add_session(old_session.clone()).await.is_none());
        assert!(
            matches!(
                app_state.claim_role_slot(&old_session).await,
                crate::state::ClaimRoleResult::Accepted { previous: None }
            )
        );

        let mut replacement = crate::session::Session::new("reused-id".to_string());
        replacement.peer_connection = Some(new_peer.clone());
        let replacement = Arc::new(replacement);
        let previous = app_state
            .add_session(replacement.clone())
            .await
            .expect("same-id replace should return the previous session");
        assert!(Arc::ptr_eq(&previous, &old_session));
        // After same-id replace, role slot still points at the id; claim with new Arc updates holder.
        assert!(
            matches!(
                app_state.claim_role_slot(&replacement).await,
                crate::state::ClaimRoleResult::Accepted { previous: None }
            )
        );
        assert_eq!(
            app_state.get_role_slot_id(crate::session::SessionRole::User).await.as_deref(),
            Some("reused-id"),
            "same-id replace must keep the role slot marker"
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
        let active_peer = test_pc().await;
        let stale_peer = test_pc().await;
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
        app_state
            .role_sessions
            .write()
            .await
            .insert(crate::session::SessionRole::User, "reused-id".to_string());

        assert!(
            app_state
                .remove_session_for_peer("reused-id", &stale_peer)
                .await
                .is_none()
        );
        assert_eq!(
            app_state.get_role_slot_id(crate::session::SessionRole::User).await.as_deref(),
            Some("reused-id"),
            "a mismatched callback must not clear the replacement's role slot"
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
    async fn concurrent_user_claims_preserve_every_predecessor() {
        let app_state = crate::state::AppState::new();
        let original = Arc::new(crate::session::Session::new("original".to_string()));
        let first = Arc::new(crate::session::Session::new("first".to_string()));
        let second = Arc::new(crate::session::Session::new("second".to_string()));
        {
            let mut sessions = app_state.sessions.write().await;
            sessions.insert(original.id.clone(), original.clone());
            sessions.insert(first.id.clone(), first.clone());
            sessions.insert(second.id.clone(), second.clone());
        }
        assert!(matches!(
            app_state.claim_role_slot(&original).await,
            crate::state::ClaimRoleResult::Accepted { previous: None }
        ));

        let (first_claim, second_claim) = tokio::join!(
            app_state.claim_role_slot(&first),
            app_state.claim_role_slot(&second),
        );

        let mut observed = Vec::new();
        for claim in [first_claim, second_claim] {
            match claim {
                crate::state::ClaimRoleResult::Accepted { previous: Some(prev) } => {
                    observed.push(prev.id.clone());
                }
                crate::state::ClaimRoleResult::Accepted { previous: None } => {
                    // Same-slot race: one may see already-updated id with no previous.
                }
                other => panic!("unexpected claim result: {:?}", other),
            }
        }
        observed.push(
            app_state
                .get_role_slot_id(crate::session::SessionRole::User)
                .await
                .expect("one replacement should remain in the user slot"),
        );
        observed.sort();
        observed.dedup();
        assert!(observed.contains(&"first".to_string()) || observed.contains(&"second".to_string()));
        assert!(
            app_state.get_role_slot_id(crate::session::SessionRole::User).await
                == Some("first".to_string())
                || app_state.get_role_slot_id(crate::session::SessionRole::User).await
                    == Some("second".to_string())
        );
    }

    #[tokio::test]
    async fn claim_rejects_a_session_removed_before_takeover() {
        let app_state = crate::state::AppState::new();
        let current = Arc::new(crate::session::Session::new("current".to_string()));
        let removed_candidate = Arc::new(crate::session::Session::new("removed".to_string()));
        app_state
            .sessions
            .write()
            .await
            .insert(current.id.clone(), current.clone());
        assert!(matches!(
            app_state.claim_role_slot(&current).await,
            crate::state::ClaimRoleResult::Accepted { previous: None }
        ));

        assert!(matches!(
            app_state.claim_role_slot(&removed_candidate).await,
            crate::state::ClaimRoleResult::NotRegistered
        ));
        assert_eq!(
            app_state.get_role_slot_id(crate::session::SessionRole::User).await.as_deref(),
            Some("current")
        );
    }

    #[tokio::test]
    async fn delayed_takeover_cleanup_does_not_close_reused_session_id() {
        let old_peer = test_pc().await;
        let replacement_peer = test_pc().await;
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

        super::close_peer_connection(old_peer, "old-id-old-generation").await;
        super::close_peer_connection(replacement_peer, "old-id-replacement").await;
    }

    #[tokio::test]
    async fn same_track_id_does_not_match_another_session_generation() {
        let _guard = super::MEDIA_LIFECYCLE_LOCK.lock().await;
        let old_track = test_sample_track(RtpCodecKind::Video, "video", 1);
        let replacement_track = test_sample_track(RtpCodecKind::Video, "video", 2);

        crate::video::attach_webrtc_sink(
            crate::session::SessionRole::User,
            "replacement".to_string(),
            replacement_track.clone(),
            2,
            102,
        )
        .await;
        assert!(
            !crate::video::equal_webrtc_sink(crate::session::SessionRole::User, &old_track).await
        );
        assert!(
            crate::video::equal_webrtc_sink(crate::session::SessionRole::User, &replacement_track)
                .await
        );
        crate::video::detach_webrtc_sink(crate::session::SessionRole::User).await;
    }

    #[tokio::test]
    async fn takeover_attaches_session_that_connected_before_publication() {
        let _guard = super::MEDIA_LIFECYCLE_LOCK.lock().await;
        let app_state = crate::state::AppState::new();
        let video_track = test_sample_track(RtpCodecKind::Video, "video", 11);
        let audio_track = test_sample_track(RtpCodecKind::Audio, "audio", 12);
        let mut session = crate::session::Session::new("already-connected".to_string());
        session.video_track = Some(video_track.clone());
        session.audio_track = Some(audio_track);
        session.video_ssrc = Some(11);
        session.audio_ssrc = Some(12);
        let session = Arc::new(session);
        app_state
            .sessions
            .write()
            .await
            .insert(session.id.clone(), session.clone());
        app_state
            .role_sessions
            .write()
            .await
            .insert(crate::session::SessionRole::User, "already-connected".to_string());

        super::attach_role_session_sinks_if_connected(
            &app_state,
            &session,
            RTCIceConnectionState::Connected,
        )
        .await;

        assert!(
            crate::video::equal_webrtc_sink(crate::session::SessionRole::User, &video_track).await
        );
        crate::video::detach_webrtc_sink(crate::session::SessionRole::User).await;
        crate::audio::detach_webrtc_sink(crate::session::SessionRole::User).await;
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

    #[test]
    fn resolve_ice_servers_prefers_credentialed() {
        let cred = vec![super::IceServerConfig {
            urls: vec!["turn:stun.arkkvm.cn:3478?transport=udp".into()],
            username: "1791979895".into(),
            credential: "c2VjcmV0".into(),
        }];
        let bare = vec!["stun:stun.arkkvm.cn:3478".into()];
        let resolved = super::resolve_ice_servers(cred.clone(), bare).unwrap();
        assert_eq!(resolved, cred);
    }

    #[test]
    fn resolve_ice_servers_falls_back_to_bare_stun() {
        let resolved =
            super::resolve_ice_servers(vec![], vec!["stun:stun.arkkvm.cn:3478".into()]).unwrap();
        assert_eq!(
            resolved,
            vec![super::IceServerConfig {
                urls: vec!["stun:stun.arkkvm.cn:3478".into()],
                username: String::new(),
                credential: String::new(),
            }]
        );
    }

    #[test]
    fn resolve_ice_servers_empty_when_nothing_configured() {
        assert!(super::resolve_ice_servers(vec![], vec![]).is_none());
    }

    #[test]
    fn rtc_ice_servers_skips_turn_without_credentials() {
        let servers = vec![
            super::IceServerConfig {
                urls: vec![
                    "turn:stun.arkkvm.cn:3478?transport=udp".into(),
                    "stun:stun.arkkvm.cn:3478".into(),
                ],
                username: String::new(),
                credential: String::new(),
            },
            super::IceServerConfig {
                urls: vec![
                    "turn:stun.arkkvm.cn:3478?transport=tcp".into(),
                    "turn:stun.arkkvm.cn:3478?transport=udp".into(),
                ],
                username: "user".into(),
                credential: "pass".into(),
            },
        ];
        let rtc = super::rtc_ice_servers_from_config(&servers);
        assert_eq!(rtc.len(), 2);
        assert_eq!(rtc[0].urls, vec!["stun:stun.arkkvm.cn:3478"]);
        assert!(rtc[0].username.is_empty());
        assert_eq!(rtc[1].urls.len(), 2);
        assert_eq!(rtc[1].username, "user");
        assert_eq!(rtc[1].credential, "pass");
    }
}
