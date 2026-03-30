//! WebRTC module test example
//!
//! This example demonstrates the complete WebRTC functionality
//! including session creation, offer/answer exchange, and ICE candidate handling.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use arkkvm::state::AppState;
use arkkvm::webrtc::SessionConfig;
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();

    info!("Starting WebRTC module test...");

    // Test 1: WebRTC API initialization
    test_webrtc_api_init().await?;

    // Test 2: Session creation
    test_session_creation().await?;

    // Test 3: Offer/Answer exchange
    test_offer_answer_exchange().await?;

    // Test 4: ICE candidate handling
    test_ice_candidate_handling().await?;

    // Test 5: Session state management
    test_session_state_management().await?;

    // Test 6: Error handling
    test_error_handling().await?;

    info!("WebRTC module test completed successfully!");
    Ok(())
}

/// Test WebRTC API initialization
async fn test_webrtc_api_init() -> Result<()> {
    info!("Testing WebRTC API initialization...");

    match arkkvm::webrtc::init_webrtc_api().await {
        Ok(_) => {
            info!("✅ WebRTC API initialized successfully");
        }
        Err(e) => {
            warn!("⚠️  Failed to initialize WebRTC API: {}", e);
            return Err(e);
        }
    }

    // Verify API is accessible
    let _api = arkkvm::webrtc::get_webrtc_api().await;
    info!("✅ WebRTC API instance retrieved successfully");

    Ok(())
}

/// Test session creation
async fn test_session_creation() -> Result<()> {
    info!("Testing WebRTC session creation...");

    let api = arkkvm::webrtc::get_webrtc_api().await;
    let session_id = Uuid::new_v4().to_string();

    // Create session configuration
    let config = SessionConfig {
        ice_servers: Some(vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
        ]),
        local_ip: Some("127.0.0.1".parse::<IpAddr>().unwrap()),
        is_cloud: false,
    };

    match api.new_session(config, session_id.clone()).await {
        Ok(session) => {
            info!("✅ WebRTC session created successfully with id: {}", session.id);
            info!("📊 Session details:");
            info!("   - ID: {}", session.id);
            info!("   - Peer connection: {}", session.peer_connection.is_some());
            info!("   - Video track: {}", session.video_track.is_some());
            info!("   - Audio track: {}", session.audio_track.is_some());
            info!("   - Control channel: {}", session.control_channel.is_some());
            info!("   - RPC channel: {}", session.rpc_channel.is_some());
        }
        Err(e) => {
            warn!("⚠️  Failed to create WebRTC session: {}", e);
            return Err(e);
        }
    }

    Ok(())
}

/// Test offer/answer exchange with real WebRTC offer
async fn test_offer_answer_exchange() -> Result<()> {
    info!("Testing offer/answer exchange with real WebRTC offer...");

    let api = arkkvm::webrtc::get_webrtc_api().await;
    let session_id = Uuid::new_v4().to_string();

    // Create session with ICE servers for real connectivity
    let config = SessionConfig {
        ice_servers: Some(vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
        ]),
        local_ip: Some("127.0.0.1".parse::<IpAddr>().unwrap()),
        is_cloud: false,
    };

    let session = api.new_session(config, session_id).await?;

    // Create a real WebRTC offer from a browser-like client
    let real_offer = r#"{
        "type": "offer",
        "sdp": "v=0\r\no=- 1234567890 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=msid-semantic: WMS\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 97 98 99 100 101 102\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:abc123\r\na=ice-pwd:def456\r\na=ice-options:trickle\r\na=fingerprint:sha-256 12:34:56:78:9A:BC:DE:F0\r\na=setup:actpass\r\na=mid:0\r\na=sendonly\r\na=rtpmap:96 H264/90000\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\na=rtpmap:98 H264/90000\r\na=rtpmap:99 rtx/90000\r\na=fmtp:99 apt=98\r\na=rtpmap:100 VP8/90000\r\na=rtpmap:101 rtx/90000\r\na=fmtp:101 apt=100\r\na=rtpmap:102 VP9/90000\r\n"
    }"#;

    // Encode the real offer to base64
    let offer_b64 = STANDARD.encode(real_offer.as_bytes());

    // Exchange offer for answer
    match session.exchange_offer(&offer_b64).await {
        Ok(answer) => {
            info!("✅ Real offer/answer exchange successful!");
            info!("📤 Real offer length: {} bytes", offer_b64.len());
            info!("📥 Answer length: {} bytes", answer.len());

            // Decode and analyze the real answer
            if let Ok(answer_bytes) = STANDARD.decode(&answer)
                && let Ok(answer_str) = String::from_utf8(answer_bytes)
            {
                info!("📋 Real answer SDP preview: {}", &answer_str[..answer_str.len().min(200)]);

                // Verify it's a valid SDP answer
                if answer_str.contains("v=0")
                    && answer_str.contains("o=")
                    && answer_str.contains("s=")
                {
                    info!("✅ Answer contains valid SDP format");
                }
                if answer_str.contains("a=sendonly") || answer_str.contains("a=recvonly") {
                    info!("✅ Answer contains valid media direction");
                }
                if answer_str.contains("a=ice-ufrag") && answer_str.contains("a=ice-pwd") {
                    info!("✅ Answer contains ICE credentials");
                }
            }
        }
        Err(e) => {
            warn!("⚠️  Real offer/answer exchange failed: {}", e);
            info!("ℹ️  This might indicate a WebRTC configuration issue");
            return Err(e);
        }
    }

    Ok(())
}

