use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{Level, debug, info};

mod axum {
    pub use axum::Router;
    pub use axum_server::tls_rustls::RustlsConfig;
    pub use axum_server::{bind, bind_rustls};
    pub use tower_http::services::ServeDir;
}

mod sio {
    pub use socketioxide::SocketIo;
    pub use socketioxide::extract::{AckSender, Data, SocketRef, State};
}

mod webrtc {
    pub use webrtc::api::interceptor_registry::register_default_interceptors;
    pub use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
    pub use webrtc::api::setting_engine::SettingEngine;
    pub use webrtc::api::{API, APIBuilder};
    pub use webrtc::data_channel::RTCDataChannel;
    pub use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
    pub use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
    pub use webrtc::interceptor::registry::Registry;
    pub use webrtc::media::Sample;
    pub use webrtc::peer_connection::RTCPeerConnection;
    pub use webrtc::peer_connection::configuration::RTCConfiguration;
    pub use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
    pub use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    pub use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
}

struct AppState {
    api: webrtc::API,
    peer_conn: RwLock<Option<webrtc::RTCPeerConnection>>,
}

// async fn handle_session_disconnect(
//     socket: sio::SocketRef,
//     sio::State(state): sio::State<Arc<AppState>>,
// ) {
//     info!("[sid={}] disconnected", socket.id);
//     let mut guard = state.peer_conn.write().await;
//     guard.take();
// }

// async fn handle_session_connect(
//     socket: sio::SocketRef,
//     sio::State(state): sio::State<Arc<AppState>>,
// ) {
//     info!("[sid={}] connected", socket.id);
//     socket.on_disconnect(handle_session_disconnect);

//     let peer_config = webrtc::RTCConfiguration::default();
//     let peer_conn = state.api.new_peer_connection(peer_config).await.unwrap();

//     // Set the handler for ICE connection state
//     // This will notify you when the peer has connected/disconnected
//     peer_conn.on_ice_connection_state_change(Box::new(
//         |connection_state: webrtc::RTCIceConnectionState| {
//             println!("ICE Connection State has changed: {connection_state}");
//             Box::pin(async {})
//         },
//     ));

//     peer_conn.on_ice_candidate(Box::new(move |candidate: Option<webrtc::RTCIceCandidate>| {
//         Box::pin(async move {
//             info!("ICE candidate: {:?}", candidate);
//         })
//     }));

//     // Send the current time via a DataChannel to the remote peer every 3 seconds
//     peer_conn.on_data_channel(Box::new(|d: Arc<webrtc::RTCDataChannel>| {
//         Box::pin(async move {
//             let d2 = Arc::clone(&d);
//             d.on_open(Box::new(move || {
//                 Box::pin(async move {
//                     loop {
//                         match d2.send_text(format!("{:?}", tokio::time::Instant::now())).await {
//                             Ok(_) => tokio::time::sleep(Duration::from_secs(3)).await,
//                             Err(e) => {
//                                 debug!("DataChannel closed: {:?}", e);
//                                 // break;
//                                 tokio::time::sleep(Duration::from_secs(3)).await;
//                             }
//                         }
//                     }
//                 })
//             }));
//         })
//     }));

//     // Create a video track
//     let video_track = Arc::new(webrtc::TrackLocalStaticSample::new(
//         webrtc::RTCRtpCodecCapability {
//             mime_type: webrtc::MIME_TYPE_H264.to_owned(),
//             ..Default::default()
//         },
//         "video".to_owned(),
//         "arkkvm".to_owned(),
//     ));

//     let rtp_sender = peer_conn.add_track(video_track).await.unwrap();

//     // Read incoming RTCP packets
//     // Before these packets are returned they are processed by interceptors. For things
//     // like NACK this needs to be called.
//     tokio::spawn(async move {
//         let mut rtcp_buf = vec![0u8; 1500];
//         while let Ok((packet, attributes)) = rtp_sender.read(&mut rtcp_buf).await {
//             debug!("packet: {:?}", packet);
//             debug!("attributes: {:?}", attributes);
//         }
//     });

//     // tokio::spawn(async move {
//     //     // Open a H264 file and start reading using our H264Reader
//     //     let file = File::open(&video_file_name)?;
//     //     let reader = BufReader::new(file);
//     //     let mut h264 = H264Reader::new(reader, 1_048_576);

//     //     // Wait for connection established
//     //     notify_video.notified().await;

//     //     println!("play video from disk file {video_file_name}");

