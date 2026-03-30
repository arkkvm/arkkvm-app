use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{debug, info};

use crate::{jsonrpc, zenoh_bus};

#[derive(Debug, Deserialize, Serialize)]
pub enum ATXPowerAction {
    #[serde(rename = "power-long")]
    PowerLong,
    #[serde(rename = "power-short")]
    PowerShort,
    #[serde(rename = "reset")]
    Reset,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ATXLedState {
    pub power: bool,
    pub hdd: bool,
}

pub async fn on_atx_power_action(action: ATXPowerAction) -> Result<()> {
    match action {
        ATXPowerAction::PowerLong => {
            // TODO: Forced Shutdown
        }
        ATXPowerAction::PowerShort => {
            // TODO: Turn off ATX power
        }
        ATXPowerAction::Reset => {
            // TODO: Reset ATX power
        }
    }
    Err(anyhow!("Not implemented"))
}

const DEFAULT_TTY: &str = "/dev/ttyS5";

pub async fn init() -> anyhow::Result<()> {
    let session = zenoh_bus::get_session();

    let mut port = tokio_serial::new(DEFAULT_TTY, 115200).open_native_async()?;

    #[cfg(unix)]
    port.set_exclusive(false).expect("Unable to set serial port exclusive to false");

    let framed_port = Framed::new(port, LinesCodec::new());
    let (mut port_tx, mut port_rx) = framed_port.split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            debug!("Send frame to ATX controller: {:?}", frame);
            port_tx.send(format!("{}\n", frame)).await.expect("Failed to send frame");
        }
    });

    let session_cloned = session.clone();
    tokio::spawn(async move {
        let mut last_state: Option<ATXLedState> = None;
        while let Some(msg_result) = port_rx.next().await {
            let msg = msg_result.expect("Failed to read frame");
            debug!("Received frame from ATX controller: {:?}", msg);
            // "NAME/ATX_CONTROLLER"
            // "STATUS/HDD_LED=0,PWR_LED=0,PWR_BTN=0,RST_BTN=0"
            if msg.starts_with("STATUS/") {
                let new_state = ATXLedState {
                    power: msg.contains("PWR_LED=1"),
                    hdd: msg.contains("HDD_LED=1"),
                };

                let changed = if let Some(last_state) = last_state.as_ref() {
                    last_state != &new_state
                }
                else {
                    true
                };

                if changed {
                    let result = jsonrpc::broadcast_atx_led_state(&new_state).await;
                    if result.is_ok() {
                        last_state = Some(new_state);
                    }
                    else if last_state.is_some() {
                        last_state = None;
                    }
                }
                let _ = session_cloned.put("", "").await;

            } else if msg.starts_with("NAME/") {
            }
        }
    });

    let subscriber = session
        .declare_subscriber("extension/atx/action/*")
        .await
        .expect("Failed to declare subscriber");

    let tx1 = tx.clone();
    tokio::spawn(async move {
        while let Ok(sample) = subscriber.recv_async().await {
            let key = sample.key_expr().as_str();
            match key {
                "extension/atx/action/pwr-btn-long-press" => {
                    tx1.send("SET/PWR_BTN/ON").unwrap();
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    tx1.send("SET/PWR_BTN/OFF").unwrap();
                }
                "extension/atx/action/pwr-btn-short-press" => {
                    tx1.send("SET/PWR_BTN/ON").unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    tx1.send("SET/PWR_BTN/OFF").unwrap();
                }
                "extension/atx/action/rst-btn-short-press" => {
                    tx1.send("SET/RST_BTN/ON").unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    tx1.send("SET/RST_BTN/OFF").unwrap();
                }
                &_ => {
                    info!("Unknown ATX action: {}", key);
                }
            }
        }
    });

    // Get extended status every second
    let tx2 = tx.clone();
    tokio::spawn(async move {
        loop {
            tx2.send("GET/STATUS").expect("Failed to send frame");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    Ok(())
}
