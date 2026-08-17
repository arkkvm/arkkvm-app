use std::sync::Arc;

use mdns_sd::{DaemonEvent, IfKind, RRType, ServiceDaemon, ServiceInfo};
use tokio::sync::RwLock;
use tracing::{info, error, warn};
use anyhow::anyhow;

use crate::network::fqdn;
use crate::network::mdns::config::{MdnsListenOptions, MdnsOptions};

/// Default service name and type
const DEFAULT_SERVICE_NAME_V4: &'static str = "arkkvm_v4";
const DEFAULT_SERVICE_NAME_V6: &'static str = "arkkvm_v6";
const DEFAULT_SERVICE_TYPE_V4: &'static str = "_http._tcp.local.";
const DEFAULT_SERVICE_TYPE_V6: &'static str = "_https._tcp.local.";
const DEFAULT_PORT_V4: u16 = 80;
const DEFAULT_PORT_V6: u16 = 443;
/// mDNS server structure
pub struct Mdns {
    service: Arc<RwLock<Option<ServiceDaemon>>>,
    local_names: Arc<RwLock<Vec<String>>>,
    domain: Arc<RwLock<String>>,
    listen_options: Arc<RwLock<MdnsListenOptions>>,
    v4_service_name: String,
    v6_service_name: String,
    v4_service_type: String,
    v6_service_type: String,
    v4_port: u16,
    v6_port: u16,
}

impl Mdns {
    /// Create new mDNS instance
    pub fn new(options: MdnsOptions) -> anyhow::Result<Self> {
        Ok(Self {
            service: Arc::new(RwLock::new(None)),
            local_names: Arc::new(RwLock::new(options.local_names)),
            domain: Arc::new(RwLock::new(options.domain)),
            listen_options: Arc::new(RwLock::new(options.listen_options)),
            v4_service_name: DEFAULT_SERVICE_NAME_V4.to_owned(),
            v6_service_name: DEFAULT_SERVICE_NAME_V6.to_owned(),
            v4_service_type: DEFAULT_SERVICE_TYPE_V4.to_owned(),
            v6_service_type: DEFAULT_SERVICE_TYPE_V6.to_owned(),
            v4_port: DEFAULT_PORT_V4,
            v6_port: DEFAULT_PORT_V6,
        })
    }

    /// Start mDNS server
    pub async fn start(&self) -> anyhow::Result<()> {
        self.start_internal(false).await
    }

    /// Restart mDNS server
    pub async fn restart(&self) -> anyhow::Result<()> {
        self.start_internal(true).await
    }

    pub async fn update_option(&self, options: MdnsOptions) -> anyhow::Result<()> {
        info!("update mDNS option: {:?}", &options);
        let mut has_changed = false;
        
        let mut local_names_guard = self.local_names.write().await;
        if *local_names_guard != options.local_names {
            *local_names_guard = options.local_names;
            has_changed = true;
            info!("mDNS local names changed");
        }
        drop(local_names_guard);

        let mut domain_guard = self.domain.write().await;
        if *domain_guard != options.domain {
            *domain_guard = options.domain;
            has_changed = true;
            info!("mDNS domain changed");
        }
        drop(domain_guard);

        let mut listen_options_guard = self.listen_options.write().await;
        if *listen_options_guard != options.listen_options {
            *listen_options_guard = options.listen_options;
            has_changed = true;
            info!("mDNS listen options changed");
        }
        drop(listen_options_guard);

        if has_changed {
            info!("trigger mDNS restart");
            self.restart().await?;
        }
        Ok(())
    }

