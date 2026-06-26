use crate::control::zenoh::send_udc_state_event;
use crate::events::{LifecycleEvent, UsbLifecycleHandler};
use crate::udc::udc_state_to_sysfs;
use tracing::warn;

pub struct ZenohUdcReporter;

impl UsbLifecycleHandler for ZenohUdcReporter {
    fn name(&self) -> &str {
        "zenoh_udc"
    }

    fn on_lifecycle_event(&self, event: &LifecycleEvent) {
        let LifecycleEvent::UdcStateChanged(state) = event else {
            return;
        };
        let state_str = udc_state_to_sysfs(*state).to_string();
        tokio::spawn(async move {
            if let Err(e) = send_udc_state_event(&state_str).await {
                warn!("failed to publish UDC state via zenoh: {}", e);
            }
        });
    }
}
