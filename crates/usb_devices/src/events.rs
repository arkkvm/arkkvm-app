use async_trait::async_trait;

/// Change request events
#[derive(Debug, Clone)]
pub enum ChangeRequest {
    /// Request a change; upper layers confirm whether it may proceed.
    /// After approval, the manager continues with the low-level rebuild.
    RequestChange,
    /// Prepare for a structural change (e.g. VID/PID update, add/remove devices).
    /// Upper layers must stop related I/O and release file handles.
    PrepareChange,
    /// Change completed; low-level layer has re-bound. Upper layers may reopen handles.
    ChangeCompleted,
    /// Change canceled; low-level rebuild is aborted. Upper layers must recover prior state.
    ChangeCanceled,
}

/// Change response
#[derive(Debug, Clone)]
pub enum ChangeResponse {
    /// Allow the change; upper layer has released related resources.
    Proceed,
    /// Reject the change.
    /// Includes a specific reason (e.g. "BIOS upgrade in progress, USB disconnect not allowed").
    /// Low-level rebuild is aborted and surfaced via `UsbError::ChangeRejected(reason)`.
    Reject(String),
}

/// USB UDC physical connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdcState {
    NotAttached,
    Attached,
    Powered,
    Default,
    Address,
    Configured,
    Suspended,
}

/// Lifecycle and status report events
#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    /// USB physical connection state changed
    UdcStateChanged(UdcState),
    /// Non-fatal low-level error (e.g. mount failure when strict_mode=false)
    Warning(String),
}

/// Change-request subscriber (blocking decision)
///
/// The manager awaits this callback before low-level rebuild and proceeds based on the response.
#[async_trait]
pub trait UsbChangeHandler: Send + Sync {
    /// Unique handler name
    fn name(&self) -> &str;

    /// Handle a change request (blocking)
    /// The manager awaits this return value before continuing the low-level rebuild.
    async fn on_change_request(&self, req: &ChangeRequest) -> ChangeResponse;
}

/// Lifecycle/status subscriber (non-blocking one-way notification)
///
/// The manager notifies only; it does not wait for handling to finish.
pub trait UsbLifecycleHandler: Send + Sync {
    /// Unique handler name
    fn name(&self) -> &str;

    /// Handle a lifecycle report (non-blocking)
    fn on_lifecycle_event(&self, event: &LifecycleEvent);
}
