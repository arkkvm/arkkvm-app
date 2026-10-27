use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::anyhow;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;
use webrtc::data_channel::{DataChannel, RTCDataChannelInit};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{PeerConnection, RTCIceConnectionState};

use crate::terminal::TerminalHandle;

/// Session-owned WebTerminal slot. `Closed` rejects further installs after teardown.
enum TerminalSlot {
    /// Session still accepts a terminal; `None` means none attached yet.
    Open(Option<TerminalHandle>),
    /// Teardown started or finished; `install_terminal` must return the handle.
    Closed,
}

/// WebRTC / auth client role: interactive user vs agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRole {
    #[default]
    User,
    Agent,
}

impl SessionRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionRole::User => "user",
            SessionRole::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(SessionRole::User),
            "agent" => Some(SessionRole::Agent),
            _ => None,
        }
    }
}

impl fmt::Display for SessionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("invalid session role: {s}"))
    }
}

pub struct Session {
    pub id: String,
    pub role: SessionRole,
    pub peer_connection: Option<Arc<dyn PeerConnection>>,
    pub video_track: Option<Arc<TrackLocalStaticSample>>,
    pub audio_track: Option<Arc<TrackLocalStaticSample>>,
    pub video_ssrc: Option<u32>,
    pub audio_ssrc: Option<u32>,
    pub ice_connection_state: Arc<parking_lot::RwLock<RTCIceConnectionState>>,
    pub control_channel: Option<Arc<dyn DataChannel>>,
    pub rpc_channel: RwLock<Option<Arc<dyn DataChannel>>>,
    pub hid_channel: Option<Arc<dyn DataChannel>>,
    pub disk_channel: Option<Arc<dyn DataChannel>>,
    pub should_unmount_virtual_media: bool,
    pub is_cloud: bool,
    /// Cloud-minted client signaling id (echoed on answer/ICE/error). None for LAN / legacy.
    pub cloud_client_id: Option<String>,
    channel_cache: Arc<RwLock<HashMap<String, Arc<dyn DataChannel>>>>,
    pub username_fragment: RwLock<Option<String>>, // Cache usernameFragment to avoid blocking in send_ice_candidate
    pub created_at: std::time::Instant,
    /// Local WebTerminal (`label=terminal`); stopped on session teardown.
    terminal: Mutex<TerminalSlot>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("has_peer_connection", &self.peer_connection.is_some())
            .field("has_video_track", &self.video_track.is_some())
            .field("has_audio_track", &self.audio_track.is_some())
            .field("has_control_channel", &self.control_channel.is_some())
            .field("has_rpc_channel", &self.rpc_channel.blocking_read().is_some())
            .field("has_hid_channel", &self.hid_channel.is_some())
            .field("has_disk_channel", &self.disk_channel.is_some())
            .field("should_unmount_virtual_media", &self.should_unmount_virtual_media)
            .field("is_cloud", &self.is_cloud)
            .finish()
    }
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "id={} role={}", self.id, self.role)
    }
}

impl Session {
    pub fn new(id: String) -> Self {
        Self::new_with_role(id, SessionRole::User, false)
    }

    pub fn new_with_cloud(id: String, is_cloud: bool) -> Self {
        Self::new_with_role(id, SessionRole::User, is_cloud)
    }

    pub fn new_with_role(id: String, role: SessionRole, is_cloud: bool) -> Self {
        Self {
            id,
            role,
            peer_connection: None,
            video_track: None,
            audio_track: None,
            video_ssrc: None,
            audio_ssrc: None,
            ice_connection_state: Arc::new(parking_lot::RwLock::new(
                RTCIceConnectionState::Unspecified,
            )),
            control_channel: None,
            rpc_channel: RwLock::new(None),
            hid_channel: None,
            disk_channel: None,
            should_unmount_virtual_media: false,
            is_cloud,
            cloud_client_id: None,
            channel_cache: Arc::new(RwLock::new(HashMap::new())),
            username_fragment: RwLock::new(None),
            created_at: std::time::Instant::now(),
            terminal: Mutex::new(TerminalSlot::Open(None)),
        }
    }

