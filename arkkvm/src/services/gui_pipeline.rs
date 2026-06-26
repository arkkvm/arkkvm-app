use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Duration, timeout};
use tracing::{error, info};

use crate::jsonrpc;
use crate::network::RpcIPv6Address;
use crate::network::vlan::{self, VlanConfigReport, VlanIpv6AddressesEntry};

const SOCKET_PATH: &str = "/tmp/arkkvm_ui.sock";

lazy_static::lazy_static! {
    pub static ref GUI_PIPELINE: OnceCell<Arc<Mutex<IpcClient>>> = OnceCell::new();
    pub static ref RUNNING: AtomicBool = AtomicBool::new(true);
}

// Config data structures
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub orientation: i32,
    pub luminance: i32,
    pub dark_screen_time: i32,
    pub sleep_time: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            orientation: 0,
            luminance: 64,
            dark_screen_time: 0,
            sleep_time: 1800,
        }
    }
}

#[derive(Debug)]
pub enum Command {
    GetConfig,
    SetOrientation(i32),
    SetLuminance(i32),
    SetDarkScreenTime(i32),
    SetSleepTime(i32),
    ReportIpv6Addresses(String),
    ReportVlanConfig(String),
    ReportVlanIpv6Addresses(String),
}

impl Command {
    fn to_protocol_string(&self) -> String {
        match self {
            Command::GetConfig => "GET_CONFIG".to_string(),
            Command::SetOrientation(value) => format!("SCREEN_ORIENTATION={}", value),
            Command::SetLuminance(value) => format!("SCREEN_LUMINANCE={}", value),
            Command::SetDarkScreenTime(value) => format!("DARK_SCREEN_TIME={}", value),
            Command::SetSleepTime(value) => format!("SLEEP_TIME={}", value),
            Command::ReportIpv6Addresses(addresses) => format!("REPORT_IPV6_ADDRESSES={}", addresses),
            Command::ReportVlanConfig(config) => format!("REPORT_VLAN_CONFIG={}", config),
            Command::ReportVlanIpv6Addresses(addresses) => {
                format!("REPORT_VLAN_IPV6_ADDRESSES={}", addresses)
            }
        }
    }
}

