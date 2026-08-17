use std::collections::HashMap;
use std::sync::Arc;

use socketioxide::extract::SocketRef;
use tokio::sync::RwLock;
use tracing::{debug, warn, info};
use webrtc::data_channel::RTCDataChannel;

use crate::session::Session;

#[derive(Debug)]
pub struct AppState {
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub current_session: RwLock<Option<String>>,
    pub sockets: RwLock<HashMap<String, SocketRef>>,
    pub websocket_ice_queue: RwLock<HashMap<String, Vec<String>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            current_session: RwLock::new(None),
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
        peer_connection: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    ) -> Option<Arc<Session>> {
        self.remove_session_inner(session_id, Some(peer_connection)).await
    }

    async fn remove_session_inner(
        &self,
        session_id: &str,
        expected_peer: Option<&Arc<webrtc::peer_connection::RTCPeerConnection>>,
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

        // A stale callback for a different peer with the same ID must not
        // clear the replacement's current-session marker.
        if removed.is_some() {
            let mut current = self.current_session.write().await;
            if let Some(ref current_id) = *current
                && current_id == session_id
            {
                *current = None;
            }
        }

        if removed.is_some() && count == 0 {
            tokio::spawn(crate::webrtc::on_last_session_disconnected());
        }

        removed
    }

    /// Get the current active session
    pub async fn get_current_session_id(&self) -> Option<String> {
        self.current_session.read().await.clone()
    }

    /// Set the current active session
    pub async fn set_current_session_id(&self, session_id: Option<String>) {
        *self.current_session.write().await = session_id;
    }

    /// Atomically verify and publish an exact session generation while
    /// capturing the exact predecessor generation. Holding the sessions read
    /// lock prevents removal or ID reuse between validation and capture.
    pub async fn promote_session_if_registered(
        &self,
        session: &Arc<Session>,
    ) -> Result<Option<Arc<Session>>, ()> {
        let sessions = self.sessions.read().await;
        if !sessions
            .get(&session.id)
            .is_some_and(|registered| Arc::ptr_eq(registered, session))
        {
            return Err(());
        }

        let mut current = self.current_session.write().await;
        let previous_id = current.replace(session.id.clone());
        Ok(previous_id.and_then(|id| sessions.get(&id).cloned()))
    }

    pub async fn set_current_session_if_none(&self, session_id: String) -> bool {
        let mut current = self.current_session.write().await;
        if current.is_some() {
            return false;
        }
        *current = Some(session_id);
        true
    }

    pub async fn clear_current_session_if(&self, session_id: &str) -> bool {
        let mut current = self.current_session.write().await;
        if current.as_deref() != Some(session_id) {
            return false;
        }
        *current = None;
        true
    }

    /// Get the current active session
    pub async fn get_current_session(&self) -> Option<Arc<Session>> {
        let session_id = {
            let current = self.current_session.read().await;
            current.clone()
        };

        if let Some(session_id) = session_id {
            self.get_session_by_id(session_id.as_str()).await
        } else {
            None
        }
    }

    /// Get a session by ID
    pub async fn get_session_by_id(&self, session_id: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).cloned()
    }

    pub async fn session_owns_peer(
        &self,
        session_id: &str,
        peer_connection: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    ) -> bool {
        self.sessions.read().await.get(session_id).is_some_and(|session| {
            session
                .peer_connection
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, peer_connection))
        })
    }

    pub async fn update_session_rpc_channel(&self, session_id: &str, rpc_channel: Arc<RTCDataChannel>) {
        let sessions = self.sessions.read().await;
        let Some(session) = sessions.get(session_id) else {
            warn!("Session not found by id: {session_id}");
            return;
        };
        *session.rpc_channel.write().await = Some(rpc_channel);
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
