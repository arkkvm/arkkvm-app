//! Cross-role Control lease (View vs Control) for Human / Agent conflict.
//!
//! View stays on per-role WebRTC slots; HID requires holding `control_owner`.
//! Arbitration uses a single pending request + cancellable 120s timeout.
//! The pending requester can never grant its own request: transfer happens
//! only via the other side's consent (user WebUI) or the 120s silence timeout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::jsonrpc::PROCESSOR;
use crate::session::{Session, SessionRole};
use crate::web::get_global_app_state;

pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
pub const CONTROL_DENIED: &str = "controlDenied";
pub const CONTROL_BUSY: &str = "controlBusy";
pub const CONTROL_STALE: &str = "controlStale";
/// Agent control refused because device MCP switch is off (ARKKVM-619).
pub const MCP_DISABLED: &str = "mcpDisabled";

/// What happens when the 120s timer fires with no grant/deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPolicy {
    /// Human → Agent: grant control to the requester.
    GrantRequester,
    /// Agent → Agent (future / explicit): leave owner unchanged.
    DenyRequester,
}

#[derive(Debug, Clone)]
struct PendingRequest {
    request_id: String,
    requester: SessionRole,
    policy: TimeoutPolicy,
    generation: u64,
    deadline: Instant,
    reason: String,
}

#[derive(Debug, Default)]
struct ControlState {
    owner: Option<SessionRole>,
    pending: Option<PendingRequest>,
}

lazy_static::lazy_static! {
    static ref LEASE: Mutex<ControlState> = Mutex::new(ControlState::default());
}

static HID_DENY_LOG_COUNTER: AtomicU64 = AtomicU64::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlStateView {
    pub owner: Option<&'static str>,
    pub pending: Option<PendingView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    pub request_id: String,
    pub requester: &'static str,
    pub reason: String,
    pub deadline_ms: u64,
    pub timeout_policy: &'static str,
}

fn role_str(role: SessionRole) -> &'static str {
    role.as_str()
}

fn policy_str(policy: TimeoutPolicy) -> &'static str {
    match policy {
        TimeoutPolicy::GrantRequester => "grantRequester",
        TimeoutPolicy::DenyRequester => "denyRequester",
    }
}

fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

fn snapshot(state: &ControlState) -> ControlStateView {
    ControlStateView {
        owner: state.owner.map(role_str),
        pending: state.pending.as_ref().map(|p| {
            let remaining = p.deadline.saturating_duration_since(Instant::now());
            PendingView {
                request_id: p.request_id.clone(),
                requester: role_str(p.requester),
                reason: p.reason.clone(),
                deadline_ms: remaining.as_millis() as u64,
                timeout_policy: policy_str(p.policy),
            }
        }),
    }
}

async fn notify_roles(method: &str, params: Value, roles: &[SessionRole]) {
    let app = get_global_app_state();
    for role in roles {
        if let Some(session) = app.get_session_for_role(*role).await {
            if let Err(e) = PROCESSOR.send_event(method, Some(params.clone()), session).await {
                warn!("Failed to send {} to {}: {}", method, role, e);
            }
        }
    }
}

async fn notify_control_changed(owner: Option<SessionRole>, reason: &str) {
    let params = json!({
        "owner": owner.map(role_str),
        "reason": reason,
    });
    notify_roles("controlChanged", params, &[SessionRole::User, SessionRole::Agent]).await;
}

async fn notify_control_conflict(view: &ControlStateView) {
    let params = serde_json::to_value(view).unwrap_or_else(|_| json!({}));
    notify_roles("controlConflict", params, &[SessionRole::User, SessionRole::Agent]).await;
}

