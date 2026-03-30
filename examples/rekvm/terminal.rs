use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use arkkvm::terminal::TerminalSize;
use arkkvm::webrtc::SessionConfig;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio::time::timeout;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::peer_connection::configuration::RTCConfiguration;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize rustls CryptoProvider to avoid panic
    rustls::crypto::ring::default_provider().install_default().unwrap();

    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    // Initialize arkkvm WebRTC API (terminal data channel handlers registered on server side)
    arkkvm::webrtc::init_webrtc_api().await?;
    let api = arkkvm::webrtc::get_webrtc_api().await;

    // Create server session (routes label=terminal to setup_terminal_channel in on_data_channel)
    let session = api
        .new_session(SessionConfig::default(), "terminal_example".into())
        .await
        .context("create server-side session")?;

    // Client PeerConnection and DataChannel (label=terminal)
    let client_api = APIBuilder::new().build();
    let pc = client_api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .context("create client pc")?;

    // Monitor connection state
    pc.on_peer_connection_state_change(Box::new(|state| {
        eprintln!("Peer connection state changed: {:?}", state);
        Box::pin(async {})
    }));

    pc.on_ice_connection_state_change(Box::new(|state| {
        eprintln!("ICE connection state changed: {:?}", state);
        Box::pin(async {})
    }));

    let dc_init = RTCDataChannelInit { negotiated: None, ..Default::default() };
    let dc: Arc<RTCDataChannel> = pc
        .create_data_channel("terminal", Some(dc_init))
        .await
        .context("create terminal datachannel")?;

    eprintln!("Created data channel: label='{}', id={:?}", dc.label(), dc.id());

    // Set up data channel callbacks BEFORE SDP exchange
    let (open_tx, mut open_rx) = mpsc::unbounded_channel();
    dc.on_open(Box::new(move || {
        eprintln!("Data channel opened!");
        let _ = open_tx.send(());
        Box::pin(async {})
    }));

    dc.on_close(Box::new(move || {
        eprintln!("Data channel closed!");
        Box::pin(async {})
    }));

    // Bind message callback (PTY output → stderr to avoid interfering with stdin)
    dc.on_message(Box::new(move |msg: DataChannelMessage| {
        Box::pin(async move {
            let _ = io::stderr().write_all(&msg.data);
            let _ = io::stderr().flush();
        })
    }));

    // Complete Offer/Answer (wait for ICE completion to avoid trickle timeout)
    let offer = pc.create_offer(None).await.context("create offer")?;
    pc.set_local_description(offer).await.context("set local desc")?;

    // Wait for local ICE gathering to complete, ensuring SDP includes candidates
    let mut gather_complete = pc.gathering_complete_promise().await;
    let _ = gather_complete.recv().await;

    // Exchange SDP with candidates included
    let local =
        pc.local_description().await.context("local description missing after gathering")?;
    let offer_json = serde_json::to_string(&local)?;
    let offer_b64 = B64.encode(offer_json.as_bytes());

    let answer_b64 = session.exchange_offer(&offer_b64).await?;
    let answer_bytes = B64.decode(answer_b64).context("decode answer b64")?;
    let answer: webrtc::peer_connection::sdp::session_description::RTCSessionDescription =
        serde_json::from_slice(&answer_bytes).context("parse answer json")?;
    pc.set_remote_description(answer).await.context("set remote desc")?;

    // Wait for DataChannel open (after SDP exchange to avoid timeout)
    eprintln!("Waiting for terminal connection...");
    eprintln!("Data channel state: ready_state={:?}", dc.ready_state());

    timeout(Duration::from_secs(15), open_rx.recv())
        .await
        .context("terminal datachannel open timeout")?;

    eprintln!("Data channel opened successfully!");
    eprintln!("Data channel state after open: ready_state={:?}", dc.ready_state());

    eprintln!(
        "[terminal example ready]\n\
         - Enter commands, e.g.: `echo hi`\n\
         - Enter JSON to resize terminal, e.g.: {{\"rows\": 24, \"cols\": 80}}\n\
         - Ctrl+C to exit\n\
         > "
    );

    // Probe: verify I/O loopback
    dc.send(&"echo READY_FROM_EXAMPLE\n".as_bytes().into()).await.ok();

    // Test: send a simple command to verify message sending works
    eprintln!("Testing message sending...");
    let test_payload = "echo TEST_MESSAGE\n".as_bytes().to_vec();
    eprintln!("Sending test payload: {:?}", String::from_utf8_lossy(&test_payload));
    if let Err(e) = dc.send(&test_payload.into()).await {
        eprintln!("Failed to send test message: {}", e);
    } else {
        eprintln!("Successfully sent test message");
    }

    // Read from stdin: JSON → resize window (server side); otherwise write to PTY
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();

        // Check data channel state before sending
        eprintln!("Data channel state before sending: ready_state={:?}", dc.ready_state());

        if trimmed.len() > 1 && trimmed.starts_with('{') && trimmed.ends_with('}') {
            // Optional: parse validation; actual resize handled by server @terminal.rs
            let _ = serde_json::from_str::<TerminalSize>(trimmed);
            let payload = trimmed.as_bytes().to_vec();
            eprintln!("Sending JSON payload: {:?}", String::from_utf8_lossy(&payload));
            if let Err(e) = dc.send(&payload.into()).await {
                eprintln!("Failed to send JSON: {}", e);
            } else {
                eprintln!("Successfully sent JSON payload");
            }
        } else {
            let payload = format!("{line}\n").into_bytes();
            eprintln!("Sending command payload: {:?}", String::from_utf8_lossy(&payload));
            eprintln!("Command payload length: {} bytes", payload.len());
            eprintln!("Command payload hex: {:02x?}", payload);
            if let Err(e) = dc.send(&payload.into()).await {
                eprintln!("Failed to send command: {}", e);
            } else {
                eprintln!("Successfully sent command payload");
            }
        }
        // Add prompt after each command
        eprint!("> ");
        let _ = io::stderr().flush();
    }

    // Graceful shutdown
    let _ = dc.close().await;
    let _ = pc.close().await;
    Ok(())
}