    /// Attach or replace the WebTerminal handle.
    ///
    /// If teardown already moved the slot to [`TerminalSlot::Closed`], returns
    /// `Err(handle)` so the caller must `stop_and_wait` (do not drop-only).
    pub fn install_terminal(&self, handle: TerminalHandle) -> Result<(), TerminalHandle> {
        let mut slot = self.terminal.lock();
        match &mut *slot {
            TerminalSlot::Closed => Err(handle),
            TerminalSlot::Open(inner) => {
                if let Some(prev) = inner.replace(handle) {
                    prev.stop();
                }
                Ok(())
            }
        }
    }

    /// Mark the slot closed and wait for the WebTerminal actor to reclaim `/bin/sh` + PTY.
    pub async fn stop_terminal(&self) {
        let handle = {
            let mut slot = self.terminal.lock();
            match std::mem::replace(&mut *slot, TerminalSlot::Closed) {
                TerminalSlot::Open(h) => h,
                TerminalSlot::Closed => None,
            }
        };
        if let Some(handle) = handle {
            handle.stop_and_wait().await;
        }
    }

    /// Exchange WebRTC offer and return answer
    pub async fn exchange_offer(&self, offer_str: &str) -> anyhow::Result<String> {
        use base64::Engine as _;
        use base64::engine::general_purpose;

        // Decode base64 encoded offer
        let offer_bytes = general_purpose::STANDARD.decode(offer_str)?;
        let offer: webrtc::peer_connection::RTCSessionDescription =
            serde_json::from_slice(&offer_bytes)?;

        let Some(peer_conn) = &self.peer_connection else {
            anyhow::bail!("No peer connection available for session: {}", self.id);
        };

        // Set remote description
        peer_conn.set_remote_description(offer).await?;

        // Create answer
        let answer = peer_conn.create_answer(None).await?;

        // Set local description
        peer_conn.set_local_description(answer).await?;

        // Get local description and encode to base64

        let local_desc = peer_conn
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to get local description after setting it"))?;

        // Extract and cache usernameFragment from local description
        let sdp = &local_desc.sdp;
        if let Some(ufrag_line) = sdp.lines().find(|line| line.starts_with("a=ice-ufrag:")) {
            let ufrag = ufrag_line.strip_prefix("a=ice-ufrag:").unwrap_or("").trim();
            *self.username_fragment.write().await = Some(ufrag.to_string());
        }

        let local_desc_bytes = serde_json::to_vec(&local_desc)?;
        let answer_str = general_purpose::STANDARD.encode(local_desc_bytes);
        Ok(answer_str)
    }

    /// Add ICE candidate to the peer connection
    pub async fn add_ice_candidate(&self, candidate_str: &str) -> anyhow::Result<()> {
        // Parse JSON formatted ICE candidate
        let candidate: webrtc::peer_connection::RTCIceCandidateInit =
            serde_json::from_str(candidate_str)?;

        if let Some(peer_conn) = &self.peer_connection {
            peer_conn.add_ice_candidate(candidate).await?;
            info!("Added ICE candidate to session: {}", self.id);
            Ok(())
        } else {
            anyhow::bail!("No peer connection available")
        }
    }

    pub async fn create_data_channel(
        &self,
        label: &str,
        options: Option<RTCDataChannelInit>,
    ) -> anyhow::Result<Arc<dyn DataChannel>> {
        let Some(peer_connection) = self.peer_connection.as_ref() else {
            return Err(anyhow!("No peer connection available"));
        };

        let channel = match peer_connection.create_data_channel(label, options).await {
            Ok(channel) => channel,
            Err(e) => return Err(anyhow!("Failed to create data channel({}): {:?}", label, e)),
        };
        Ok(channel)
    }

    pub async fn cache_channel(&self, channel: Arc<dyn DataChannel>) {
        let label = channel.label().await.unwrap_or_default();
        self.channel_cache.write().await.insert(label, channel);
    }

    pub async fn remove_channel(&self, label: &String) {
        self.channel_cache.write().await.remove(label);
    }
}
