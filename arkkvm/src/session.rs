use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::RwLock;
use tracing::info;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

pub struct Session {
    pub id: String,
    pub peer_connection: Option<Arc<RTCPeerConnection>>,
    pub video_track: Option<Arc<TrackLocalStaticSample>>,
    pub audio_track: Option<Arc<TrackLocalStaticSample>>,
    pub control_channel: Option<Arc<RTCDataChannel>>,
    pub rpc_channel: RwLock<Option<Arc<RTCDataChannel>>>,
    pub hid_channel: Option<Arc<RTCDataChannel>>,
    pub disk_channel: Option<Arc<RTCDataChannel>>,
    pub should_unmount_virtual_media: bool,
    pub is_cloud: bool,
    channel_cache: Arc<RwLock<HashMap<String, Arc<RTCDataChannel>>>>,
    pub username_fragment: RwLock<Option<String>>, // Cache usernameFragment to avoid blocking in send_ice_candidate
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
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

// impl Drop for Session {
//     fn drop(&mut self) {
//         warn!("Session {} dropped", self.id);
//     }
// }

// impl Clone for Session {
//     fn clone(&self) -> Self {
//         Self {
//             id: self.id.clone(),
//             peer_connection: self.peer_connection.clone(),
//             video_track: self.video_track.clone(),
//             audio_track: self.audio_track.clone(),
//             control_channel: self.control_channel.clone(),
//             rpc_channel: self.rpc_channel.clone(),
//             hid_channel: self.hid_channel.clone(),
//             disk_channel: self.disk_channel.clone(),
//             should_unmount_virtual_media: self.should_unmount_virtual_media,
//             is_cloud: self.is_cloud,
//             channel_cache: self.channel_cache.clone(),
//         }
//     }
// }

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "id={}", self.id,)
    }
}

impl Session {
    pub fn new(id: String) -> Self {
        Self {
            id,
            peer_connection: None,
            video_track: None,
            audio_track: None,
            control_channel: None,
            rpc_channel: RwLock::new(None),
            hid_channel: None,
            disk_channel: None,
            should_unmount_virtual_media: false,
            is_cloud: false,
            channel_cache: Arc::new(RwLock::new(HashMap::new())),
            username_fragment: RwLock::new(None),
        }
    }
    
    pub fn new_with_cloud(id: String, is_cloud: bool) -> Self {
        Self {
            id,
            peer_connection: None,
            video_track: None,
            audio_track: None,
            control_channel: None,
            rpc_channel: RwLock::new(None),
            hid_channel: None,
            disk_channel: None,
            should_unmount_virtual_media: false,
            is_cloud,
            channel_cache: Arc::new(RwLock::new(HashMap::new())),
            username_fragment: RwLock::new(None),
        }
    }

    /// Exchange WebRTC offer and return answer
    pub async fn exchange_offer(&self, offer_str: &str) -> anyhow::Result<String> {
        use base64::Engine as _;
        use base64::engine::general_purpose;

        // Decode base64 encoded offer
        let offer_bytes = general_purpose::STANDARD.decode(offer_str)?;
        let offer: webrtc::peer_connection::sdp::session_description::RTCSessionDescription =
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

        let local_desc = peer_conn.local_description().await
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
        let candidate: webrtc::ice_transport::ice_candidate::RTCIceCandidateInit =
            serde_json::from_str(candidate_str)?;

        if let Some(peer_conn) = &self.peer_connection {
            peer_conn.add_ice_candidate(candidate).await?;
            info!("Added ICE candidate to session: {}", self.id);
            Ok(())
        } else {
            anyhow::bail!("No peer connection available")
        }
    }

    pub async fn create_data_channel(&self, label: &str, options: Option<RTCDataChannelInit>) -> anyhow::Result<Arc<RTCDataChannel>> { 
        let Some(peer_connection) = self.peer_connection.as_ref() else {
            return Err(anyhow!("No peer connection available"));
        };
        
        let channel = match peer_connection.create_data_channel(label, options).await {
            Ok(channel) => channel,
            Err(e) => return Err(anyhow!("Failed to create data channel({}): {:?}", label, e)),
        };
        Ok(channel)
    }

    pub async fn cache_channel(&self, channel: Arc<RTCDataChannel>) {
        self.channel_cache.write().await.insert(channel.label().to_string(), channel);
    }

    pub async fn remove_channel(&self, label: &String) {
        self.channel_cache.write().await.remove(label);
    }
}
