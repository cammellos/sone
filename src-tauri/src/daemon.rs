//! Headless daemon: owns the engine and the play queue, and serves clients
//! (CLI, TUI) over a Unix socket.
//!
//! Protocol: newline-delimited JSON. A request is
//! `{"id": 1, "cmd": "next", "args": {...}}`; the reply is
//! `{"id": 1, "ok": true, "result": ...}` or `{"id": 1, "ok": false, "error": "..."}`.
//! After `subscribe`, the connection also receives `{"event": "...", "data": ...}`
//! lines: `state` whenever the track, play state, volume, repeat or shuffle
//! changes, `position` once a second while playing, and `seeked` after a seek.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::cache::DiskCache;
use crate::engine::Engine;
use crate::events::EventSink;
use crate::mcp::host::McpHost;
use crate::mcp::{NowPlayingSnapshot, QueueTrackSnapshot};
use crate::tidal_api::TidalClient;
use crate::queue::{Previous, Queue, QueueTrack, Repeat};
use crate::tidal_api::{TidalAlbumDetail, TidalTrack};
use crate::SoneError;

mod mpris;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

struct Player {
    queue: Queue,
    state: PlayState,
    volume: f32,
}

pub struct Daemon {
    engine: Engine,
    player: Mutex<Player>,
    events: broadcast::Sender<String>,
    logged_in: AtomicBool,
    /// The PKCE login `login_start` began: (code verifier, client unique key).
    pending_login: Mutex<Option<(String, String)>>,
    /// Output devices for exclusive mode, probed on the first `settings`.
    devices: std::sync::OnceLock<Vec<crate::audio::AudioDevice>>,
}

#[derive(Deserialize)]
struct Request {
    id: Value,
    cmd: String,
    #[serde(default)]
    args: Value,
}

/// `$XDG_RUNTIME_DIR/sone.sock`, falling back to the temp dir.
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sone.sock")
}

/// Where the MCP server listens: port and token, as in the URL clients use.
pub struct McpConfig {
    pub port: u16,
    pub token: String,
}

/// Parse an MCP URL of the form `http://127.0.0.1:<port>/<token>/mcp`.
pub fn parse_mcp_url(url: &str) -> Option<McpConfig> {
    let rest = url.trim().strip_prefix("http://")?;
    let mut parts = rest.split('/');
    let port = parts.next()?.rsplit_once(':')?.1.parse().ok()?;
    let token = parts.next()?.to_string();
    match (parts.next(), parts.next()) {
        (Some("mcp"), None) if !token.is_empty() => Some(McpConfig { port, token }),
        _ => None,
    }
}