/// After a role slot is claimed successfully.
pub async fn on_role_claimed(session: &Arc<Session>, previous_user: Option<&Arc<Session>>) {
    let role = session.role;
    // Resolve online peers before taking the lease lock (no await while locked).
    let agent_online = role_still_online(SessionRole::Agent).await;
    let user_online = role_still_online(SessionRole::User).await;
    let mut events: Vec<ControlEvent> = Vec::new();

    {
        let mut guard = LEASE.lock().await;
        match role {
            SessionRole::User => {
                // Human–Human takeover: new user always owns control.
                if previous_user.is_some() {
                    clear_pending_locked(&mut guard);
                    guard.owner = Some(SessionRole::User);
                    events.push(ControlEvent::Changed {
                        owner: Some(SessionRole::User),
                        reason: "userTakeover".into(),
                    });
                } else if guard.owner == Some(SessionRole::Agent) && agent_online {
                    // Agent holds control: human gets View only; UI must call takeOverControl.
                    // No pending/timer — manual take-over only.
                    let view = snapshot(&guard);
                    events.push(ControlEvent::ConflictHint {
                        view,
                        reason: "agentHasControl".into(),
                    });
                } else {
                    clear_pending_locked(&mut guard);
                    guard.owner = Some(SessionRole::User);
                    events.push(ControlEvent::Changed {
                        owner: Some(SessionRole::User),
                        reason: "userClaimed".into(),
                    });
                }
            }
            SessionRole::Agent => {
                let owner_online = match guard.owner {
                    Some(SessionRole::User) => user_online,
                    Some(SessionRole::Agent) => agent_online,
                    None => false,
                };
                if guard.owner.is_none() || !owner_online {
                    clear_pending_locked(&mut guard);
                    guard.owner = Some(SessionRole::Agent);
                    events.push(ControlEvent::Changed {
                        owner: Some(SessionRole::Agent),
                        reason: "agentClaimed".into(),
                    });
                }
                // else: human keeps control; agent is View-only until requestControl
            }
        }
        info!("control lease after claim role={}: {:?}", role, snapshot(&guard));
    }

    dispatch_events(events).await;
}

/// Session removed from AppState (after role slot cleared for this id).
pub async fn on_session_removed(session: &Session) {
    let role = session.role;
    let user_online = role_still_online(SessionRole::User).await;
    let agent_online = role_still_online(SessionRole::Agent).await;
    let role_online = match role {
        SessionRole::User => user_online,
        SessionRole::Agent => agent_online,
    };
    let mut events: Vec<ControlEvent> = Vec::new();

    {
        let mut guard = LEASE.lock().await;
        let owner_gone = guard.owner == Some(role) && !role_online;
        let pending_requester_gone =
            guard.pending.as_ref().is_some_and(|p| p.requester == role && !role_online);

        if pending_requester_gone {
            clear_pending_locked(&mut guard);
            events.push(ControlEvent::Changed {
                owner: guard.owner,
                reason: "requesterDisconnected".into(),
            });
        }

        if owner_gone {
            clear_pending_locked(&mut guard);
            let new_owner = if user_online {
                Some(SessionRole::User)
            } else if agent_online {
                Some(SessionRole::Agent)
            } else {
                None
            };
            guard.owner = new_owner;
            events.push(ControlEvent::Changed {
                owner: new_owner,
                reason: "ownerDisconnected".into(),
            });
        }
    }

    dispatch_events(events).await;
}

enum ControlEvent {
    Changed { owner: Option<SessionRole>, reason: String },
    Conflict(ControlStateView),
    ConflictHint { view: ControlStateView, reason: String },
}

async fn dispatch_events(events: Vec<ControlEvent>) {
    for ev in events {
        match ev {
            ControlEvent::Changed { owner, reason } => {
                notify_control_changed(owner, &reason).await;
            }
            ControlEvent::Conflict(view) => {
                notify_control_conflict(&view).await;
            }
            ControlEvent::ConflictHint { view, reason } => {
                let mut params = serde_json::to_value(&view).unwrap_or_else(|_| json!({}));
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("reason".into(), json!(reason));
                }
                notify_roles(
                    "controlConflict",
                    params,
                    &[SessionRole::User, SessionRole::Agent],
                )
                .await;
            }
        }
    }
}

fn clear_pending_locked(state: &mut ControlState) {
    if state.pending.take().is_some() {
        next_generation(); // invalidate in-flight timers
    }
}

fn make_pending(requester: SessionRole, policy: TimeoutPolicy, reason: &str) -> PendingRequest {
    let generation = next_generation();
    PendingRequest {
        request_id: Uuid::new_v4().to_string(),
        requester,
        policy,
        generation,
        deadline: Instant::now() + CONTROL_TIMEOUT,
        reason: reason.to_string(),
    }
}

