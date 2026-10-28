//! `sonetui`: terminal client for soned. Tabs, an nvim-style statusline and
//! command line, Workman-friendly keys; keys and colours in
//! `~/.config/sone/tui.toml` (see `config`).

pub mod app;
pub mod client;
pub mod config;
pub mod keys;
mod ui;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use app::App;
use client::{Client, Incoming};
use keys::Key;

pub async fn run(terminal: &mut DefaultTerminal, socket: PathBuf, config_path: Option<PathBuf>) -> std::io::Result<()> {
    let (config, errors) = match &config_path {
        Some(p) => config::load(p),
        None => Default::default(),
    };
    let mut app = App::new(config);
    if !errors.is_empty() {
        app.error(errors.join("; "));
    }

    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut conn: Option<(Client, mpsc::UnboundedReceiver<Incoming>)> = None;
    let mut retry_at = tokio::time::Instant::now();

    while !app.quit {
        if conn.is_none() && tokio::time::Instant::now() >= retry_at {
            match connect(&socket).await {
                Ok(c) => {
                    conn = Some(c);
                    app.on_connected();
                }
                Err(_) => {
                    if app.connected || app.message.is_none() {
                        app.on_disconnected();
                    }
                    retry_at = tokio::time::Instant::now() + Duration::from_secs(2);
                }
            }
        }

        if let Some((client, _)) = conn.as_mut() {
            let mut failed = false;
            for (cmd, args, expect) in std::mem::take(&mut app.outbox) {
                match client.request(cmd, args).await {
                    Ok(id) => app.sent(id, expect),
                    Err(_) => failed = true,
                }
            }
            if failed {
                conn = None;
                app.on_disconnected();
            }
        } else {
            app.outbox.clear();
        }

        terminal.draw(|f| ui::draw(&mut app, f))?;

        let incoming = async {
            match conn.as_mut() {
                Some((_, rx)) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => {
                    if let Some(key) = to_key(k) {
                        app.on_key(key);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => break,
            },
            msg = incoming => match msg {
                Some(Incoming::Reply { id, result }) => app.on_reply(id, result),
                Some(Incoming::Event { name, data }) => app.on_event(&name, &data),
                Some(Incoming::Disconnected) | None => {
                    conn = None;
                    app.on_disconnected();
                    retry_at = tokio::time::Instant::now() + Duration::from_secs(2);
                }
            },
            _ = tick.tick() => {}
        }
    }
    Ok(())
}

async fn connect(socket: &Path) -> std::io::Result<(Client, mpsc::UnboundedReceiver<Incoming>)> {
    let (mut client, rx) = Client::connect(socket).await?;
    // The subscribe reply is the current status; it arrives like any other.
    client.request("subscribe", serde_json::Value::Null).await?;
    Ok((client, rx))
}

fn to_key(k: KeyEvent) -> Option<Key> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        _ => return None,
    })
}