/// Run the daemon. With `mcp`, the MCP server uses that port and token
/// whatever the settings say; without it, the settings decide.
pub async fn run(
    config_dir: &Path,
    socket_path: &Path,
    mcp: Option<McpConfig>,
) -> Result<(), SoneError> {
    let (engine_tx, mut engine_rx) = mpsc::unbounded_channel::<(String, Value)>();
    let sink = EventSink::new(move |name, payload| {
        let _ = engine_tx.send((name.to_string(), payload));
    });
    let engine = Engine::new(sink, config_dir).await?;
    let volume = engine.settings().volume;
    let logged_in = AtomicBool::new(engine.logged_in().await);
    let (events, _) = broadcast::channel(64);
    let daemon = Arc::new(Daemon {
        engine,
        player: Mutex::new(Player {
            queue: Queue::default(),
            state: PlayState::Stopped,
            volume,
        }),
        events,
        logged_in,
        pending_login: Mutex::new(None),
        devices: std::sync::OnceLock::new(),
    });

    // Audio engine events drive the queue.
    {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            while let Some((name, payload)) = engine_rx.recv().await {
                daemon.on_engine_event(&name, &payload).await;
            }
        });
    }

    // Position ticks for subscribed clients.
    {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                if daemon.player.lock().await.state == PlayState::Playing {
                    let pos = daemon.position();
                    daemon.publish("position", json!({ "positionSeconds": pos }));
                }
            }
        });
    }

    let mcp = start_mcp(&daemon, mcp).await;
    mpris::spawn(Arc::clone(&daemon)).await;

    if socket_path.exists() {
        if UnixStream::connect(socket_path).await.is_ok() {
            return Err(SoneError::Io(format!(
                "another daemon is listening on {}",
                socket_path.display()
            )));
        }
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    log::info!("[daemon] listening on {}", socket_path.display());

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(e) = daemon.serve(stream).await {
                        log::debug!("[daemon] client closed: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = daemon.engine.audio_player.stop();
    let _ = std::fs::remove_file(socket_path);
    if let Some(handle) = mcp {
        handle.shutdown().await;
    }
    Ok(())
}

/// Serve the same MCP tools as the GUI, on the given port and token, or else
/// on those stored in SONE's settings when MCP is enabled there.
async fn start_mcp(
    daemon: &Arc<Daemon>,
    config: Option<McpConfig>,
) -> Option<crate::mcp::McpHandle> {
    let config = match config {
        Some(config) => config,
        None => {
            let settings = daemon.engine.settings();
            if !settings.mcp_enabled || settings.mcp_token.is_empty() {
                log::info!("[daemon] MCP disabled in settings");
                return None;
            }
            McpConfig {
                port: settings.mcp_port,
                token: settings.mcp_token.clone(),
            }
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
    let host = Arc::new(DaemonHost {
        daemon: Arc::clone(daemon),
        sink: EventSink::new(move |name, payload| {
            let _ = tx.send((name.to_string(), payload));
        }),
    });
    {
        let daemon = Arc::clone(daemon);
        tokio::spawn(async move {
            while let Some((name, payload)) = rx.recv().await {
                let Some((cmd, args)) = mcp_event_command(&name, &payload) else {
                    continue;
                };
                if let Err(e) = daemon.handle(cmd, &args).await {
                    log::warn!("[daemon] MCP {cmd}: {e}");
                }
            }
        });
    }
    match crate::mcp::start_server(host, config.port, config.token).await {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::error!("[daemon] MCP server failed to start: {e}");
            None
        }
    }
}

/// The MCP playback tools emit the events the GUI frontend listens for; map
/// them onto daemon commands. Library-change events (playlist created,
/// favorite changed) only refresh GUI views, so they map to nothing.
fn mcp_event_command(name: &str, p: &Value) -> Option<(&'static str, Value)> {
    use crate::mcp::events::*;
    Some(match name {
        EV_PLAY_TRACKS => (
            "play_tracks",
            json!({ "track_ids": p["trackIds"], "action": p["action"] }),
        ),
        EV_PLAY_SOURCE => ("play_source", json!({ "source_type": p["sourceType"], "id": p["id"] })),
        EV_SHUFFLE_SOURCE => (
            "shuffle_source",
            json!({ "source_type": p["sourceType"], "id": p["id"] }),
        ),
        EV_PAUSE => ("pause", Value::Null),
        EV_RESUME => ("resume", Value::Null),
        EV_SKIP_NEXT => ("next", Value::Null),
        EV_SKIP_PREVIOUS => ("previous", Value::Null),
        EV_SEEK => ("seek", json!({ "position_seconds": p["positionSeconds"] })),
        EV_SET_VOLUME => ("volume", json!({ "level": p["level"] })),
        EV_CLEAR_QUEUE => ("clear_queue", Value::Null),
        EV_REMOVE_FROM_QUEUE => ("remove", json!({ "track_id": p["trackId"] })),
        EV_SET_REPEAT => ("repeat", json!({ "mode": p["mode"] })),
        EV_TOGGLE_SHUFFLE => ("shuffle", json!({})),
        _ => return None,
    })
}

struct DaemonHost {
    daemon: Arc<Daemon>,
    sink: EventSink,
}

#[async_trait::async_trait]
impl McpHost for DaemonHost {
    fn tidal_client(&self) -> &Mutex<TidalClient> {
        &self.daemon.engine.tidal_client
    }

    fn sink(&self) -> &EventSink {
        &self.sink
    }

    fn disk_cache(&self) -> &DiskCache {
        &self.daemon.engine.disk_cache
    }

    async fn now_playing(&self) -> Option<NowPlayingSnapshot> {
        let p = self.daemon.player.lock().await;
        if p.state == PlayState::Stopped {
            return None;
        }
        let t = &p.queue.current()?.track;
        Some(NowPlayingSnapshot {
            track_id: Some(t.id),
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            duration_seconds: t.duration_seconds,
            position_seconds: self.daemon.position() as u32,
            is_playing: p.state == PlayState::Playing,
        })
    }

    async fn queue(&self) -> Vec<QueueTrackSnapshot> {
        let p = self.daemon.player.lock().await;
        p.queue
            .upcoming()
            .map(|e| QueueTrackSnapshot {
                id: e.track.id,
                title: e.track.title.clone(),
                artist: e.track.artist.clone(),
            })
            .collect()
    }
}

impl Daemon {
    async fn serve(self: Arc<Self>, stream: UnixStream) -> std::io::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let mut events: Option<broadcast::Receiver<String>> = None;
        loop {
            let event = async {
                match events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else { return Ok(()) };
                    let reply = match serde_json::from_str::<Request>(&line) {
                        Ok(req) if req.cmd == "subscribe" => {
                            events = Some(self.events.subscribe());
                            json!({ "id": req.id, "ok": true, "result": self.status().await })
                        }
                        Ok(req) => match self.handle(&req.cmd, &req.args).await {
                            Ok(result) => json!({ "id": req.id, "ok": true, "result": result }),
                            Err(e) => json!({ "id": req.id, "ok": false, "error": e.to_string() }),
                        },
                        Err(e) => json!({ "id": null, "ok": false, "error": format!("bad request: {e}") }),
                    };
                    write.write_all(format!("{reply}\n").as_bytes()).await?;
                }
                ev = event => match ev {
                    Ok(line) => write.write_all(format!("{line}\n").as_bytes()).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }

    async fn handle(&self, cmd: &str, args: &Value) -> Result<Value, SoneError> {
        let audio = &self.engine.audio_player;
        match cmd {
            "status" => return Ok(self.status().await),
            "queue" => {
                let limit = args["limit"].as_u64().unwrap_or(50) as usize;
                let p = self.player.lock().await;
                let tracks: Vec<&QueueTrack> =
                    p.queue.upcoming().take(limit).map(|e| &e.track).collect();
                return Ok(json!({
                    "current": p.queue.current().map(|e| &e.track),
                    "upcoming": tracks,
                }));
            }
            "play_tracks" => {
                let ids: Vec<u64> = serde_json::from_value(args["track_ids"].clone())
                    .map_err(|e| SoneError::Parse(format!("track_ids: {e}")))?;
                let tracks = self.load_tracks(&ids).await?;
                let action = args["action"].as_str().unwrap_or("play_now");
                self.enqueue(tracks, action).await?;
            }
            "play_source" | "shuffle_source" => {
                let source_type = arg_str(args, "source_type")?;
                let id = arg_str(args, "id")?;
                let tracks = self.load_source(source_type, id).await?;
                if tracks.is_empty() {
                    return Err(SoneError::NotConfigured(format!("{source_type} {id} has no playable tracks")));
                }
                let mut rand = rand_below;
                let start = match args["start"].as_u64() {
                    Some(i) => i as usize,
                    None if cmd == "shuffle_source" => rand(tracks.len()),
                    None => 0,
                };
                if cmd == "shuffle_source" {
                    self.player.lock().await.queue.set_shuffle(true, &mut rand);
                }
                self.player.lock().await.queue.replace(tracks, start, &mut rand);
                self.start_current().await?;
            }
            "login_start" => {
                let params = crate::pkce::build_pkce_params(&crate::embedded_config::stream_key_c());
                *self.pending_login.lock().await = Some((params.code_verifier, params.client_unique_key));
                return Ok(json!({ "url": params.authorize_url }));
            }
            "login_finish" => {
                let pasted = arg_str(args, "redirect_url")?;
                let code = crate::pkce::code_from_redirect(pasted).ok_or_else(|| {
                    SoneError::Parse("no login code in that: paste the address the browser ended on".into())
                })?;
                let (verifier, key) = self
                    .pending_login
                    .lock()
                    .await
                    .clone()
                    .ok_or_else(|| SoneError::Parse("no login in progress: run login first".into()))?;
                self.engine.login(&code, &verifier, &key).await?;
                *self.pending_login.lock().await = None;
                self.logged_in.store(true, Ordering::Relaxed);
                log::info!("[daemon] logged in");
                self.publish_state().await;
            }
            "logout" => {
                self.stop().await;
                {
                    let mut p = self.player.lock().await;
                    let repeat = p.queue.repeat();
                    p.queue = Queue::default();
                    p.queue.set_repeat(repeat);
                }
                self.engine.logout().await?;
                self.logged_in.store(false, Ordering::Relaxed);
                log::info!("[daemon] logged out");
                self.publish_state().await;
            }
            "settings" => return Ok(self.settings_json().await),
            "set_setting" => {
                self.set_setting(arg_str(args, "name")?, &args["value"]).await?;
                return Ok(self.settings_json().await);
            }
            "playlists" => return self.playlists().await,
            "delete_playlist" => {
                let uuid = arg_str(args, "uuid")?;
                self.delete_playlist(uuid).await?;
                return Ok(json!({ "uuid": uuid }));
            }
            "rename_playlist" => {
                let name = arg_str(args, "name")?.trim();
                if name.is_empty() {
                    return Err(SoneError::Parse("empty playlist name".into()));
                }
                self.rename_playlist(arg_str(args, "uuid")?, name).await?;
                return Ok(json!({ "uuid": args["uuid"], "title": name }));
            }
            "source_tracks" => {
                let source_type = arg_str(args, "source_type")?;
                let id = arg_str(args, "id")?;
                return Ok(json!({ "tracks": self.load_source(source_type, id).await? }));
            }
            "artist" => {
                let id = args["id"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| args["id"].as_u64())
                    .ok_or_else(|| SoneError::Parse("missing numeric argument id".into()))?;
                return self.artist(id).await;
            }
            "search" => {
                let query = arg_str(args, "query")?;
                let limit = args["limit"].as_u64().unwrap_or(20).min(50) as u32;
                return self.search(query, limit).await;
            }
            "pause" => {
                audio.pause().map_err(SoneError::Audio)?;
                self.set_state(PlayState::Paused).await;
            }
            "resume" => self.resume().await?,
            "toggle" => {
                let state = self.player.lock().await.state;
                if state == PlayState::Playing {
                    audio.pause().map_err(SoneError::Audio)?;
                    self.set_state(PlayState::Paused).await;
                } else {
                    self.resume().await?;
                }
            }
            "next" => {
                let moved = self.player.lock().await.queue.skip_next().is_some();
                if moved {
                    self.start_current().await?;
                } else {
                    self.stop().await;
                }
            }
            "previous" => {
                let pos = self.position();
                let decision = self.player.lock().await.queue.previous(pos);
                match decision {
                    Some(Previous::Moved) => self.start_current().await?,
                    Some(Previous::Restart) => {
                        audio.seek(0.0).map_err(SoneError::Audio)?;
                        self.publish("seeked", json!({ "positionSeconds": 0.0 }));
                    }
                    None => {}
                }
            }
            "jump" => {
                let n = args["index"].as_u64().unwrap_or(0) as usize;
                let moved = self.player.lock().await.queue.jump_upcoming(n).is_some();
                if !moved {
                    return Err(SoneError::NotConfigured(format!("no upcoming entry {n}")));
                }
                self.start_current().await?;
            }
            "seek" => {
                let secs = args["position_seconds"].as_f64().unwrap_or(0.0) as f32;
                audio.seek(secs).map_err(SoneError::Audio)?;
                self.publish("seeked", json!({ "positionSeconds": secs }));
            }
            "volume" => {
                let level = (args["level"].as_f64().unwrap_or(1.0) as f32).clamp(0.0, 1.0);
                audio.set_volume(level).map_err(SoneError::Audio)?;
                self.player.lock().await.volume = level;
                self.publish_state().await;
            }
            "repeat" => {
                let mode: Repeat = serde_json::from_value(args["mode"].clone())
                    .map_err(|e| SoneError::Parse(format!("mode: {e}")))?;
                self.player.lock().await.queue.set_repeat(mode);
                self.arm_next().await;
                self.publish_state().await;
            }
            "shuffle" => {
                let mut p = self.player.lock().await;
                let on = args["on"].as_bool().unwrap_or(!p.queue.shuffle());
                p.queue.set_shuffle(on, &mut rand_below);
                drop(p);
                self.arm_next().await;
                self.publish_state().await;
            }
            "clear_queue" => {
                self.player.lock().await.queue.clear_upcoming();
                self.arm_next().await;
            }
            "remove" => {
                let id = args["track_id"].as_u64().unwrap_or(0);
                if !self.player.lock().await.queue.remove_upcoming(id) {
                    return Err(SoneError::NotConfigured(format!("track {id} is not upcoming")));
                }
                self.arm_next().await;
            }
            "stop" => self.stop().await,
            other => return Err(SoneError::Parse(format!("unknown command {other}"))),
        }
        Ok(self.status().await)
    }

    async fn enqueue(&self, tracks: Vec<QueueTrack>, action: &str) -> Result<(), SoneError> {
        match action {
            "play_now" => {
                self.player.lock().await.queue.replace(tracks, 0, &mut rand_below);
                self.start_current().await
            }
            "queue" | "play_next" => {
                let start = {
                    let mut p = self.player.lock().await;
                    if action == "queue" {
                        p.queue.append(tracks);
                    } else {
                        p.queue.insert_next(tracks);
                    }
                    p.state == PlayState::Stopped
                };
                if start {
                    self.start_current().await
                } else {
                    self.arm_next().await;
                    Ok(())
                }
            }
            other => Err(SoneError::Parse(format!(
                "unknown action {other} (play_now, queue, play_next)"
            ))),
        }
    }

    async fn resume(&self) -> Result<(), SoneError> {
        let state = self.player.lock().await.state;
        match state {
            PlayState::Paused => {
                self.engine.audio_player.resume().map_err(SoneError::Audio)?;
                self.set_state(PlayState::Playing).await;
                Ok(())
            }
            PlayState::Stopped => self.start_current().await,
            PlayState::Playing => Ok(()),
        }
    }

    async fn stop(&self) {
        let _ = self.engine.audio_player.stop();
        self.set_state(PlayState::Stopped).await;
    }

    /// Start the queue's current entry from the beginning, then pre-arm the
    /// next one for a gapless transition.
    async fn start_current(&self) -> Result<(), SoneError> {
        let Some(track_id) = self.player.lock().await.queue.current().map(|e| e.track.id) else {
            self.stop().await;
            return Ok(());
        };
        let (_info, uri, gain, _rg, _peak, _is_dash) = self.engine.resolve(track_id, true).await?;
        let player = Arc::clone(&self.engine.audio_player);
        tokio::task::spawn_blocking(move || {
            player.set_normalization_gain(gain)?;
            player.play_url(&uri, None)
        })
        .await
        .map_err(|e| SoneError::Audio(e.to_string()))?
        .map_err(SoneError::Audio)?;
        self.set_state(PlayState::Playing).await;
        self.arm_next().await;
        Ok(())
    }

    /// Hand the audio engine the entry after the current one, so it can switch
    /// without a gap. With nothing next (or gapless off) the slot is cleared.
    async fn arm_next(&self) {
        let audio = &self.engine.audio_player;
        let _ = audio.clear_next_track();
        if !self.engine.settings().gapless {
            return;
        }
        let next = {
            let p = self.player.lock().await;
            if p.state == PlayState::Stopped {
                return;
            }
            p.queue.peek_next().map(|e| (e.track.id, e.qid))
        };
        let Some((track_id, qid)) = next else { return };
        match self.engine.resolve(track_id, true).await {
            Ok((_info, uri, gain, rg, peak, is_dash)) => {
                if let Err(e) =
                    audio.set_next_track(uri, gain, track_id, qid.to_string(), rg, peak, is_dash)
                {
                    log::warn!("[daemon] arming next track {track_id}: {e}");
                }
            }
            Err(e) => log::warn!("[daemon] resolving next track {track_id}: {e}"),
        }
    }

    async fn on_engine_event(&self, name: &str, payload: &Value) {
        match name {
            // The gapless slot became the playing track.
            "track-advanced" => {
                {
                    let mut p = self.player.lock().await;
                    p.queue.advance();
                    let expected = p.queue.current().map(|e| e.qid.to_string());
                    let got = payload["qid"].as_str().map(str::to_string);
                    if expected != got {
                        log::warn!("[daemon] gapless advanced to qid {got:?}, queue expected {expected:?}");
                    }
                }
                self.publish_state().await;
                self.arm_next().await;
            }
            // Playback ended with nothing armed (gapless off, or end of queue).
            "track-finished" => {
                let more = self.player.lock().await.queue.advance().is_some();
                if more {
                    if let Err(e) = self.start_current().await {
                        log::warn!("[daemon] starting next track: {e}");
                        self.stop().await;
                    }
                } else {
                    self.set_state(PlayState::Stopped).await;
                }
            }
            "audio-error" => {
                log::warn!("[daemon] audio error: {payload}");
                self.publish("audio-error", payload.clone());
            }
            _ => {}
        }
    }

    fn position(&self) -> f32 {
        self.engine.audio_player.get_position().unwrap_or(0.0)
    }

    async fn status(&self) -> Value {
        let p = self.player.lock().await;
        json!({
            "state": p.state,
            "track": p.queue.current().map(|e| &e.track),
            "positionSeconds": self.position(),
            "volume": p.volume,
            "repeat": p.queue.repeat(),
            "shuffle": p.queue.shuffle(),
            "upcoming": p.queue.upcoming().count(),
            "loggedIn": self.logged_in.load(Ordering::Relaxed),
        })
    }

    async fn set_state(&self, state: PlayState) {
        self.player.lock().await.state = state;
        self.publish_state().await;
    }

    async fn publish_state(&self) {
        let status = self.status().await;
        self.publish("state", status);
    }

    fn publish(&self, event: &str, data: Value) {
        let _ = self.events.send(json!({ "event": event, "data": data }).to_string());
    }

    /// The settings sonetui's settings page shows and changes.
    async fn settings_json(&self) -> Value {
        if self.devices.get().is_none() {
            let probed = tokio::task::spawn_blocking(crate::audio::list_alsa_devices)
                .await
                .ok()
                .and_then(|r| r.map_err(|e| log::warn!("[daemon] listing audio devices: {e}")).ok())
                .unwrap_or_default();
            let _ = self.devices.set(probed);
        }
        let s = self.engine.settings();
        json!({
            "maxQuality": s.max_quality,
            "gapless": s.gapless,
            "exclusiveMode": s.exclusive_mode,
            "bitPerfect": s.bit_perfect,
            "exclusiveDevice": s.exclusive_device,
            "devices": self.devices.get(),
            "loggedIn": self.logged_in.load(Ordering::Relaxed),
        })
    }

    /// Change one setting, applying it to the audio engine the way the app's
    /// setters do (commands/utility.rs), and save it.
    async fn set_setting(&self, name: &str, value: &Value) -> Result<(), SoneError> {
        let audio = &self.engine.audio_player;
        let flag = || value.as_bool().ok_or_else(|| SoneError::Parse(format!("{name} takes true or false")));
        let text = || value.as_str().ok_or_else(|| SoneError::Parse(format!("{name} takes a string")));
        let device = self.engine.settings().exclusive_device;
        match name {
            "max_quality" => {
                let q = text()?.to_string();
                if !matches!(q.as_str(), "HI_RES_LOSSLESS" | "LOSSLESS" | "HIGH") {
                    return Err(SoneError::Parse(format!("max_quality: HI_RES_LOSSLESS, LOSSLESS or HIGH, not {q}")));
                }
                self.engine.update_settings(|s| s.max_quality = q.clone())?;
            }
            "gapless" => {
                let on = flag()?;
                audio.set_gapless(on).map_err(SoneError::Audio)?;
                self.engine.update_settings(|s| s.gapless = on)?;
                self.arm_next().await;
            }
            "exclusive_mode" => {
                let on = flag()?;
                if !on {
                    audio.set_bit_perfect(false).map_err(SoneError::Audio)?;
                }
                audio.set_exclusive_mode(on, device).map_err(SoneError::Audio)?;
                self.engine.update_settings(|s| {
                    s.exclusive_mode = on;
                    s.bit_perfect &= on;
                })?;
            }
            "bit_perfect" => {
                let on = flag()?;
                if on {
                    audio.set_exclusive_mode(true, device).map_err(SoneError::Audio)?;
                }
                audio.set_bit_perfect(on).map_err(SoneError::Audio)?;
                self.engine.update_settings(|s| {
                    s.bit_perfect = on;
                    s.exclusive_mode |= on;
                })?;
            }
            "exclusive_device" => {
                let dev = text()?.to_string();
                let enabled = self.engine.settings().exclusive_mode;
                audio.set_exclusive_mode(enabled, Some(dev.clone())).map_err(SoneError::Audio)?;
                self.engine.update_settings(|s| s.exclusive_device = Some(dev.clone()))?;
            }
            other => {
                return Err(SoneError::Parse(format!(
                    "unknown setting {other} (max_quality, gapless, exclusive_mode, bit_perfect, exclusive_device)"
                )))
            }
        }
        Ok(())
    }

    /// The user's playlists: owned, then favourited, without duplicates.
    async fn playlists(&self) -> Result<Value, SoneError> {
        let mut client = self.engine.tidal_client.lock().await;
        let user_id = client
            .tokens
            .as_ref()
            .and_then(|t| t.user_id)
            .ok_or(SoneError::NotAuthenticated)?;
        let owned = client.get_user_playlists(user_id, 0, 200).await?;
        let favorited = client.get_favorite_playlists(user_id, 0, 200).await?;
        let mut seen = std::collections::HashSet::new();
        let playlists: Vec<Value> = owned
            .items
            .into_iter()
            .chain(favorited.items)
            .filter(|p| seen.insert(p.uuid.clone()))
            .map(|p| playlist_json(&p))
            .collect();
        Ok(json!({ "playlists": playlists }))
    }

    /// Delete a playlist for good, as the app does
    /// (`commands::library::delete_playlist`).
    async fn delete_playlist(&self, uuid: &str) -> Result<(), SoneError> {
        let client = self.engine.tidal_client.lock().await;
        client.delete_playlist(uuid).await?;
        let user_id = client.tokens.as_ref().and_then(|t| t.user_id);
        drop(client);
        log::info!("[daemon] deleted playlist {uuid}");
        let cache = &self.engine.disk_cache;
        if let Some(uid) = user_id {
            cache.invalidate_tag(&format!("user:{uid}")).await;
        }
        cache.invalidate_tag(&format!("playlist:{uuid}")).await;
        cache.invalidate_tag("folders").await;
        Ok(())
    }

    /// Rename a playlist, keeping its description and visibility, as the
    /// app's playlist editor does (`commands::library::update_playlist`).
    async fn rename_playlist(&self, uuid: &str, name: &str) -> Result<(), SoneError> {
        let mut client = self.engine.tidal_client.lock().await;
        let current = client.get_playlist_details(uuid).await?;
        let description = current["description"].as_str().unwrap_or("").to_string();
        // v1 answers `publicPlaylist`, the update takes `accessType`; never
        // guess, a wrong one would publish a private playlist.
        let access = match (current["accessType"].as_str(), current["publicPlaylist"].as_bool()) {
            (Some(a), _) => a.to_string(),
            (None, Some(true)) => "PUBLIC".to_string(),
            (None, Some(false)) => "UNLISTED".to_string(),
            (None, None) => return Err(SoneError::Parse(format!("playlist {uuid}: unknown visibility"))),
        };
        client.update_playlist(uuid, name, &description, &access).await?;
        let user_id = client.tokens.as_ref().and_then(|t| t.user_id);
        drop(client);
        log::info!("[daemon] renamed playlist {uuid}");
        let cache = &self.engine.disk_cache;
        if let Some(uid) = user_id {
            cache.invalidate_tag(&format!("user:{uid}")).await;
        }
        cache.invalidate_tag(&format!("playlist:{uuid}")).await;
        cache.invalidate_tag("folders").await;
        Ok(())
    }

    async fn search(&self, query: &str, limit: u32) -> Result<Value, SoneError> {
        let r = self.engine.tidal_client.lock().await.search(query, limit).await?;
        let tracks: Vec<QueueTrack> = r.tracks.into_iter().map(queue_track).collect();
        let albums: Vec<Value> = r.albums.iter().map(album_json).collect();
        let artists: Vec<Value> = r.artists.iter().map(|a| json!({ "id": a.id, "name": a.name })).collect();
        let playlists: Vec<Value> = r.playlists.iter().map(playlist_json).collect();
        Ok(json!({ "tracks": tracks, "albums": albums, "artists": artists, "playlists": playlists }))
    }

    /// An artist's albums and EPs (singles left out), and the top tracks
    /// that `play_source artist` plays, in the same order. Releases come from
    /// the v2 artist page's sections; v1 `/artists/{id}/albums` answers 404.
    async fn artist(&self, id: u64) -> Result<Value, SoneError> {
        let mut client = self.engine.tidal_client.lock().await;
        let mut albums = Vec::new();
        for section in ["ARTIST_ALBUMS", "ARTIST_TOP_SINGLES"] {
            let page = client
                .get_artist_view_all(id, &format!("artist/{section}/view-all"), 0, 50)
                .await?;
            albums.extend(
                page["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| serde_json::from_value::<TidalAlbumDetail>(item["data"].clone()).ok())
                    .filter(|a| a.album_type.as_deref() != Some("SINGLE"))
                    .map(|a| album_json(&a)),
            );
        }
        let tracks: Vec<QueueTrack> =
            client.get_artist_top_tracks(id, 50).await?.into_iter().map(queue_track).collect();
        Ok(json!({ "albums": albums, "tracks": tracks }))
    }

    async fn load_tracks(&self, ids: &[u64]) -> Result<Vec<QueueTrack>, SoneError> {
        let mut client = self.engine.tidal_client.lock().await;
        let mut tracks = Vec::with_capacity(ids.len());
        for &id in ids {
            let value = client.get_track(id).await?;
            let track: TidalTrack = serde_json::from_value(value)
                .map_err(|e| SoneError::Parse(format!("track {id}: {e}")))?;
            tracks.push(queue_track(track));
        }
        Ok(tracks)
    }

    async fn load_source(&self, source_type: &str, id: &str) -> Result<Vec<QueueTrack>, SoneError> {
        let mut client = self.engine.tidal_client.lock().await;
        let parse_id = |id: &str| {
            id.parse::<u64>()
                .map_err(|_| SoneError::Parse(format!("{source_type} id must be numeric: {id}")))
        };
        let tracks = match source_type {
            "playlist" => client.get_playlist_tracks(id).await?,
            "album" => client.get_album_tracks(parse_id(id)?, 0, 500).await?.items,
            "artist" => client.get_artist_top_tracks(parse_id(id)?, 50).await?,
            "mix" => client.get_mix_items(id).await?.tracks,
            other => {
                return Err(SoneError::Parse(format!(
                    "unknown source type {other} (playlist, album, artist, mix)"
                )))
            }
        };
        Ok(tracks.into_iter().map(queue_track).collect())
    }
}

fn queue_track(t: TidalTrack) -> QueueTrack {
    let artist = t
        .artist
        .map(|a| a.name)
        .or_else(|| {
            t.artists
                .map(|v| v.into_iter().map(|a| a.name).collect::<Vec<_>>().join(", "))
        })
        .unwrap_or_default();
    QueueTrack {
        id: t.id,
        title: t.title,
        artist,
        album: t.album.map(|a| a.title),
        duration_seconds: t.duration,
    }
}

fn album_json(a: &TidalAlbumDetail) -> Value {
    let artist = a
        .artist
        .as_ref()
        .map(|x| x.name.clone())
        .or_else(|| a.artists.as_ref().and_then(|v| v.first()).map(|x| x.name.clone()))
        .unwrap_or_default();
    let year = a
        .release_date
        .as_deref()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<u16>().ok());
    json!({ "id": a.id, "title": a.title, "artist": artist, "year": year, "type": a.album_type })
}

fn playlist_json(p: &crate::tidal_api::TidalPlaylist) -> Value {
    json!({ "uuid": p.uuid, "title": p.title, "tracks": p.number_of_tracks.unwrap_or(0) })
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, SoneError> {
    args[key]
        .as_str()
        .ok_or_else(|| SoneError::Parse(format!("missing string argument {key}")))
}

fn rand_below(n: usize) -> usize {
    rand::rng().random_range(0..n)
}

#[cfg(test)]
mod tests {
    use super::parse_mcp_url;

    #[test]
    fn parses_mcp_url() {
        let c = parse_mcp_url("http://127.0.0.1:5577/abc123/mcp\n").unwrap();
        assert_eq!((c.port, c.token.as_str()), (5577, "abc123"));
    }

    #[test]
    fn rejects_malformed_mcp_urls() {
        for url in [
            "",
            "https://127.0.0.1:5577/abc/mcp",
            "http://127.0.0.1/abc/mcp",
            "http://127.0.0.1:5577//mcp",
            "http://127.0.0.1:5577/abc",
            "http://127.0.0.1:5577/abc/mcp/x",
        ] {
            assert!(parse_mcp_url(url).is_none(), "{url}");
        }
    }
}