fn spawn_timeout(generation: u64, request_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(CONTROL_TIMEOUT).await;
        if let Err(e) = on_timeout(generation, &request_id).await {
            debug!("control lease timeout ignored: {}", e);
        }
    });
}

async fn role_still_online(role: SessionRole) -> bool {
    get_global_app_state().get_session_for_role(role).await.is_some()
}

/// Fast HID gate: session must be role-slot holder and that role must own control.
pub async fn allows_hid(session_id: &str) -> bool {
    let app = get_global_app_state();
    let Some(session) = app.get_session_by_id(session_id).await else {
        return false;
    };
    if !app.is_role_slot_holder(session_id).await {
        return false;
    }
    if session.role == SessionRole::Agent
        && !crate::config::get_config_manager().get_mcp_enabled().await
    {
        return false;
    }
    let owner = LEASE.lock().await.owner;
    owner == Some(session.role)
}

pub fn note_hid_denied() {
    let n = HID_DENY_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
    if n % 50 == 0 {
        warn!("HID denied by control lease (count={})", n + 1);
    }
}

pub async fn get_control_state() -> ControlStateView {
    snapshot(&*LEASE.lock().await)
}

pub async fn request_control(session_id: &str) -> Result<Value> {
    let app = get_global_app_state();
    let session = app
        .get_session_by_id(session_id)
        .await
        .ok_or_else(|| anyhow!("session not found"))?;
    if !app.is_role_slot_holder(session_id).await {
        return Err(anyhow!("session is not the role slot holder"));
    }

    let role = session.role;
    if role == SessionRole::Agent
        && !crate::config::get_config_manager().get_mcp_enabled().await
    {
        return Err(anyhow!(
            "{}: enable MCP on the device before agent control",
            MCP_DISABLED
        ));
    }
    let user_online = role_still_online(SessionRole::User).await;
    let agent_online = role_still_online(SessionRole::Agent).await;
    let mut conflict: Option<ControlStateView> = None;
    let mut changed: Option<(Option<SessionRole>, String)> = None;
    let result_view;

    {
        let mut guard = LEASE.lock().await;

        if guard.owner == Some(role) {
            result_view = snapshot(&guard);
            return Ok(serde_json::to_value(result_view)?);
        }

        let owner_online = match guard.owner {
            Some(SessionRole::User) => user_online,
            Some(SessionRole::Agent) => agent_online,
            None => false,
        };

        if guard.owner.is_none() || !owner_online {
            clear_pending_locked(&mut guard);
            guard.owner = Some(role);
            result_view = snapshot(&guard);
            changed = Some((Some(role), "requestControlVacant".into()));
        } else if let Some(pending) = &guard.pending {
            if pending.requester == role {
                // Idempotent: return existing pending.
                result_view = snapshot(&guard);
            } else {
                return Err(anyhow!("{}: arbitration already in progress", CONTROL_BUSY));
            }
        } else {
            let policy = match (guard.owner, role) {
                // Human has control, Agent asks → 120s grant agent if silence.
                (Some(SessionRole::User), SessionRole::Agent) => TimeoutPolicy::GrantRequester,
                // Agent has control, Human should prefer takeOverControl; if they
                // requestControl, timeout leaves agent in control (manual take-over).
                (Some(SessionRole::Agent), SessionRole::User) => TimeoutPolicy::DenyRequester,
                (Some(SessionRole::Agent), SessionRole::Agent) => TimeoutPolicy::DenyRequester,
                _ => TimeoutPolicy::DenyRequester,
            };
            let reason = match (guard.owner, role) {
                (Some(SessionRole::User), SessionRole::Agent) => "humanHasControl",
                (Some(SessionRole::Agent), SessionRole::User) => "agentHasControl",
                _ => "controlConflict",
            };
            let pending = make_pending(role, policy, reason);
            spawn_timeout(pending.generation, pending.request_id.clone());
            guard.pending = Some(pending);
            result_view = snapshot(&guard);
            conflict = Some(result_view.clone());
        }
    }

    if let Some((owner, reason)) = changed {
        notify_control_changed(owner, &reason).await;
    }
    if let Some(view) = conflict {
        notify_control_conflict(&view).await;
    }

    Ok(serde_json::to_value(result_view)?)
}

