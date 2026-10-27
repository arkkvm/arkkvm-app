//! Bidirectional bridge between a DataChannel and TCP/UDP to the resolved host.
//!
//! Lifecycle is bound to the DataChannel:
//! - Do not open the target socket until the DataChannel is `Open`.
//! - If the DataChannel closes (or fails to open), abort connect / tear down the socket.
//! - If the target socket ends, close the DataChannel.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{info, warn};
use webrtc::data_channel::{DataChannel, RTCDataChannelMessage};

use crate::services::local_file::DataChannelBackpressure;
use crate::webrtc::{DcCallbacks, spawn_dc_events, wait_dc_open};

use super::resolve::{IpForwardError, err_msg};
use super::ticket::{ForwardProtocol, IpForwardParams};

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const UDP_MAX_DATAGRAM: usize = 1200;
const TCP_BUF: usize = 16 * 1024;

pub struct IpForward;

impl IpForward {
    pub async fn attach_async(
        channel: Arc<dyn DataChannel>,
        params: IpForwardParams,
    ) -> Result<()> {
        let label = channel.label().await.unwrap_or_default();
        info!(
            label = %label,
            host = %params.host,
            port = params.port,
            protocol = params.protocol.as_str(),
            "IpForward attaching"
        );

        let (dc_tx, dc_rx) = mpsc::channel::<Bytes>(64);
        let (close_tx, close_rx) = mpsc::channel::<()>(1);
        let udp_max = matches!(&params.protocol, ForwardProtocol::Udp).then_some(UDP_MAX_DATAGRAM);
        bind_dc_handlers(channel.clone(), dc_tx, close_tx, udp_max);

        // Strong binding: no target I/O until the DataChannel can carry bytes.
        wait_dc_open(&channel, std::time::Duration::from_secs(30)).await?;

        let result = match params.protocol {
            ForwardProtocol::Tcp => bridge_tcp(channel.clone(), params, dc_rx, close_rx).await,
            ForwardProtocol::Udp => bridge_udp(channel.clone(), params, dc_rx, close_rx).await,
        };

        // Ensure DataChannel is closed when the bridge ends for any reason.
        let _ = channel.close().await;
        result
    }
}

/// Install DC message + close handlers before any target I/O so close aborts the bridge.
fn bind_dc_handlers(
    channel: Arc<dyn DataChannel>,
    dc_tx: mpsc::Sender<Bytes>,
    close_tx: mpsc::Sender<()>,
    udp_max: Option<usize>,
) {
    let close_tx_msg = close_tx.clone();
    let on_message = Box::new(move |msg: RTCDataChannelMessage| {
        let dc_tx = dc_tx.clone();
        let close_tx_msg = close_tx_msg.clone();
        tokio::spawn(async move {
            if msg.data.is_empty() {
                return;
            }
            if let Some(max) = udp_max {
                if msg.data.len() > max {
                    warn!(
                        len = msg.data.len(),
                        max, "dropping oversized UDP datagram from DataChannel"
                    );
                    return;
                }
            }
            if dc_tx.send(Bytes::copy_from_slice(&msg.data)).await.is_err() {
                let _ = close_tx_msg.try_send(());
            }
        });
    });
    let on_close = Box::new(move || {
        let _ = close_tx.try_send(());
    });
    spawn_dc_events(
        channel,
        DcCallbacks {
            on_message: Some(on_message),
            on_close: Some(on_close),
            ..Default::default()
        },
    );
}