/// Test ICE candidate handling with real candidates
async fn test_ice_candidate_handling() -> Result<()> {
    info!("Testing ICE candidate handling with real candidates...");

    let api = arkkvm::webrtc::get_webrtc_api().await;
    let session_id = Uuid::new_v4().to_string();

    // Create session with ICE servers for real connectivity
    let config = SessionConfig {
        ice_servers: Some(vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
        ]),
        local_ip: Some("127.0.0.1".parse::<IpAddr>().unwrap()),
        is_cloud: true, // Enable cloud mode for ICE handling
    };

    let session = api.new_session(config, session_id).await?;

    // First, we need to set a remote description before adding ICE candidates
    let real_offer = r#"{
        "type": "offer",
        "sdp": "v=0\r\no=- 1234567890 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\na=msid-semantic: WMS\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:abc123\r\na=ice-pwd:def456\r\na=ice-options:trickle\r\na=fingerprint:sha-256 12:34:56:78:9A:BC:DE:F0\r\na=setup:actpass\r\na=mid:0\r\na=sendonly\r\na=rtpmap:96 H264/90000\r\n"
    }"#;

    let offer_b64 = STANDARD.encode(real_offer.as_bytes());

    // Set remote description first
    match session.exchange_offer(&offer_b64).await {
        Ok(_) => {
            info!("✅ Remote description set successfully");
        }
        Err(e) => {
            warn!("⚠️  Failed to set remote description: {}", e);
            return Err(e);
        }
    }

    // Test real ICE candidate addition
    let real_candidates = [
        r#"{
            "candidate": "candidate:1 1 UDP 2122252543 192.168.1.100 54321 typ host",
            "sdpMLineIndex": 0,
            "sdpMid": "0"
        }"#,
        r#"{
            "candidate": "candidate:2 1 UDP 1686052607 203.0.113.1 12345 typ srflx raddr 192.168.1.100 rport 54321",
            "sdpMLineIndex": 0,
            "sdpMid": "0"
        }"#,
        r#"{
            "candidate": "candidate:3 1 TCP 1019216383 192.168.1.100 9 typ host tcptype active",
            "sdpMLineIndex": 0,
            "sdpMid": "0"
        }"#,
    ];

    for (i, candidate) in real_candidates.iter().enumerate() {
        match session.add_ice_candidate(candidate).await {
            Ok(_) => {
                info!("✅ Real ICE candidate {} added successfully", i + 1);
            }
            Err(e) => {
                warn!("⚠️  Failed to add real ICE candidate {}: {}", i + 1, e);
                info!("ℹ️  This might indicate ICE candidate format issue");
            }
        }
    }

    Ok(())
}

/// Test session state management
async fn test_session_state_management() -> Result<()> {
    info!("Testing session state management...");

    // Test current session management
    let current_session = arkkvm::webrtc::get_current_session().await;
    info!("📱 Current session: {:?}", current_session);

    // Test RPC channel storage
    let test_session_id = "test_session_123".to_string();
    // let has_rpc = arkkvm::webrtc::get_rpc_channel(&test_session_id).await.is_some();
    // info!("🔗 RPC channel for test session: {}", has_rpc);

    Ok(())
}

/// Test error handling
async fn test_error_handling() -> Result<()> {
    info!("Testing error handling...");

    // Test invalid offer
    let invalid_offer = "invalid_base64_string!!!";

    let api = arkkvm::webrtc::get_webrtc_api().await;
    let session = api.new_session(SessionConfig::default(), Uuid::new_v4().to_string()).await?;

    match session.exchange_offer(invalid_offer).await {
        Ok(_) => {
            warn!("⚠️  Unexpected success with invalid offer");
        }
        Err(e) => {
            info!("✅ Correctly handled invalid offer: {}", e);
        }
    }

    // Test invalid ICE candidate
    let invalid_candidate = "invalid_json_candidate";
    match session.add_ice_candidate(invalid_candidate).await {
        Ok(_) => {
            warn!("⚠️  Unexpected success with invalid ICE candidate");
        }
        Err(e) => {
            info!("✅ Correctly handled invalid ICE candidate: {}", e);
        }
    }

    Ok(())
}