pub fn init() -> anyhow::Result<()> {
    RUNNING.store(true, std::sync::atomic::Ordering::Release);
     tokio::spawn(async {
        loop {
            let exists = match tokio::fs::try_exists(SOCKET_PATH).await {
                Ok(exists) => exists,
                Err(e) => {
                    error!("Failed to check if socket exists: {:?}", e);
                    false
                },
            };

            if exists {
                break;
            }

            if !RUNNING.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        
        let ipc_client = Arc::new(Mutex::new(IpcClient::new(SOCKET_PATH)));
        if let Err(e) = GUI_PIPELINE.set(ipc_client.clone()) {
            error!("Failed to set GUI_PIPELINE: {:?}", e);
        }

        info!("GUI_PIPELINE initialized");
        loop {
            if !RUNNING.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }

            {
                let mut client = ipc_client.lock().await;
                let ipv6_addresses = jsonrpc::handlers::get_ipv6_addresses().await;
                if let Err(e) = client.set_ipv6_addresses(&ipv6_addresses).await {
                    error!("Failed to set IPv6 addresses: {:?}", e);
                }
                let vlan_config = vlan::get_effective_vlan_config_report().await;
                if let Err(e) = client.set_vlan_config(&vlan_config).await {
                    error!("Failed to set VLAN config: {:?}", e);
                }
                let vlan_ipv6_addresses = vlan::get_effective_vlan_ipv6_addresses_report().await;
                if let Err(e) = client
                    .set_vlan_ipv6_addresses(&vlan_ipv6_addresses)
                    .await
                {
                    error!("Failed to set VLAN IPv6 addresses: {:?}", e);
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        drop(ipc_client);
        info!("GUI_PIPELINE uninitialized");
     });
     Ok(())
}

pub fn uninit() {
    RUNNING.store(false, std::sync::atomic::Ordering::Release);
}

pub async fn get_config() -> Result<ServerConfig> {
    let Some(pipeline) = GUI_PIPELINE.get() else {
        return Err(anyhow!("GUI_PIPELINE not set"));
    };

    Ok(pipeline.lock().await.get_config().await?)
}

pub async fn set_orientation(orientation: i32) -> Result<()> {
    let Some(pipeline) = GUI_PIPELINE.get() else {
        return Err(anyhow!("GUI_PIPELINE not set"));
    };

    let _ = pipeline.lock().await.set_orientation(orientation).await;
    Ok(())
}

pub async fn set_brightness(brightness: i32) -> Result<()> {
    let Some(pipeline) = GUI_PIPELINE.get() else {
        return Err(anyhow!("GUI_PIPELINE not set"));
    };

    let _ = pipeline.lock().await.set_luminance(brightness).await;
    Ok(())
}

pub async fn set_sleep_time(seconds: i32) -> Result<()> {
    let Some(pipeline) = GUI_PIPELINE.get() else {
        return Err(anyhow!("GUI_PIPELINE not set"));
    };

    pipeline.lock().await.set_sleep_time(seconds).await?;
    Ok(())
}

pub async fn set_dark_screen_time(seconds: i32) -> Result<()> {
    let Some(pipeline) = GUI_PIPELINE.get() else {
        return Err(anyhow!("GUI_PIPELINE not set"));
    };

    pipeline.lock().await.set_dark_screen_time(seconds).await?;
    Ok(())
}

// IPC client
#[derive(Debug)]
pub struct IpcClient {
    socket_path: String,
}

impl IpcClient {
    fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    async fn send_command(&self, command: Command) -> Result<String> {
        // connect to Unix domain socket server
        let mut stream = UnixStream::connect(&self.socket_path).await?;

        // build command string
        let command_str = command.to_protocol_string();

        // overall read/write timeout (e.g. 5 seconds)
        match timeout(Duration::from_secs(5), async {
            // send command to server
            stream.write_all(command_str.as_bytes()).await?;

            // read server response
            let mut buffer = vec![0; 1024];
            let n = stream.read(&mut buffer).await?;

            if n == 0 {
                return Err(anyhow!("Connection closed by server"));
            }

            let response = String::from_utf8_lossy(&buffer[..n]).to_string();
            Ok(response)
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Operation timed out")),
        }
    }

    // fetch server config
    pub async fn get_config(&self) -> Result<ServerConfig> {
        let response = self.send_command(Command::GetConfig).await?;

        // response format: ORIENTATION=1,LUMINANCE=80,DARK_SCREEN_TIME=30,SLEEP_TIME=300
        let config_parts: Vec<&str> = response.split(',').collect();

        let mut config = ServerConfig::default();
        for part in config_parts {
            let key_value: Vec<&str> = part.split('=').collect();
            if key_value.len() == 2 {
                let value = key_value[1].parse().unwrap_or(0);
                match key_value[0] {
                    "ORIENTATION" => config.orientation = value,
                    "LUMINANCE" => config.luminance = value,
                    "DARK_SCREEN_TIME" => config.dark_screen_time = value,
                    "SLEEP_TIME" => config.sleep_time = value,
                    _ => {}
                }
            }
        }

        Ok(config)
    }

    pub async fn set_ipv6_addresses(&self, ipv6_addresses: &Vec<RpcIPv6Address>) -> Result<()> {
        let response = self.send_command(Command::ReportIpv6Addresses(serde_json::to_string(ipv6_addresses)?)).await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    pub async fn set_vlan_config(&self, vlan_config: &VlanConfigReport) -> Result<()> {
        let response = self
            .send_command(Command::ReportVlanConfig(serde_json::to_string(vlan_config)?))
            .await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    pub async fn set_vlan_ipv6_addresses(
        &self,
        vlan_ipv6_addresses: &[VlanIpv6AddressesEntry],
    ) -> Result<()> {
        let response = self
            .send_command(Command::ReportVlanIpv6Addresses(serde_json::to_string(
                vlan_ipv6_addresses,
            )?))
            .await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    // set screen orientation
    pub async fn set_orientation(&self, orientation: i32) -> Result<()> {
        let response = self
            .send_command(Command::SetOrientation(orientation))
            .await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    // set screen brightness
    pub async fn set_luminance(&self, luminance: i32) -> Result<()> {
        let response = self.send_command(Command::SetLuminance(luminance)).await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    // set dim-screen timeout
    pub async fn set_dark_screen_time(&self, seconds: i32) -> Result<()> {
        let response = self
            .send_command(Command::SetDarkScreenTime(seconds))
            .await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response).into())
        }
    }

    // set sleep/screen-off timeout
    pub async fn set_sleep_time(&self, seconds: i32) -> Result<()> {
        let response = self.send_command(Command::SetSleepTime(seconds)).await?;
        if response == "OK" {
            Ok(())
        } else {
            Err(anyhow!("Server returned error: {}", response))
        }
    }
}