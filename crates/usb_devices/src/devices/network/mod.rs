//! USB network adapter (NCM / RNDIS): gadget function + board L3 + DHCP.

mod adapter_l3;
mod dhcp;
pub mod ncm;
pub mod rndis;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, info, warn};
use usb_gadget::function::net::Net;
use usb_gadget::function::Handle;

use crate::error::UsbError;
use crate::events::{ChangeRequest, ChangeResponse, UsbChangeHandler};
use crate::proto::v1::{NetworkAdapterL3, NetworkAdapterType};

use adapter_l3::{apply_board_l3, flush_and_down, AdapterL3Settings};
use dhcp::{derive_dhcp_pool, dhcp_start, dhcp_stop};

pub use adapter_l3::AdapterL3Settings as NetworkAdapterL3Settings;
pub use dhcp::{
    read_active_leases, resolve_controlled_host_ip, select_controlled_host_lease, DhcpLeaseEntry,
    LEASE_FILE,
};

const SERVICE_NAME: &str = "network_adapter";
const IFACE_WAIT_ATTEMPTS: u32 = 20;
const IFACE_WAIT_MS: u64 = 250;

/// Build the USB net function for the selected adapter type (default NCM).
pub fn build_net_function(adapter_type: NetworkAdapterType) -> (Net, Handle) {
    match adapter_type {
        NetworkAdapterType::Rndis => rndis::build_function(),
        _ => ncm::build_function(),
    }
}

pub fn adapter_type_from_i32(v: i32) -> NetworkAdapterType {
    NetworkAdapterType::try_from(v).unwrap_or(NetworkAdapterType::Ncm)
}

/// Apply function-level MS OS Extended Compatibility IDs (RNDIS only; NCM no-op).
fn apply_ms_os_compat(net: &Net, adapter_type: NetworkAdapterType) -> Result<(), UsbError> {
    match adapter_type {
        NetworkAdapterType::Rndis => rndis::apply_ms_os_compat(net),
        _ => Ok(()),
    }
}

/// Gadget 已 `register()`、尚未写 UDC 时由 Manager 调用一次。
/// MS OS / adapter_type 细节留在 network 内；Manager 只负责这个时序缝。
pub fn on_gadget_registered(
    net: Option<&Net>,
    switches: &crate::proto::v1::DeviceSwitches,
) -> Result<(), UsbError> {
    let Some(net) = net else {
        return Ok(());
    };
    let adapter_type = adapter_type_from_i32(
        switches
            .network_adapter
            .as_ref()
            .map(|n| n.adapter_type)
            .unwrap_or(0),
    );
    apply_ms_os_compat(net, adapter_type)
}

pub struct NetworkAdapterController {
    handle: Arc<NetworkAdapterHandle>,
}

impl NetworkAdapterController {
    pub fn new(handle: Arc<NetworkAdapterHandle>) -> Self {
        Self { handle }
    }

    pub fn ifname(&self) -> Option<String> {
        self.handle.ifname()
    }

    pub async fn apply_l3(&self) -> Result<(), UsbError> {
        self.handle.apply_l3().await
    }
}

pub struct NetworkAdapterHandle {
    name: String,
    enabled: AtomicBool,
    udc_updating: AtomicBool,
    net: Mutex<Option<Net>>,
    ifname: Mutex<Option<String>>,
    l3: Mutex<AdapterL3Settings>,
}

