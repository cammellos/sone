//! MPRIS for the daemon, so `playerctl`, media keys and desktop widgets
//! control it. Controls become daemon commands; the daemon's `state`,
//! `position` and `seeked` events update the D-Bus player.
//!
//! Registered as `org.mpris.MediaPlayer2.soned`, distinct from the GUI's
//! name, so the two never fight over one bus name.

use std::rc::Rc;
use std::sync::Arc;

use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use super::Daemon;

type Control = (&'static str, Value);

pub(super) async fn spawn(daemon: Arc<Daemon>) {
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Control>();
    let (upd_tx, upd_rx) = mpsc::unbounded_channel::<Value>();

    // MPRIS controls → daemon commands.
    {
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            while let Some((cmd, args)) = ctl_rx.recv().await {
                if let Err(e) = daemon.handle(cmd, &args).await {
                    log::warn!("[mpris] {cmd}: {e}");
                }
            }
        });
    }

    // Daemon events → MPRIS properties, starting from the current state.
    let _ = upd_tx.send(json!({ "event": "state", "data": daemon.status().await }));
    let mut events = daemon.events.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if upd_tx.send(v).is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // `Player` is !Send: it lives on its own thread with a local runtime.
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("[mpris] runtime: {e}");
                return;
            }
        };
        tokio::task::LocalSet::new().block_on(&rt, run_player(ctl_tx, upd_rx));
    });
}

async fn run_player(ctl: mpsc::UnboundedSender<Control>, mut updates: mpsc::UnboundedReceiver<Value>) {
    let player = match Player::builder("soned")
        .can_play(true)
        .can_pause(true)
        .can_go_next(true)
        .can_go_previous(true)
        .can_seek(true)
        .can_control(true)
        .has_track_list(false)
        .rate(1.0)
        .minimum_rate(1.0)
        .maximum_rate(1.0)
        .build()
        .await
    {
        Ok(p) => Rc::new(p),
        Err(e) => {
            log::error!("[mpris] cannot register on the session bus: {e}");
            return;
        }
    };
    player.set_identity("SONE (headless)").await.ok();

    let send = |cmd: &'static str, args: Value| {
        let ctl = ctl.clone();
        move || {
            let _ = ctl.send((cmd, args.clone()));
        }
    };
    let f = send("toggle", Value::Null);
    player.connect_play_pause(move |_| f());
    let f = send("resume", Value::Null);
    player.connect_play(move |_| f());
    let f = send("pause", Value::Null);
    player.connect_pause(move |_| f());
    let f = send("stop", Value::Null);
    player.connect_stop(move |_| f());
    let f = send("next", Value::Null);
    player.connect_next(move |_| f());
    let f = send("previous", Value::Null);
    player.connect_previous(move |_| f());

    let c = ctl.clone();
    player.connect_seek(move |p, offset| {
        let secs = (p.position().as_micros() + offset.as_micros()).max(0) as f64 / 1e6;
        let _ = c.send(("seek", json!({ "position_seconds": secs })));
    });
    let c = ctl.clone();
    player.connect_set_position(move |_, _track, position| {
        let secs = position.as_micros().max(0) as f64 / 1e6;
        let _ = c.send(("seek", json!({ "position_seconds": secs })));
    });
    let c = ctl.clone();
    player.connect_set_volume(move |_, level| {
        let _ = c.send(("volume", json!({ "level": level })));
    });
    let c = ctl.clone();
    player.connect_set_shuffle(move |_, on| {
        let _ = c.send(("shuffle", json!({ "on": on })));
    });
    let c = ctl.clone();
    player.connect_set_loop_status(move |_, status| {
        let mode = match status {
            LoopStatus::None => "off",
            LoopStatus::Playlist => "all",
            LoopStatus::Track => "one",
        };
        let _ = c.send(("repeat", json!({ "mode": mode })));
    });

    tokio::task::spawn_local(player.run());
    log::info!("[mpris] registered as org.mpris.MediaPlayer2.soned");

    let mut current_track: Option<u64> = None;
    while let Some(update) = updates.recv().await {
        let data = &update["data"];
        match update["event"].as_str() {
            Some("state") => apply_state(&player, data, &mut current_track).await,
            Some("position") => player.set_position(secs_to_time(data["positionSeconds"].as_f64())),
            Some("seeked") => {
                let t = secs_to_time(data["positionSeconds"].as_f64());
                player.set_position(t);
                player.seeked(t).await.ok();
            }
            _ => {}
        }
    }
}

async fn apply_state(player: &Player, s: &Value, current_track: &mut Option<u64>) {
    let track = s.get("track").filter(|t| !t.is_null());
    let track_id = track.and_then(|t| t["id"].as_u64());
    if track_id != *current_track {
        *current_track = track_id;
        let mut metadata = Metadata::new();
        if let Some(t) = track {
            if let Some(id) = track_id {
                let path = format!("/org/mpris/MediaPlayer2/Track/{id}");
                if let Ok(tid) = TrackId::try_from(path.as_str()) {
                    metadata.set_trackid(Some(tid));
                }
            }
            metadata.set_title(t["title"].as_str());
            metadata.set_artist(t["artist"].as_str().map(|a| [a]));
            metadata.set_album(t["album"].as_str());
            metadata.set_length(Some(secs_to_time(t["durationSeconds"].as_f64())));
        }
        player.set_metadata(metadata).await.ok();
    }

    let status = match s["state"].as_str() {
        Some("playing") => PlaybackStatus::Playing,
        Some("paused") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    };
    player.set_playback_status(status).await.ok();
    player.set_position(secs_to_time(s["positionSeconds"].as_f64()));
    if let Some(v) = s["volume"].as_f64() {
        player.set_volume(v).await.ok();
    }
    if let Some(on) = s["shuffle"].as_bool() {
        player.set_shuffle(on).await.ok();
    }
    let loop_status = match s["repeat"].as_str() {
        Some("all") => LoopStatus::Playlist,
        Some("one") => LoopStatus::Track,
        _ => LoopStatus::None,
    };
    player.set_loop_status(loop_status).await.ok();
}

fn secs_to_time(secs: Option<f64>) -> Time {
    Time::from_micros((secs.unwrap_or(0.0).max(0.0) * 1e6) as i64)
}