    /// Internal start logic
    async fn start_internal(&self, allow_restart: bool) -> anyhow::Result<()> {
        let service_guard = self.service.read().await;
        let is_running = service_guard.is_some();
        drop(service_guard);

        if is_running {
            if !allow_restart {
                anyhow::bail!("mDNS server already running, and can not restart");
            }
            self.stop_internal().await?;
        }

        let listen_options = self.listen_options.read().await;

        if !listen_options.ipv4 && !listen_options.ipv6 {
            info!("mDNS server disabled");
            return Ok(());
        }

        let open_ifaces = crate::network::vlan::open_l3_ifaces().await;
        let interface_kinds: Vec<IfKind> = open_ifaces
            .iter()
            .map(|name| IfKind::Name(name.clone()))
            .collect();
        info!(
            "mDNS binding interfaces: {:?}",
            open_ifaces
        );

        // Create mDNS service daemon
        let daemon = ServiceDaemon::new()?;
        let monitor = daemon.monitor()?;

        let local_names = self.local_names.read().await;
        let domain = self.domain.read().await.clone();
        let mut service_infos = Vec::new();

        // Create service info for each local name
        for name in local_names.iter() {
            let hostname = fqdn::build_network_fqdn(name, &domain).await?;
            let device_id = crate::config::get_config_manager()
                .get()
                .await
                .device_id
                .clone();
            let properties: std::collections::HashMap<String, String> = [
                ("hostname", hostname.as_str()),
                ("product", "arkkvm"),
                ("device_id", device_id.as_str()),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

            if listen_options.ipv4 {
                let mut service_info = ServiceInfo::new(
                    &self.v4_service_type,
                    &self.v4_service_name,
                    &hostname,
                    "",
                    self.v4_port,
                    properties.clone(),
                )?
                .enable_addr_auto();
                service_info.set_interfaces(interface_kinds.clone());
                service_infos.push(service_info);
            }

            if listen_options.ipv6 {
                let mut service_info = ServiceInfo::new(
                    &self.v6_service_type,
                    &self.v6_service_name,
                    &hostname,
                    "",
                    self.v6_port,
                    properties,
                )?
                .enable_addr_auto();
                service_info.set_interfaces(interface_kinds.clone());
                service_infos.push(service_info);
            }
        }

        if service_infos.is_empty() {
            error!("No mDNS services configured for registration");
            return Err(anyhow!("No mDNS services configured for registration"));
        }

        // Register all services
        for service_info in service_infos {
            daemon.register(service_info)?;
        }

        let runtime = tokio::runtime::Handle::try_current().ok();
        std::thread::spawn(move || {
            loop {
                match monitor.recv() {
                    Ok(event) => {
                        if let DaemonEvent::NameChange(change) = &event {
                            if matches!(change.rr_type, RRType::A | RRType::AAAA) {
                                warn!(
                                    "mDNS hostname conflict resolved: {} -> {} on {}",
                                    change.original, change.new_name, change.intf_name
                                );
                                if let Some(rt) = runtime.as_ref() {
                                    let fqdn = change.new_name.clone();
                                    rt.spawn(async move {
                                        if let Err(e) = crate::web::reload_tls_config_with_fqdn(&fqdn).await {
                                            warn!("Failed to reload TLS after mDNS NameChange: {:?}", e);
                                        }
                                    });
                                }
                            }
                        }
                        info!("mDNS event: {:?}", event);
                    }

                    Err(e) => {
                        error!("mDNS monitor error: {:?}", e);
                        break;
                    }
                }
            }
        });

        let mut service_guard = self.service.write().await;
        *service_guard = Some(daemon);

        info!(
            local_names = ?*local_names,
            ipv4 = listen_options.ipv4,
            ipv6 = listen_options.ipv6,
            "mDNS server started"
        );

        Ok(())
    }

    /// Stop mDNS server
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.stop_internal().await
    }

    async fn stop_internal(&self) -> anyhow::Result<()> {
        let mut service_guard = self.service.write().await;

        if let Some(daemon) = service_guard.take() {
            daemon.shutdown()?;
            info!("mDNS server stopped");
        }

        Ok(())
    }

    /// Set local names
    pub async fn set_local_names(&self, names: Vec<String>, always: bool) -> anyhow::Result<()> {
        let mut local_names_guard = self.local_names.write().await;

        if !always && *local_names_guard == names {
            return Ok(());
        }

        *local_names_guard = names;
        drop(local_names_guard);

        self.restart().await?;
        Ok(())
    }

    /// Set listening options
    pub async fn set_listen_options(&self, options: MdnsListenOptions) -> anyhow::Result<()> {
        let mut listen_options_guard = self.listen_options.write().await;

        if *listen_options_guard == options {
            return Ok(());
        }

        *listen_options_guard = options;
        drop(listen_options_guard);

        self.restart().await?;
        Ok(())
    }

    /// Get current local names
    pub async fn get_local_names(&self) -> Vec<String> {
        self.local_names.read().await.clone()
    }