pub async fn grant_control(session_id: &str, request_id: Option<&str>) -> Result<Value> {
    resolve_pending(session_id, request_id, true).await
}

pub async fn deny_control(session_id: &str, request_id: Option<&str>) -> Result<Value> {
    resolve_pending(session_id, request_id, false).await
}

/// Grant authority: the pending requester must not approve its own request.
/// Control transfers only via the other side's consent (user WebUI) or the
/// 120s silence timeout; denying is always allowed (a requester may safely
/// withdraw its own arbitration).
fn can_grant(requester: SessionRole, responder: SessionRole) -> bool {
    requester != responder
}

async fn resolve_pending(
    session_id: &str,
    request_id: Option<&str>,
    grant: bool,
) -> Result<Value> {
    let app = get_global_app_state();
    let session = app
        .get_session_by_id(session_id)
        .await
        .ok_or_else(|| anyhow!("session not found"))?;
    let responder = session.role;

    let mut changed: Option<(Option<SessionRole>, String)> = None;
    let result_view;

    {
        let mut guard = LEASE.lock().await;
        let Some(pending) = guard.pending.clone() else {
            return Err(anyhow!("{}: no pending control request", CONTROL_STALE));
        };
        if let Some(id) = request_id {
            if id != pending.request_id {
                return Err(anyhow!("{}: requestId mismatch", CONTROL_STALE));
            }
        }
        if grant && !can_grant(pending.requester, responder) {
            warn!(
                "control lease grant refused: {} cannot approve its own arbitration",
                responder
            );
            return Err(anyhow!(
                "{}: requester cannot grant its own arbitration; wait for the user decision or the {}s timeout",
                CONTROL_DENIED,
                CONTROL_TIMEOUT.as_secs()
            ));
        }

        clear_pending_locked(&mut guard);
        if grant {
            guard.owner = Some(pending.requester);
            changed = Some((Some(pending.requester), "granted".into()));
        } else {
            changed = Some((guard.owner, "denied".into()));
        }
        info!(
            "control lease arbitration resolved by {} ({}): requester={} request_id={}",
            responder,
            if grant { "granted" } else { "denied" },
            pending.requester,
            pending.request_id
        );
        result_view = snapshot(&guard);
    }

    if let Some((owner, reason)) = changed {
        notify_control_changed(owner, &reason).await;
    }
    Ok(serde_json::to_value(result_view)?)
}

/// Human force-takeover from Agent (keeps agent WebRTC View).
pub async fn take_over_control(session_id: &str) -> Result<Value> {
    let app = get_global_app_state();
    let session = app
        .get_session_by_id(session_id)
        .await
        .ok_or_else(|| anyhow!("session not found"))?;
    if session.role != SessionRole::User {
        return Err(anyhow!("only user can takeOverControl"));
    }
    if !app.is_role_slot_holder(session_id).await {
        return Err(anyhow!("session is not the user slot holder"));
    }

    let result_view;
    {
        let mut guard = LEASE.lock().await;
        clear_pending_locked(&mut guard);
        guard.owner = Some(SessionRole::User);
        result_view = snapshot(&guard);
    }
    notify_control_changed(Some(SessionRole::User), "takeOver").await;
    Ok(serde_json::to_value(result_view)?)
}

pub async fn release_control(session_id: &str) -> Result<Value> {
    let app = get_global_app_state();
    let session = app
        .get_session_by_id(session_id)
        .await
        .ok_or_else(|| anyhow!("session not found"))?;

    let user_online = role_still_online(SessionRole::User).await;
    let agent_online = role_still_online(SessionRole::Agent).await;
    let result_view;
    let new_owner;
    {
        let mut guard = LEASE.lock().await;
        if guard.owner != Some(session.role) {
            return Err(anyhow!("{}: caller is not control owner", CONTROL_DENIED));
        }
        clear_pending_locked(&mut guard);
        new_owner = match session.role {
            SessionRole::User if agent_online => Some(SessionRole::Agent),
            SessionRole::Agent if user_online => Some(SessionRole::User),
            _ => None,
        };
        guard.owner = new_owner;
        result_view = snapshot(&guard);
    }
    notify_control_changed(new_owner, "released").await;
    Ok(serde_json::to_value(result_view)?)
}

