use std::collections::HashMap;
use std::sync::Arc;

use socketioxide::extract::SocketRef;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::PeerConnection;

use crate::session::{Session, SessionRole};

/// Result of claiming a per-role session slot.
#[derive(Debug)]
pub enum ClaimRoleResult {
    /// Slot claimed (or already held by this session). `previous` is the prior holder on user takeover.
    Accepted { previous: Option<Arc<Session>> },
    /// Agent slot already held by another session.
    RejectedOccupied,
    /// Agent claim refused because device MCP switch is off (ARKKVM-619).
    RejectedMcpDisabled,
    /// Session is not registered in `sessions` (or was replaced).
    NotRegistered,
}

#[derive(Debug)]
pub struct AppState {
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    /// Official session id per role (user takeover / agent reject).
    pub role_sessions: RwLock<HashMap<SessionRole, String>>,
    pub sockets: RwLock<HashMap<String, SocketRef>>,
    pub websocket_ice_queue: RwLock<HashMap<String, Vec<String>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            role_sessions: RwLock::new(HashMap::new()),
            sockets: RwLock::new(HashMap::new()),
            websocket_ice_queue: RwLock::new(HashMap::new()),
        }
    }

    /// Add a session, replacing any existing session with the same ID.
    /// Returns the previous session when replaced so the caller can close its
    /// peer connection. Same-ID replace is required for WebSocket renegotiation
    /// that reuses `connection_id`; callers must close the returned session and
    /// must not treat replace as "last session gone" cleanup.
    pub async fn add_session(&self, session: Arc<Session>) -> Option<Arc<Session>> {
        let session_id = session.id.clone();
        let (previous, count) = {
            let mut sessions = self.sessions.write().await;
            let previous = sessions.insert(session_id.clone(), session);
            let count = sessions.len();
            if previous.is_some() {
                warn!(
                    "Replacing active session {}; caller must close the previous peer",
                    session_id
                );
            }
            info!("Added session, and session length is now {}", count);
            (previous, count)
        };

        // Only the first registered session triggers first-connect side effects.
        // Same-ID replace keeps count unchanged and must not clear current.
        if previous.is_none() && count == 1 {
            tokio::spawn(crate::webrtc::on_first_session_connected());
        }

        previous
    }

    /// Remove a session from the state
    pub async fn remove_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.remove_session_inner(session_id, None).await
    }

    /// Remove a session only when it owns the peer connection that emitted the
    /// callback. This prevents a delayed callback from an old connection from
    /// removing a newer session that happens to reuse the same ID.
    pub async fn remove_session_for_peer(
        &self,
        session_id: &str,
        peer_connection: &Arc<dyn PeerConnection>,
    ) -> Option<Arc<Session>> {
        self.remove_session_inner(session_id, Some(peer_connection)).await
    }

    async fn remove_session_inner(
        &self,
        session_id: &str,
        expected_peer: Option<&Arc<dyn PeerConnection>>,
    ) -> Option<Arc<Session>> {
        let (removed, count) = {
            let mut sessions = self.sessions.write().await;
            let matches_expected_peer = sessions.get(session_id).is_some_and(|session| {
                expected_peer.is_none_or(|expected| {
                    session
                        .peer_connection
                        .as_ref()
                        .is_some_and(|actual| Arc::ptr_eq(actual, expected))
                })
            });
            let removed = matches_expected_peer.then(|| sessions.remove(session_id)).flatten();
            let count = sessions.len();
            if removed.is_some() {
                info!("Removed session {}, and session length is now {}", session_id, count);
            } else {
                debug!(
                    "Skip remove_session for already-removed session {}, session length is {}",
                    session_id, count
                );
            }
            (removed, count)
        };

        // Clear any role slots that pointed at this session id.
        if let Some(ref removed_session) = removed {
            let mut roles = self.role_sessions.write().await;
            roles.retain(|_, id| id != session_id);
            crate::ip_forward::clear_session(session_id).await;
            // Drop role write lock before control-lease work.
            drop(roles);
            crate::control_lease::on_session_removed(removed_session).await;
        }

        if removed.is_some() && count == 0 {
            tokio::spawn(crate::webrtc::on_last_session_disconnected());
        }

        removed
    }

    /// Claim the role slot for `session`.
    ///
    /// - User: empty → occupy; other id → takeover (return previous); same id → ok
    /// - Agent: empty → occupy; other id → RejectedOccupied; same id → ok
    pub async fn claim_role_slot(&self, session: &Arc<Session>) -> ClaimRoleResult {
        let sessions = self.sessions.read().await;
        if !sessions.get(&session.id).is_some_and(|registered| Arc::ptr_eq(registered, session)) {
            return ClaimRoleResult::NotRegistered;
        }

        let role = session.role;
        let mut roles = self.role_sessions.write().await;
        match roles.get(&role) {
            None => {
                roles.insert(role, session.id.clone());
                ClaimRoleResult::Accepted { previous: None }
            }
            Some(existing_id) if existing_id == &session.id => {
                ClaimRoleResult::Accepted { previous: None }
            }
            Some(existing_id) => {
                if role == SessionRole::Agent {
                    ClaimRoleResult::RejectedOccupied
                } else {
                    let previous = sessions.get(existing_id).cloned();
                    roles.insert(role, session.id.clone());
                    ClaimRoleResult::Accepted { previous }
                }
            }
        }
    }

    /// Fill an empty role slot only (ICE-before-claim race). Returns true if this call wrote the slot.
    pub async fn set_role_slot_if_none(&self, role: SessionRole, session_id: String) -> bool {
        let mut roles = self.role_sessions.write().await;
        if roles.contains_key(&role) {
            return false;
        }
        roles.insert(role, session_id);
        true
    }

    /// Clear role slot only if it still holds `session_id`.
    pub async fn clear_role_slot_if(&self, role: SessionRole, session_id: &str) -> bool {
        let mut roles = self.role_sessions.write().await;
        if roles.get(&role).map(|id| id.as_str()) != Some(session_id) {
            return false;
        }
        roles.remove(&role);
        true
    }

    pub async fn get_session_for_role(&self, role: SessionRole) -> Option<Arc<Session>> {
        let session_id = {
            let roles = self.role_sessions.read().await;
            roles.get(&role).cloned()
        };
        if let Some(session_id) = session_id {
            self.get_session_by_id(session_id.as_str()).await
        } else {
            None
        }
    }

    pub async fn get_role_slot_id(&self, role: SessionRole) -> Option<String> {
        self.role_sessions.read().await.get(&role).cloned()
    }

    /// True when the sessions map is non-empty or any role slot is set.
    pub async fn has_any_session(&self) -> bool {
        if !self.sessions.read().await.is_empty() {
            return true;
        }
        !self.role_sessions.read().await.is_empty()
    }

    pub async fn is_role_slot_holder(&self, session_id: &str) -> bool {
        self.role_sessions.read().await.values().any(|id| id == session_id)
    }

    /// Get a session by ID
    pub async fn get_session_by_id(&self, session_id: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).cloned()
    }

    pub async fn session_owns_peer(
        &self,
        session_id: &str,
        peer_connection: &Arc<dyn PeerConnection>,
    ) -> bool {
        self.sessions.read().await.get(session_id).is_some_and(|session| {
            session
                .peer_connection
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, peer_connection))
        })
    }

    pub async fn update_session_rpc_channel(
        &self,
        session_id: &str,
        rpc_channel: Arc<dyn DataChannel>,
    ) {
        let sessions = self.sessions.read().await;
        let Some(session) = sessions.get(session_id) else {
            warn!("Session not found by id: {session_id}");
            return;
        };
        *session.rpc_channel.write().await = Some(rpc_channel);
        let session = session.clone();
        drop(sessions);
        // Re-push control state once the rpc channel exists (claim may have raced ahead).
        crate::control_lease::on_rpc_ready(&session).await;
    }

    /// Get count of active sessions
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Queue ICE candidate for WebSocket connection
    pub async fn queue_ice_candidate(&self, session_id: &str, candidate: String) {
        const MAX_ICE_CANDIDATES: usize = 20;

        let mut queue = self.websocket_ice_queue.write().await;
        let ice_queue = queue.entry(session_id.to_string()).or_default();

        if ice_queue.len() >= MAX_ICE_CANDIDATES {
            ice_queue.remove(0);
            warn!("ICE queue full for session {}, removed oldest candidate", session_id);
        }

        ice_queue.push(candidate);
    }

    /// Get and clear ICE candidates for WebSocket connection
    pub async fn get_ice_candidates(&self, session_id: &str) -> Vec<String> {
        let candidates =
            self.websocket_ice_queue.write().await.remove(session_id).unwrap_or_default();
        if !candidates.is_empty() {
            debug!("Retrieved {} ICE candidates for session {}", candidates.len(), session_id);
        }
        candidates
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_role(id: &str, role: SessionRole) -> Arc<Session> {
        Arc::new(Session::new_with_role(id.to_string(), role, false))
    }

    #[tokio::test]
    async fn user_takeover_returns_previous() {
        let state = AppState::new();
        let first = session_with_role("user-1", SessionRole::User);
        let second = session_with_role("user-2", SessionRole::User);
        state.add_session(first.clone()).await;
        state.add_session(second.clone()).await;

        assert!(matches!(
            state.claim_role_slot(&first).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
        match state.claim_role_slot(&second).await {
            ClaimRoleResult::Accepted { previous: Some(prev) } => {
                assert_eq!(prev.id, "user-1");
            }
            other => panic!("expected takeover, got {:?}", other),
        }
        assert_eq!(state.get_role_slot_id(SessionRole::User).await.as_deref(), Some("user-2"));
    }

    #[tokio::test]
    async fn agent_reject_when_occupied() {
        let state = AppState::new();
        let first = session_with_role("agent-1", SessionRole::Agent);
        let second = session_with_role("agent-2", SessionRole::Agent);
        state.add_session(first.clone()).await;
        state.add_session(second.clone()).await;

        assert!(matches!(
            state.claim_role_slot(&first).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
        assert!(matches!(state.claim_role_slot(&second).await, ClaimRoleResult::RejectedOccupied));
        assert_eq!(state.get_role_slot_id(SessionRole::Agent).await.as_deref(), Some("agent-1"));
    }

    #[tokio::test]
    async fn renegotiation_same_id_allowed() {
        let state = AppState::new();
        let session = session_with_role("same", SessionRole::User);
        state.add_session(session.clone()).await;
        assert!(matches!(
            state.claim_role_slot(&session).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
        assert!(matches!(
            state.claim_role_slot(&session).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
    }

    #[tokio::test]
    async fn dual_role_coexist() {
        let state = AppState::new();
        let user = session_with_role("u", SessionRole::User);
        let agent = session_with_role("a", SessionRole::Agent);
        state.add_session(user.clone()).await;
        state.add_session(agent.clone()).await;

        assert!(matches!(
            state.claim_role_slot(&user).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
        assert!(matches!(
            state.claim_role_slot(&agent).await,
            ClaimRoleResult::Accepted { previous: None }
        ));
        assert!(state.get_session_for_role(SessionRole::User).await.is_some());
        assert!(state.get_session_for_role(SessionRole::Agent).await.is_some());
        assert!(state.is_role_slot_holder("u").await);
        assert!(state.is_role_slot_holder("a").await);
        assert!(state.has_any_session().await);
    }

    #[tokio::test]
    async fn remove_session_clears_matching_role_slot_only() {
        let state = AppState::new();
        let user = session_with_role("u", SessionRole::User);
        let agent = session_with_role("a", SessionRole::Agent);
        state.add_session(user.clone()).await;
        state.add_session(agent.clone()).await;
        let _ = state.claim_role_slot(&user).await;
        let _ = state.claim_role_slot(&agent).await;

        state.remove_session("u").await;
        assert!(state.get_role_slot_id(SessionRole::User).await.is_none());
        assert_eq!(state.get_role_slot_id(SessionRole::Agent).await.as_deref(), Some("a"));
    }
}
