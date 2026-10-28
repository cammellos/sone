//! What the MCP tools need from the app hosting them, so the same tools run
//! inside the GUI and inside the headless daemon.
//!
//! Playback tools and library-change notifications go out as events through
//! `sink()` (the GUI's frontend acts on them; the daemon maps them onto its
//! queue). Catalog, playlist and favorite tools call the TIDAL client directly.

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::cache::DiskCache;
use crate::events::EventSink;
use crate::mcp::{NowPlayingSnapshot, QueueTrackSnapshot};
use crate::tidal_api::TidalClient;

#[async_trait]
pub trait McpHost: Send + Sync + 'static {
    fn tidal_client(&self) -> &Mutex<TidalClient>;
    fn sink(&self) -> &EventSink;
    fn disk_cache(&self) -> &DiskCache;
    async fn now_playing(&self) -> Option<NowPlayingSnapshot>;
    async fn queue(&self) -> Vec<QueueTrackSnapshot>;
}

/// The GUI: state lives in `AppState`, now-playing is mirrored from the
/// frontend into `mcp_state`.
#[cfg(feature = "gui")]
pub struct GuiHost {
    app: tauri::AppHandle,
    sink: EventSink,
}

#[cfg(feature = "gui")]
impl GuiHost {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            sink: app.clone().into(),
            app,
        }
    }

    fn state(&self) -> &crate::AppState {
        use tauri::Manager;
        self.app.state::<crate::AppState>().inner()
    }
}

#[cfg(feature = "gui")]
#[async_trait]
impl McpHost for GuiHost {
    fn tidal_client(&self) -> &Mutex<TidalClient> {
        &self.state().tidal_client
    }

    fn sink(&self) -> &EventSink {
        &self.sink
    }

    fn disk_cache(&self) -> &DiskCache {
        &self.state().disk_cache
    }

    async fn now_playing(&self) -> Option<NowPlayingSnapshot> {
        self.state().mcp_state.read().await.now_playing.clone()
    }

    async fn queue(&self) -> Vec<QueueTrackSnapshot> {
        self.state().mcp_state.read().await.queue.clone()
    }
}
