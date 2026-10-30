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

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use app::{App, Mouse};
use client::{Client, Incoming};
use keys::Key;

pub async fn run(terminal: &mut DefaultTerminal, socket: PathBuf, config_path: Option<PathBuf>) -> std::io::Result<()> {
    let (config, errors) = match &config_path {
        Some(p) => config::load(p),
        None => Default::default(),
    };
    let _mouse = if config.mouse { Some(MouseCapture::enable()?) } else { None };
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

        if let Some(url) = app.open_url.take() {
            open_in_browser(&mut app, &url);
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
                Some(Ok(Event::Mouse(m))) => {
                    if let Some(mouse) = to_mouse(m) {
                        app.on_mouse(mouse, std::time::Instant::now());
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
            _ = tick.tick() => app.on_tick(std::time::Instant::now()),
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

/// xdg-open, detached; its output would draw over the TUI.
fn open_in_browser(app: &mut App, url: &str) {
    let spawned = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        // The prompt would hide the message: there's nothing to paste anyway.
        app.mode = app::Mode::Normal;
        app.error(format!("can't open the browser ({e}): sign in with `sonectl login` instead"));
    }
}

/// Mouse reporting on while it lives. Dropping it turns reporting off, also on
/// an error or a panic, or the shell would get the mouse's escape codes.
struct MouseCapture;

impl MouseCapture {
    fn enable() -> std::io::Result<Self> {
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
}

fn to_mouse(m: MouseEvent) -> Option<Mouse> {
    Some(match m.kind {
        MouseEventKind::Down(MouseButton::Left) => Mouse::Click { column: m.column, row: m.row },
        MouseEventKind::ScrollUp => Mouse::ScrollUp,
        MouseEventKind::ScrollDown => Mouse::ScrollDown,
        _ => return None,
    })
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