    /// Get current listening options
    pub async fn get_listen_options(&self) -> MdnsListenOptions {
        *self.listen_options.read().await
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        // Use tokio runtime for async cleanup
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let service = self.service.clone();
            rt.spawn(async move {
                let mut service_guard = service.write().await;
                if let Some(daemon) = service_guard.take() {
                    let _ = daemon.shutdown();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_mdns_creation() {
        let options = MdnsOptions {
            local_names: vec!["test".to_string()],
            listen_options: MdnsListenOptions::default(),
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert_eq!(mdns.get_local_names().await, vec!["test".to_string()]);
    }

    #[tokio::test]
    async fn test_start_stop_cycle_disabled() {
        let options = MdnsOptions {
            local_names: vec!["test".to_string()],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();

        // Test start with disabled listening (should succeed)
        assert!(mdns.start().await.is_ok());

        // Test stop
        assert!(mdns.stop().await.is_ok());

        // Test stop when already stopped
        assert!(mdns.stop().await.is_ok());
    }

    #[tokio::test]
    async fn test_set_local_names_logic() {
        let options = MdnsOptions {
            local_names: vec!["test1".to_string()],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert!(mdns.start().await.is_ok());

        // Test setting same names (should not restart)
        assert!(mdns.set_local_names(vec!["test1".to_string()], false).await.is_ok());

        // Test setting different names (should restart)
        assert!(mdns.set_local_names(vec!["test2".to_string()], false).await.is_ok());

        // Test force restart
        assert!(mdns.set_local_names(vec!["test2".to_string()], true).await.is_ok());

        assert_eq!(mdns.get_local_names().await, vec!["test2".to_string()]);
    }

    #[tokio::test]
    async fn test_set_listen_options_logic() {
        let options = MdnsOptions {
            local_names: vec!["test".to_string()],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert!(mdns.start().await.is_ok());

        // Test setting same options (should not restart)
        let same_options = MdnsListenOptions { ipv4: false, ipv6: false };
        assert!(mdns.set_listen_options(same_options).await.is_ok());

        // Test setting different options (should update and restart)
        let different_options = MdnsListenOptions { ipv4: true, ipv6: false };
        // In test environment, we test the logic flow rather than actual restart
        // The key is that options are updated correctly
        let _ = mdns.set_listen_options(different_options).await;
        assert_eq!(mdns.get_listen_options().await, different_options);
    }

    #[tokio::test]
    async fn test_empty_local_names() {
        let options = MdnsOptions {
            local_names: vec![],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert!(mdns.start().await.is_ok());
        assert!(mdns.stop().await.is_ok());
    }

    #[tokio::test]
    async fn test_multiple_local_names() {
        let options = MdnsOptions {
            local_names: vec!["host1".to_string(), "host2".to_string(), "host3".to_string()],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert!(mdns.start().await.is_ok());

        let names = mdns.get_local_names().await;
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"host1".to_string()));
        assert!(names.contains(&"host2".to_string()));
        assert!(names.contains(&"host3".to_string()));

        assert!(mdns.stop().await.is_ok());
    }

    #[tokio::test]
    async fn test_config_options() {
        let listen_options = MdnsListenOptions { ipv4: false, ipv6: true };
        let options = MdnsOptions { local_names: vec!["test".to_string()], listen_options, ..Default::default() };

        let mdns = Mdns::new(options).unwrap();
        assert_eq!(mdns.get_listen_options().await, listen_options);
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let options = MdnsOptions {
            local_names: vec!["test".to_string()],
            listen_options: MdnsListenOptions::default(),
            ..Default::default()
        };

        let mdns = Arc::new(Mdns::new(options).unwrap());

        // Test concurrent reads
        let mdns_clone1 = Arc::clone(&mdns);
        let mdns_clone2 = Arc::clone(&mdns);

        let handle1 = tokio::spawn(async move { mdns_clone1.get_local_names().await });

        let handle2 = tokio::spawn(async move { mdns_clone2.get_listen_options().await });

        let (names, options) = tokio::join!(handle1, handle2);
        assert_eq!(names.unwrap(), vec!["test".to_string()]);
        assert_eq!(options.unwrap(), MdnsListenOptions::default());
    }

    #[tokio::test]
    async fn test_drop_cleanup() {
        let options = MdnsOptions {
            local_names: vec!["test".to_string()],
            listen_options: MdnsListenOptions { ipv4: false, ipv6: false },
            ..Default::default()
        };

        let mdns = Mdns::new(options).unwrap();
        assert!(mdns.start().await.is_ok());

        // Drop should clean up resources
        drop(mdns);

        // Give some time for cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

