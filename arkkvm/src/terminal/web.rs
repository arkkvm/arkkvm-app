use std::sync::Arc;

use anyhow::Result;
use tokio::process::Command;
use tracing::info;
use webrtc::data_channel::DataChannel;

use super::core::{SpawnSpec, TerminalHandle, attach};

/// KVM local interactive shell over DataChannel label `terminal`.
pub struct WebTerminal;

impl WebTerminal {
    /// Attach handlers; `/bin/sh -i` starts when the DataChannel opens.
    /// Returns a handle that the owning [`crate::session::Session`] must retain
    /// and `stop_and_wait` on teardown.
    pub fn attach(channel: Arc<dyn DataChannel>) -> TerminalHandle {
        let channel_id = channel.id();
        info!("WebTerminal attaching (channel ID: {:?})", channel_id);

        let mut command = Command::new("/bin/sh");
        command.arg("-i");

        attach(channel, SpawnSpec { command, cleanup_paths: Vec::new() })
    }

    /// Async entry used by the DataChannel router.
    pub async fn attach_async(channel: Arc<dyn DataChannel>) -> Result<TerminalHandle> {
        Ok(Self::attach(channel))
    }
}