impl NetworkAdapterHandle {
    pub fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            name: SERVICE_NAME.to_owned(),
            enabled: AtomicBool::new(enabled),
            udc_updating: AtomicBool::new(false),
            net: Mutex::new(None),
            ifname: Mutex::new(None),
            l3: Mutex::new(AdapterL3Settings {
                ip_address: "172.16.0.1".into(),
                subnet_mask: "255.255.255.0".into(),
                gateway: None,
                dns_servers: Vec::new(),
            }),
        })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_net(&self, net: Option<Net>) {
        let clear = net.is_none();
        if let Ok(mut g) = self.net.lock() {
            *g = net;
        }
        if clear {
            self.clear_ifname();
        }
    }

    pub fn set_l3_from_proto(&self, cfg: Option<&NetworkAdapterL3>) {
        let Some(c) = cfg else {
            return;
        };
        if c.ip_address.is_empty() && c.subnet_mask.is_empty() {
            return;
        }
        if let Ok(mut g) = self.l3.lock() {
            *g = AdapterL3Settings::from(c);
        }
    }

    pub fn l3_settings(&self) -> AdapterL3Settings {
        self.l3.lock().ok().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn ifname(&self) -> Option<String> {
        self.ifname.lock().ok().and_then(|g| g.clone())
    }

    fn clear_ifname(&self) {
        if let Ok(mut g) = self.ifname.lock() {
            *g = None;
        }
    }

    fn refresh_ifname_from_net(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            self.clear_ifname();
            return;
        }
        let name = self
            .net
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|n| n.ifname().ok()))
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty());
        match name {
            Some(name) => {
                info!("network adapter ifname ready: {}", name);
                if let Ok(mut g) = self.ifname.lock() {
                    *g = Some(name);
                }
            }
            None => {
                warn!("network adapter ifname resolve via usb-gadget Net failed");
                self.clear_ifname();
            }
        }
    }

    /// Apply or clear board L3; when enabled, start shared DHCP. `enable=false` stops DHCP.
    pub async fn apply_l3(&self) -> Result<(), UsbError> {
        if !self.enabled.load(Ordering::Acquire) {
            dhcp_stop().await;
            if let Some(ifname) = self.ifname() {
                flush_and_down(&ifname).await;
                info!(ifname = %ifname, "network adapter disabled; flushed board L3 and stopped DHCP");
            }
            return Ok(());
        }

        dhcp_stop().await;

        self.refresh_ifname_from_net();
        let ifname = wait_for_ifname(self).await.ok_or_else(|| {
            UsbError::DeviceNotFound("network adapter enabled but netdev ifname not found".into())
        })?;

        let cfg = self.l3_settings();
        apply_board_l3(&ifname, &cfg).await?;

        match derive_dhcp_pool(&cfg.ip_address, &cfg.subnet_mask) {
            Ok(pool) => {
                if let Err(e) = dhcp_start(&ifname, &cfg, &pool).await {
                    warn!(error = %e, ifname = %ifname, "network adapter DHCP start failed (L3 kept)");
                }
            }
            Err(e) => warn!(error = %e, "network adapter DHCP pool derive failed; skip DHCP"),
        }

        Ok(())
    }
}

async fn wait_for_ifname(handle: &NetworkAdapterHandle) -> Option<String> {
    for _ in 0..IFACE_WAIT_ATTEMPTS {
        handle.refresh_ifname_from_net();
        if let Some(name) = handle.ifname() {
            return Some(name);
        }
        tokio::time::sleep(Duration::from_millis(IFACE_WAIT_MS)).await;
    }
    None
}

#[async_trait]
impl UsbChangeHandler for NetworkAdapterHandle {
    fn name(&self) -> &str {
        &self.name
    }

    async fn on_change_request(&self, req: &ChangeRequest) -> ChangeResponse {
        match req {
            ChangeRequest::RequestChange => ChangeResponse::Proceed,
            ChangeRequest::PrepareChange => {
                self.udc_updating.store(true, Ordering::Release);
                dhcp_stop().await;
                if let Some(ifname) = self.ifname() {
                    flush_and_down(&ifname).await;
                    debug!("network adapter PrepareChange: flushed {}", ifname);
                }
                self.set_net(None);
                self.clear_ifname();
                ChangeResponse::Proceed
            }
            ChangeRequest::ChangeCompleted => {
                self.udc_updating.store(false, Ordering::Release);
                if let Err(e) = self.apply_l3().await {
                    warn!("network adapter ChangeCompleted apply_l3 failed: {}", e);
                }
                ChangeResponse::Proceed
            }
            ChangeRequest::ChangeCanceled => {
                self.udc_updating.store(false, Ordering::Release);
                if let Err(e) = self.apply_l3().await {
                    warn!("network adapter ChangeCanceled apply_l3 failed: {}", e);
                }
                ChangeResponse::Proceed
            }
        }
    }
}