async fn bridge_tcp(
    channel: Arc<dyn DataChannel>,
    params: IpForwardParams,
    mut dc_rx: mpsc::Receiver<Bytes>,
    mut close_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let addr = format!("{}:{}", params.host, params.port);

    // Connect is aborted if the DataChannel closes first.
    let stream = tokio::select! {
        biased;
        _ = close_rx.recv() => {
            bail!(
                "{}",
                err_msg(
                    IpForwardError::ForwardBridgeFailed,
                    "DataChannel closed before TCP connect completed"
                )
            );
        }
        result = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)) => {
            match result {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    bail!(
                        "{}",
                        err_msg(
                            IpForwardError::ConnectFailed,
                            format!("TCP connect to {addr} failed: {e}")
                        )
                    );
                }
                Err(_) => {
                    bail!(
                        "{}",
                        err_msg(
                            IpForwardError::ConnectFailed,
                            format!("TCP connect to {addr} timed out")
                        )
                    );
                }
            }
        }
    };
    stream.set_nodelay(true).ok();
    let (mut tcp_read, mut tcp_write) = stream.into_split();

    let backpressure = DataChannelBackpressure::attach(&channel).await;
    let channel_out = channel.clone();
    let bp = backpressure.clone();
    let tcp_to_dc = async {
        let mut buf = vec![0u8; TCP_BUF];
        loop {
            let n = tcp_read.read(&mut buf).await.context("tcp read")?;
            if n == 0 {
                break;
            }
            bp.wait_writable(&channel_out).await;
            channel_out.send(BytesMut::from(&buf[..n])).await.context("dc send")?;
        }
        Ok::<(), anyhow::Error>(())
    };

    let dc_to_tcp = async {
        while let Some(chunk) = dc_rx.recv().await {
            tcp_write.write_all(&chunk).await.context("tcp write")?;
        }
        let _ = tcp_write.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = tcp_to_dc => {
            if let Err(e) = r {
                warn!("IpForward TCP->DC ended: {e}");
            }
        }
        r = dc_to_tcp => {
            if let Err(e) = r {
                warn!("IpForward DC->TCP ended: {e}");
            }
        }
        _ = close_rx.recv() => {
            info!("IpForward DataChannel closed; tearing down TCP");
        }
    }

    // Drop halves / leave scope → TCP socket closed; caller closes DataChannel.
    drop(dc_rx);
    Ok(())
}

async fn bridge_udp(
    channel: Arc<dyn DataChannel>,
    params: IpForwardParams,
    mut dc_rx: mpsc::Receiver<Bytes>,
    mut close_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let addr = format!("{}:{}", params.host, params.port);

    let socket = UdpSocket::bind("0.0.0.0:0").await.context("udp bind")?;

    tokio::select! {
        biased;
        _ = close_rx.recv() => {
            bail!(
                "{}",
                err_msg(
                    IpForwardError::ForwardBridgeFailed,
                    "DataChannel closed before UDP connect completed"
                )
            );
        }
        result = tokio::time::timeout(CONNECT_TIMEOUT, socket.connect(&addr)) => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    bail!(
                        "{}",
                        err_msg(
                            IpForwardError::ConnectFailed,
                            format!("UDP connect to {addr} failed: {e}")
                        )
                    );
                }
                Err(_) => {
                    bail!(
                        "{}",
                        err_msg(
                            IpForwardError::ConnectFailed,
                            format!("UDP connect to {addr} timed out")
                        )
                    );
                }
            }
        }
    }

    let backpressure = DataChannelBackpressure::attach(&channel).await;
    let channel_out = channel.clone();
    let bp = backpressure.clone();
    let socket_recv = Arc::new(socket);
    let socket_send = socket_recv.clone();

    let udp_to_dc = async {
        let mut buf = vec![0u8; UDP_MAX_DATAGRAM];
        loop {
            let n = socket_recv.recv(&mut buf).await.context("udp recv")?;
            if n == 0 {
                continue;
            }
            bp.wait_writable(&channel_out).await;
            channel_out.send(BytesMut::from(&buf[..n])).await.context("dc send")?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    let dc_to_udp = async {
        while let Some(chunk) = dc_rx.recv().await {
            socket_send.send(&chunk).await.context("udp send")?;
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = udp_to_dc => {
            if let Err(e) = r {
                warn!("IpForward UDP->DC ended: {e}");
            }
        }
        r = dc_to_udp => {
            if let Err(e) = r {
                warn!("IpForward DC->UDP ended: {e}");
            }
        }
        _ = close_rx.recv() => {
            info!("IpForward DataChannel closed; tearing down UDP");
        }
    }

    drop(dc_rx);
    Ok(())
}