//     //     // It is important to use a time.Ticker instead of time.Sleep because
//     //     // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
//     //     // * works around latency issues with Sleep
//     //     let mut ticker = tokio::time::interval(Duration::from_millis(33));
//     //     loop {
//     //         let nal = match h264.next_nal() {
//     //             Ok(nal) => nal,
//     //             Err(err) => {
//     //                 println!("All video frames parsed and sent: {err}");
//     //                 break;
//     //             }
//     //         };

//     //         /*println!(
//     //             "PictureOrderCount={}, ForbiddenZeroBit={}, RefIdc={}, UnitType={}, data={}",
//     //             nal.picture_order_count,
//     //             nal.forbidden_zero_bit,
//     //             nal.ref_idc,
//     //             nal.unit_type,
//     //             nal.data.len()
//     //         );*/
//     //         video_track
//     //             .write_sample(&Sample {
//     //                 data: nal.data.freeze(),
//     //                 duration: Duration::from_secs(1),
//     //                 ..Default::default()
//     //             })
//     //             .await
//     //             .unwrap();

//     //         let _ = ticker.tick().await;
//     //     }
//     // });

//     info!("replace peer_conn: {:?}", peer_conn);
//     let mut guard = state.peer_conn.write().await;
//     guard.replace(peer_conn);
//     drop(guard);

//     socket.on("ice-candidate", handle_ice_candidate);
//     socket.on("offer", handle_offer);
// }

async fn handle_ice_candidate(
    socket: sio::SocketRef,
    sio::Data(data): sio::Data<String>,
    sio::State(state): sio::State<Arc<AppState>>,
    ack: sio::AckSender,
) {
    // TODO:
    // info!("ice-candidate: {:?}", data);
    // info!("state.peer_conn: {:?}", state.peer_conn);
    // let guard = state.peer_conn.read().await;
    // let peer_conn = guard.as_ref().unwrap();
    // let candidate = serde_json::from_str::<RTCIceCandidateInit>(&data).unwrap();
    // info!("candidate: {:?}", candidate);
    // peer_conn.add_ice_candidate(candidate).await.unwrap();
}

async fn handle_offer(
    socket: sio::SocketRef,
    sio::Data(data): sio::Data<String>,
    sio::State(state): sio::State<Arc<AppState>>,
    ack: sio::AckSender,
) {
    let offer = serde_json::from_str::<webrtc::RTCSessionDescription>(&data).unwrap();
    info!("offer: {:?}", offer);

    let guard = state.peer_conn.read().await;
    let peer_conn = guard.as_ref().unwrap();

    peer_conn.set_remote_description(offer).await.unwrap();

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_conn.gathering_complete_promise().await;

    // Create an answer
    let answer = peer_conn.create_answer(None).await.unwrap();

    // Sets the LocalDescription, and starts our UDP listeners
    peer_conn.set_local_description(answer).await.unwrap();

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    match peer_conn.local_description().await {
        Some(local_description) => {
            info!("local_description: {:?}", local_description);
            let local_description = serde_json::to_string(&local_description).unwrap();
            ack.send(&local_description).unwrap();
        }
        None => {
            ack.send(&"error".to_string()).unwrap();
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt().with_max_level(Level::DEBUG).init();

    rustls::crypto::ring::default_provider().install_default().unwrap();

    // Create a MediaEngine object to configure the supported codec
    let mut m = webrtc::MediaEngine::default();
    m.register_default_codecs()?;

    let mut registry = webrtc::Registry::new();

    // Use the default set of Interceptors
    registry = webrtc::register_default_interceptors(registry, &mut m)?;

    let setting_engine = webrtc::SettingEngine::default();

    // Create the API object with the MediaEngine
    let api = webrtc::APIBuilder::new()
        .with_media_engine(m)
        .with_setting_engine(setting_engine)
        .with_interceptor_registry(registry)
        .build();

    let app_state = Arc::new(AppState { api, peer_conn: RwLock::new(None) });

    info!("Starting server");

    // let (layer, io) = sio::SocketIo::builder().with_state(app_state.clone()).build_layer();

    // io.ns("/", handle_session_connect);

    let app = axum::Router::new()
        .fallback_service(axum::ServeDir::new("examples/webrtc"))
        // .layer(layer)
        .with_state(app_state);

    let http_addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], 8443));

    info!("Starting HTTP server on http://localhost:8000");
    info!("Starting HTTPS server on https://localhost:8443");

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("Failed to generate self-signed certificate");
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();

    let tls_config =
        axum::RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes()).await?;

    // Start both HTTP and HTTPS servers concurrently
    let http_server = axum::bind(http_addr).serve(app.clone().into_make_service());
    let https_server = axum::bind_rustls(https_addr, tls_config).serve(app.into_make_service());

    tokio::try_join!(http_server, https_server)?;

    Ok(())
}
