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

    /// Add a new session to the state
    pub async fn add_session(&self, session: Arc<Session>) {
        let session_id = session.id.clone();
        let (old, count) = {
            let mut sessions = self.sessions.write().await;
            let old = sessions.insert(session_id.clone(), session);
            let count = sessions.len();
            info!("Added session, and session length is now {}", count);
            (old, count)
        };

        // Update current session if this is the first one
        // let mut current = self.current_session.write().await;
        // if current.is_none() {
        //     *current = Some(session_id);
        // }
        // drop(current);

        if old.is_none() && count == 1 {
            tokio::spawn(crate::webrtc::on_first_session_connected());
        }
    }

    /// Remove a session from the state
    pub async fn remove_session(&self, session_id: &str) -> Option<Arc<Session>> {
        let (removed, count) = {
            let mut sessions = self.sessions.write().await;
            let removed = sessions.remove(session_id);
            let count = sessions.len();
            info!("Removed session, and session length is now {}", count);
            (removed, count)
        };

        // Clear current session if it was the removed one
        let mut current = self.current_session.write().await;
        if let Some(ref current_id) = *current
            && current_id == session_id
        {
            *current = None;
        }
        drop(current);
        
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