async fn on_timeout(generation: u64, request_id: &str) -> Result<()> {
    // Read MCP before taking the lease lock (no await while locked).
    let mcp_enabled = crate::config::get_config_manager().get_mcp_enabled().await;
    let mut changed: Option<(Option<SessionRole>, String)> = None;
    {
        let mut guard = LEASE.lock().await;
        let Some(pending) = guard.pending.clone() else {
            return Ok(());
        };
        if pending.generation != generation || pending.request_id != request_id {
            return Ok(());
        }
        let policy = pending.policy;
        let requester = pending.requester;
        clear_pending_locked(&mut guard);
        match policy {
            TimeoutPolicy::GrantRequester => {
                // MCP may have been turned off while arbitration was pending.
                let mcp_ok = requester != SessionRole::Agent || mcp_enabled;
                if mcp_ok {
                    guard.owner = Some(requester);
                    changed = Some((Some(requester), "timeoutGranted".into()));
                    info!("control lease timeout granted to {}", requester);
                } else {
                    changed = Some((guard.owner, "timeoutDeniedMcpDisabled".into()));
                    info!(
                        "control lease timeout denied requester {} (MCP disabled)",
                        requester
                    );
                }
            }
            TimeoutPolicy::DenyRequester => {
                changed = Some((guard.owner, "timeoutDenied".into()));
                info!("control lease timeout denied requester {}", requester);
            }
        }
    }
    if let Some((owner, reason)) = changed {
        notify_control_changed(owner, &reason).await;
    }
    Ok(())
}

/// When an rpc DataChannel becomes ready, push current conflict/state if relevant.
pub async fn on_rpc_ready(session: &Arc<Session>) {
    let view = get_control_state().await;
    let params = serde_json::to_value(&view).unwrap_or_else(|_| json!({}));
    if let Err(e) = PROCESSOR
        .send_event("controlChanged", Some(params), session.clone())
        .await
    {
        debug!("controlChanged on rpc ready failed: {}", e);
    }
    // Agent holds control and this is the user: also hint take-over.
    if session.role == SessionRole::User && view.owner == Some("agent") && view.pending.is_none() {
        let mut params = serde_json::to_value(&view).unwrap_or_else(|_| json!({}));
        if let Some(obj) = params.as_object_mut() {
            obj.insert("reason".into(), json!("agentHasControl"));
        }
        let _ = PROCESSOR
            .send_event("controlConflict", Some(params), session.clone())
            .await;
    }
}

/// Map handler errors that carry control codes into JSON-RPC application errors.
pub fn control_error_data(err: &anyhow::Error) -> Option<Value> {
    let msg = err.to_string();
    for code in [CONTROL_DENIED, CONTROL_BUSY, CONTROL_STALE, MCP_DISABLED] {
        if msg.contains(code) {
            return Some(json!({ "code": code, "message": msg }));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_policy_strings() {
        assert_eq!(policy_str(TimeoutPolicy::GrantRequester), "grantRequester");
        assert_eq!(policy_str(TimeoutPolicy::DenyRequester), "denyRequester");
    }

    #[tokio::test]
    async fn make_pending_unique_ids() {
        let a = make_pending(SessionRole::Agent, TimeoutPolicy::GrantRequester, "t");
        let b = make_pending(SessionRole::Agent, TimeoutPolicy::GrantRequester, "t");
        assert_ne!(a.request_id, b.request_id);
        assert_ne!(a.generation, b.generation);
    }

    #[tokio::test]
    async fn requester_cannot_grant_own_request() {
        // Either side may consent to the other's request...
        assert!(can_grant(SessionRole::Agent, SessionRole::User));
        assert!(can_grant(SessionRole::User, SessionRole::Agent));
        // ...but the requester must never approve its own arbitration.
        assert!(!can_grant(SessionRole::Agent, SessionRole::Agent));
        assert!(!can_grant(SessionRole::User, SessionRole::User));
    }
}
