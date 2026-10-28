//! The TUI's state and what keys and daemon messages do to it. Requests to
//! the daemon are queued in `outbox` with what their reply will be; the event
//! loop sends them and hands the replies back.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{json, Value};

use super::config::Config;
use super::keys::{Action, Key, Match};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub duration: f64,
}

impl Track {
    fn from_json(v: &Value) -> Option<Self> {
        if v.is_null() {
            return None;
        }
        Some(Self {
            id: v["id"].as_u64()?,
            title: str_of(&v["title"]),
            artist: str_of(&v["artist"]),
            duration: v["durationSeconds"].as_f64().unwrap_or(0.0),
        })
    }
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or("?").to_string()
}

/// One line in a browse list.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Header(String),
    Track(Track),
    Playlist { uuid: String, title: String, tracks: u64 },
    /// `kind` is Tidal's release type: "ALBUM", "EP", "SINGLE".
    Album { id: u64, title: String, artist: String, year: Option<u16>, kind: Option<String> },
    Artist { id: u64, name: String },
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Header(_))
    }
}

/// A list shown in the Playlists or Search tab. Track lists opened from a
/// playlist/album/artist remember it as `source`, so `<CR>` plays the whole
/// source from the selected track.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub title: String,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub source: Option<(String, String)>,
    pub loading: bool,
}

impl View {
    fn loading(title: impl Into<String>) -> Self {
        Self { title: title.into(), loading: true, ..Self::default() }
    }

    fn first_selectable(&mut self) {
        self.selected = self.rows.iter().position(Row::selectable).unwrap_or(0);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: String,
    pub track: Option<Track>,
    pub volume: f64,
    pub repeat: String,
    pub shuffle: bool,
    pub logged_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Search,
    /// The address the browser ended on after signing in (`;login`).
    Login,
}

pub enum Mode {
    Normal,
    Prompt(PromptKind, String),
}

pub struct Message {
    pub text: String,
    pub error: bool,
}

/// A line of the settings page (`;settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Header(&'static str),
    MaxQuality,
    Gapless,
    ExclusiveMode,
    BitPerfect,
    Device,
    Account,
}

pub const SETTING_ROWS: &[Setting] = &[
    Setting::Header("Audio"),
    Setting::MaxQuality,
    Setting::Gapless,
    Setting::ExclusiveMode,
    Setting::BitPerfect,
    Setting::Device,
    Setting::Header("Account"),
    Setting::Account,
];

const QUALITIES: &[&str] = &["HI_RES_LOSSLESS", "LOSSLESS", "HIGH"];

/// The settings page: opened from the command line, shown in place of the
/// current tab until `<BS>` or a tab switch.
#[derive(Debug, Clone, Default)]
pub struct SettingsPage {
    /// The daemon's `settings` reply; Null while loading.
    pub values: Value,
    pub selected: usize,
    /// `<CR>` on "Log out" once: the next `<CR>` there logs out.
    pub confirm_logout: bool,
}

pub const TABS: &[&str] = &["Queue", "Playlists", "Search"];
pub const QUEUE: usize = 0;
pub const PLAYLISTS: usize = 1;
pub const SEARCH: usize = 2;

/// What a request's reply is.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    /// Commands reply with the player status.
    Status,
    Queue,
    Playlists,
    /// A source's tracks, for the top view of `tab` if it's still the one waiting.
    Tracks { tab: usize, source: (String, String) },
    /// An artist's albums and top tracks, like `Tracks`.
    Artist { tab: usize, source: (String, String) },
    Search { query: String },
    /// `login_start`: the sign-in page to open.
    LoginStart,
    /// `login_finish`: the status once logged in.
    LoginFinish,
    /// `settings` / `set_setting`: the settings page's values.
    Settings,
}

pub struct App {
    pub config: Config,
    pub status: Status,
    pub queue_current: Option<Track>,
    pub upcoming: Vec<Track>,
    /// Row in the queue list: 0 is the current track, 1.. the upcoming ones.
    pub queue_selected: usize,
    /// View stacks of the Playlists and Search tabs (index = tab).
    pub stacks: [Vec<View>; 3],
    pub tab: usize,
    pub mode: Mode,
    pub message: Option<Message>,
    pub connected: bool,
    pub quit: bool,
    /// Rows the list showed last frame, for half-page moves.
    pub list_height: usize,
    pending: Vec<Key>,
    position: f64,
    position_at: Instant,
    last_query: Option<String>,
    pub outbox: Vec<(&'static str, Value, Expect)>,
    /// A page to open in the browser; the event loop runs xdg-open.
    pub open_url: Option<String>,
    pub settings: Option<SettingsPage>,
    expecting: HashMap<u64, Expect>,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            status: Status::default(),
            queue_current: None,
            upcoming: Vec::new(),
            queue_selected: 0,
            stacks: Default::default(),
            tab: QUEUE,
            mode: Mode::Normal,
            message: None,
            connected: false,
            quit: false,
            list_height: 10,
            pending: Vec::new(),
            position: 0.0,
            position_at: Instant::now(),
            last_query: None,
            outbox: Vec::new(),
            open_url: None,
            settings: None,
            expecting: HashMap::new(),
        }
    }

    /// Playback position, interpolated between the daemon's once-a-second events.
    pub fn position(&self) -> f64 {
        if self.status.state == "playing" {
            self.position + self.position_at.elapsed().as_secs_f64()
        } else {
            self.position
        }
    }

    pub fn queue_rows(&self) -> usize {
        self.queue_current.is_some() as usize + self.upcoming.len()
    }

    /// The view the current tab shows (None on the Queue tab).
    pub fn view(&self) -> Option<&View> {
        self.stacks[self.tab].last()
    }

    fn view_mut(&mut self) -> Option<&mut View> {
        self.stacks[self.tab].last_mut()
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: false });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), error: true });
    }

    pub fn pending_keys(&self) -> &[Key] {
        &self.pending
    }

    fn request(&mut self, cmd: &'static str, args: Value, expect: Expect) {
        self.outbox.push((cmd, args, expect));
    }

    fn send(&mut self, cmd: &'static str, args: Value) {
        self.request(cmd, args, Expect::Status);
    }

    // ---- daemon -> app ----------------------------------------------------

    pub fn on_connected(&mut self) {
        self.connected = true;
        self.message = None;
        self.refresh_queue();
        if self.tab == PLAYLISTS {
            self.load_playlists();
        }
    }

    pub fn on_disconnected(&mut self) {
        self.connected = false;
        self.expecting.clear();
        for stack in &mut self.stacks {
            if let Some(v) = stack.last_mut() {
                v.loading = false;
            }
        }
        self.error("disconnected from soned, retrying…");
    }

    /// Remember what the reply to request `id` is.
    pub fn sent(&mut self, id: u64, expect: Expect) {
        self.expecting.insert(id, expect);
    }

    pub fn on_reply(&mut self, id: u64, result: Result<Value, String>) {
        let expect = self.expecting.remove(&id).unwrap_or(Expect::Status);
        let v = match result {
            Ok(v) => v,
            Err(e) => {
                self.stop_loading(&expect);
                self.error(e);
                return;
            }
        };
        match expect {
            Expect::Status => {
                self.apply_status(&v);
                self.refresh_queue();
            }
            Expect::Queue => self.apply_queue(&v),
            Expect::Playlists => {
                let rows = v["playlists"].as_array().into_iter().flatten().map(playlist_row).collect();
                self.fill(PLAYLISTS, |view| view.title.is_empty(), rows);
            }
            Expect::Tracks { tab, source } => {
                self.fill(tab, |view| view.source.as_ref() == Some(&source), track_rows(&v["tracks"]));
            }
            Expect::Artist { tab, source } => {
                self.fill(tab, |view| view.source.as_ref() == Some(&source), artist_rows(&v));
            }
            Expect::Search { query } => {
                let title = format!("/{query}");
                self.fill(SEARCH, |view| view.title == title, search_rows(&v));
            }
            Expect::LoginStart => {
                self.open_url = v["url"].as_str().map(str::to_string);
                self.mode = Mode::Prompt(PromptKind::Login, String::new());
            }
            Expect::Settings => {
                if let Some(page) = &mut self.settings {
                    page.values = v;
                }
            }
            Expect::LoginFinish => {
                self.apply_status(&v);
                self.info("logged in");
                if self.tab == PLAYLISTS {
                    self.load_playlists();
                }
            }
        }
    }

    /// Put `rows` into the top view of `tab`, if that view is the one waiting.
    fn fill(&mut self, tab: usize, is_target: impl Fn(&View) -> bool, rows: Vec<Row>) {
        if let Some(view) = self.stacks[tab].last_mut() {
            if view.loading && is_target(view) {
                view.rows = rows;
                view.loading = false;
                view.first_selectable();
            }
        }
    }

    fn stop_loading(&mut self, expect: &Expect) {
        let tab = match expect {
            Expect::Playlists => PLAYLISTS,
            Expect::Tracks { tab, .. } | Expect::Artist { tab, .. } => *tab,
            Expect::Search { .. } => SEARCH,
            _ => return,
        };
        if let Some(v) = self.stacks[tab].last_mut() {
            v.loading = false;
        }
    }

    pub fn on_event(&mut self, name: &str, data: &Value) {
        match name {
            "state" => {
                self.apply_status(data);
                self.refresh_queue();
            }
            "position" | "seeked" => self.set_position(data["positionSeconds"].as_f64()),
            "audio-error" => self.error(format!("audio error: {data}")),
            _ => {}
        }
    }

    fn apply_status(&mut self, v: &Value) {
        if v.get("state").is_none() {
            return;
        }
        let was_logged_in = self.status.logged_in || self.status.state.is_empty();
        self.status = Status {
            state: str_of(&v["state"]),
            track: Track::from_json(&v["track"]),
            volume: v["volume"].as_f64().unwrap_or(0.0),
            repeat: v["repeat"].as_str().unwrap_or("off").to_string(),
            shuffle: v["shuffle"].as_bool().unwrap_or(false),
            logged_in: v["loggedIn"].as_bool().unwrap_or(true),
        };
        if was_logged_in && !self.status.logged_in {
            let login = self.config.keymap.describe(Action::CommandLine).unwrap_or_default();
            self.error(format!("not logged in to Tidal: {login}login"));
        }
        self.set_position(v["positionSeconds"].as_f64());
    }

    fn apply_queue(&mut self, v: &Value) {
        self.queue_current = Track::from_json(&v["current"]);
        self.upcoming = v["upcoming"]
            .as_array()
            .map(|a| a.iter().filter_map(Track::from_json).collect())
            .unwrap_or_default();
        self.queue_selected = self.queue_selected.min(self.queue_rows().saturating_sub(1));
    }

    fn set_position(&mut self, secs: Option<f64>) {
        if let Some(s) = secs {
            self.position = s;
            self.position_at = Instant::now();
        }
    }

    fn refresh_queue(&mut self) {
        self.request("queue", json!({ "limit": 1000 }), Expect::Queue);
    }

    fn load_playlists(&mut self) {
        self.stacks[PLAYLISTS] = vec![View::loading("")];
        self.request("playlists", Value::Null, Expect::Playlists);
    }

    fn open_source(&mut self, kind: &str, id: String, title: String) {
        let source = (kind.to_string(), id.clone());
        let mut view = View::loading(title);
        view.source = Some(source.clone());
        self.stacks[self.tab].push(view);
        let tab = self.tab;
        if kind == "artist" {
            self.request("artist", json!({ "id": id }), Expect::Artist { tab, source });
        } else {
            self.request("source_tracks", json!({ "source_type": kind, "id": id }), Expect::Tracks { tab, source });
        }
    }

    fn search(&mut self, query: &str) {
        self.settings = None;
        self.tab = SEARCH;
        self.last_query = Some(query.to_string());
        self.stacks[SEARCH] = vec![View::loading(format!("/{query}"))];
        self.request(
            "search",
            json!({ "query": query, "limit": 20 }),
            Expect::Search { query: query.to_string() },
        );
    }

    // ---- keys -> app ------------------------------------------------------

    pub fn on_key(&mut self, key: Key) {
        if let Mode::Prompt(kind, line) = &mut self.mode {
            let kind = *kind;
            match key {
                Key::Esc => self.mode = Mode::Normal,
                Key::Enter => {
                    let line = std::mem::take(line);
                    self.mode = Mode::Normal;
                    match kind {
                        PromptKind::Command => self.run_command(&line),
                        PromptKind::Search if line.trim().is_empty() => {}
                        PromptKind::Search => self.search(line.trim()),
                        PromptKind::Login if line.trim().is_empty() => {}
                        PromptKind::Login => {
                            self.info("logging in…");
                            self.request(
                                "login_finish",
                                json!({ "redirect_url": line.trim() }),
                                Expect::LoginFinish,
                            );
                        }
                    }
                }
                Key::Backspace => {
                    if line.pop().is_none() {
                        self.mode = Mode::Normal;
                    }
                }
                Key::Char(c) => line.push(c),
                _ => {}
            }
            return;
        }
        // Like nvim, the next keystroke clears the message line.
        self.message = None;
        if let Match::Action(action) = self.config.keymap.feed(&mut self.pending, key) {
            self.do_action(action);
        }
    }

    fn do_action(&mut self, action: Action) {
        if self.settings.is_some() && self.settings_action(action) {
            return;
        }
        let half = (self.list_height / 2).max(1) as isize;
        match action {
            Action::Down => self.move_by(1),
            Action::Up => self.move_by(-1),
            Action::Top => self.move_by(isize::MIN / 2),
            Action::Bottom => self.move_by(isize::MAX / 2),
            Action::HalfPageDown => self.move_by(half),
            Action::HalfPageUp => self.move_by(-half),
            Action::NextTab => self.switch_tab((self.tab + 1) % TABS.len()),
            Action::PrevTab => self.switch_tab((self.tab + TABS.len() - 1) % TABS.len()),
            Action::CommandLine => self.mode = Mode::Prompt(PromptKind::Command, String::new()),
            Action::Search => self.mode = Mode::Prompt(PromptKind::Search, String::new()),
            Action::PlaySelected => self.activate(),
            Action::AddToQueue => self.enqueue_selected("queue"),
            Action::PlayNext => self.enqueue_selected("play_next"),
            Action::RemoveSelected => self.remove_selected(),
            Action::Back => {
                if self.stacks[self.tab].len() > 1 {
                    self.stacks[self.tab].pop();
                }
            }
            Action::Toggle => self.send("toggle", Value::Null),
            Action::NextTrack => self.send("next", Value::Null),
            Action::PrevTrack => self.send("previous", Value::Null),
            Action::SeekBack => self.seek_by(-5.0),
            Action::SeekForward => self.seek_by(5.0),
            Action::VolumeDown => self.volume_by(-0.05),
            Action::VolumeUp => self.volume_by(0.05),
            Action::Quit => self.quit = true,
        }
    }

    fn switch_tab(&mut self, tab: usize) {
        self.settings = None;
        self.tab = tab;
        if tab == PLAYLISTS && self.stacks[PLAYLISTS].is_empty() {
            self.load_playlists();
        }
    }

    /// Move the selection by `delta` rows, skipping headers, clamped to the list.
    fn move_by(&mut self, delta: isize) {
        if self.tab == QUEUE {
            let last = self.queue_rows().saturating_sub(1) as isize;
            self.queue_selected = (self.queue_selected as isize + delta).clamp(0, last.max(0)) as usize;
            return;
        }
        let Some(view) = self.view_mut() else { return };
        let selectable: Vec<usize> = (0..view.rows.len()).filter(|&i| view.rows[i].selectable()).collect();
        if selectable.is_empty() {
            return;
        }
        let at = selectable.iter().position(|&i| i >= view.selected).unwrap_or(0) as isize;
        let to = (at + delta).clamp(0, selectable.len() as isize - 1) as usize;
        view.selected = selectable[to];
    }

    fn selected_row(&self) -> Option<&Row> {
        self.view().and_then(|v| v.rows.get(v.selected))
    }

    /// `<CR>`: play a track (the whole source from there, when there is one),
    /// or open a playlist/album/artist.
    fn activate(&mut self) {
        if self.tab == QUEUE {
            return self.play_queue_selected();
        }
        let Some(view) = self.view() else { return };
        let Some(row) = view.rows.get(view.selected).cloned() else { return };
        match row {
            Row::Track(t) => match view.source.clone() {
                Some((kind, id)) => {
                    let start = view.rows[..view.selected]
                        .iter()
                        .filter(|r| matches!(r, Row::Track(_)))
                        .count();
                    self.send("play_source", json!({ "source_type": kind, "id": id, "start": start }));
                }
                None => self.send("play_tracks", json!({ "track_ids": [t.id], "action": "play_now" })),
            },
            Row::Playlist { uuid, title, .. } => self.open_source("playlist", uuid, title),
            Row::Album { id, title, artist, .. } => {
                self.open_source("album", id.to_string(), format!("{artist} – {title}"))
            }
            Row::Artist { id, name } => self.open_source("artist", id.to_string(), name),
            Row::Header(_) => {}
        }
    }

    fn enqueue_selected(&mut self, action: &str) {
        match self.selected_row() {
            Some(Row::Track(t)) => {
                let (id, title) = (t.id, t.title.clone());
                self.send("play_tracks", json!({ "track_ids": [id], "action": action }));
                self.info(if action == "queue" { format!("queued: {title}") } else { format!("next: {title}") });
            }
            Some(_) => self.error("open it and pick tracks to queue"),
            None => {}
        }
    }

    fn play_queue_selected(&mut self) {
        match (self.queue_selected, self.queue_current.is_some()) {
            (0, true) => self.send("seek", json!({ "position_seconds": 0 })),
            (row, has_current) => {
                let index = row - has_current as usize;
                if index < self.upcoming.len() {
                    self.send("jump", json!({ "index": index }));
                    self.queue_selected = has_current as usize;
                }
            }
        }
    }

    fn remove_selected(&mut self) {
        if self.tab != QUEUE {
            return;
        }
        let offset = self.queue_current.is_some() as usize;
        if self.queue_selected < offset {
            self.error("the playing track can't be removed");
            return;
        }
        if let Some(t) = self.upcoming.get(self.queue_selected - offset) {
            let id = t.id;
            self.send("remove", json!({ "track_id": id }));
        }
    }

    fn seek_by(&mut self, delta: f64) {
        let target = (self.position() + delta).max(0.0);
        // Move the local position now, so repeated presses add up before the
        // daemon's reply arrives.
        self.set_position(Some(target));
        self.send("seek", json!({ "position_seconds": target }));
    }

    fn volume_by(&mut self, delta: f64) {
        let level = (self.status.volume + delta).clamp(0.0, 1.0);
        self.status.volume = level;
        self.send("volume", json!({ "level": level }));
    }

    // ---- settings page ----------------------------------------------------

    /// Keys on the settings page. Returns false for the actions it leaves to
    /// the normal handling (playback, volume, tabs, command line, quit).
    fn settings_action(&mut self, action: Action) -> bool {
        let Some(page) = self.settings.as_mut() else { return false };
        let confirming = std::mem::take(&mut page.confirm_logout);
        let selectable: Vec<usize> =
            (0..SETTING_ROWS.len()).filter(|&i| !matches!(SETTING_ROWS[i], Setting::Header(_))).collect();
        let at = selectable.iter().position(|&i| i == page.selected).unwrap_or(0) as isize;
        let move_to = |delta: isize| selectable[(at + delta).clamp(0, selectable.len() as isize - 1) as usize];
        match action {
            Action::Down | Action::HalfPageDown => page.selected = move_to(1),
            Action::Up | Action::HalfPageUp => page.selected = move_to(-1),
            Action::Top => page.selected = move_to(isize::MIN / 2),
            Action::Bottom => page.selected = move_to(isize::MAX / 2),
            Action::Back => self.settings = None,
            Action::PlaySelected => {
                let row = SETTING_ROWS[page.selected];
                self.change_setting(row, confirming);
            }
            _ => return false,
        }
        true
    }

    fn change_setting(&mut self, row: Setting, confirming: bool) {
        let Some(page) = self.settings.as_mut() else { return };
        let v = page.values.clone();
        if v.is_null() && row != Setting::Account {
            return;
        }
        let flip = |key: &str| !v[key].as_bool().unwrap_or(false);
        let (name, value) = match row {
            Setting::Header(_) => return,
            Setting::MaxQuality => {
                let current = v["maxQuality"].as_str().unwrap_or("");
                let i = QUALITIES.iter().position(|q| *q == current).map_or(0, |i| i + 1);
                ("max_quality", json!(QUALITIES[i % QUALITIES.len()]))
            }
            Setting::Gapless => ("gapless", json!(flip("gapless"))),
            Setting::ExclusiveMode => ("exclusive_mode", json!(flip("exclusiveMode"))),
            Setting::BitPerfect => ("bit_perfect", json!(flip("bitPerfect"))),
            Setting::Device => {
                let ids: Vec<&str> = v["devices"].as_array().into_iter().flatten().filter_map(|d| d["id"].as_str()).collect();
                if ids.is_empty() {
                    return self.error("no output devices found");
                }
                let current = v["exclusiveDevice"].as_str();
                let i = ids.iter().position(|d| Some(*d) == current).map_or(0, |i| i + 1);
                ("exclusive_device", json!(ids[i % ids.len()]))
            }
            Setting::Account if !self.status.logged_in => {
                return self.request("login_start", Value::Null, Expect::LoginStart);
            }
            Setting::Account if confirming => {
                self.send("logout", Value::Null);
                return self.info("logged out");
            }
            Setting::Account => {
                page.confirm_logout = true;
                let key = self.config.keymap.describe(Action::PlaySelected).unwrap_or_default();
                return self.info(format!("log out of Tidal? {key} again to confirm"));
            }
        };
        self.request("set_setting", json!({ "name": name, "value": value }), Expect::Settings);
    }

    // ---- command line -----------------------------------------------------

    fn run_command(&mut self, line: &str) {
        match parse_command(line) {
            Ok(Command::Quit) => self.quit = true,
            Ok(Command::Login) => self.request("login_start", Value::Null, Expect::LoginStart),
            Ok(Command::Settings) => {
                self.settings = Some(SettingsPage { selected: 1, ..Default::default() });
                self.request("settings", Value::Null, Expect::Settings);
            }
            Ok(Command::Refresh) => match self.tab {
                QUEUE => self.refresh_queue(),
                PLAYLISTS => self.load_playlists(),
                _ => {
                    if let Some(q) = self.last_query.clone() {
                        self.search(&q);
                    }
                }
            },
            Ok(Command::Send(cmd, args)) => self.send(cmd, args),
            Err(e) if e.is_empty() => {}
            Err(e) => self.error(e),
        }
    }
}

fn playlist_row(p: &Value) -> Row {
    Row::Playlist {
        uuid: str_of(&p["uuid"]),
        title: str_of(&p["title"]),
        tracks: p["tracks"].as_u64().unwrap_or(0),
    }
}

fn album_row(a: &Value) -> Option<Row> {
    Some(Row::Album {
        id: a["id"].as_u64()?,
        title: str_of(&a["title"]),
        artist: str_of(&a["artist"]),
        year: a["year"].as_u64().map(|y| y as u16),
        kind: a["type"].as_str().map(str::to_string),
    })
}

fn album_year(row: &Row) -> u16 {
    match row {
        // unknown years sort last
        Row::Album { year, .. } => year.unwrap_or(u16::MAX),
        _ => u16::MAX,
    }
}

/// Albums grouped by artist, the artists in the order Tidal ranked them,
/// each artist's albums by year; an empty header between artists.
fn grouped_albums(albums: Vec<Row>) -> Vec<Row> {
    let mut groups: Vec<(String, Vec<Row>)> = Vec::new();
    for row in albums {
        let Row::Album { artist, .. } = &row else { continue };
        match groups.iter_mut().find(|(a, _)| a == artist) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((artist.clone(), vec![row])),
        }
    }
    let mut out = Vec::new();
    for (i, (_, mut rows)) in groups.into_iter().enumerate() {
        if i > 0 {
            out.push(Row::Header(String::new()));
        }
        rows.sort_by_key(album_year);
        out.extend(rows);
    }
    out
}

fn sections(sections: Vec<(&str, Vec<Row>)>) -> Vec<Row> {
    let mut rows = Vec::new();
    for (name, items) in sections {
        if !items.is_empty() {
            rows.push(Row::Header(name.to_string()));
            rows.extend(items);
        }
    }
    rows
}

fn track_rows(v: &Value) -> Vec<Row> {
    v.as_array().into_iter().flatten().filter_map(Track::from_json).map(Row::Track).collect()
}

fn search_rows(v: &Value) -> Vec<Row> {
    let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
    sections(vec![
        ("Albums", grouped_albums(list("albums").iter().filter_map(album_row).collect())),
        ("Tracks", track_rows(&v["tracks"])),
        (
            "Artists",
            list("artists")
                .iter()
                .filter_map(|a| Some(Row::Artist { id: a["id"].as_u64()?, name: str_of(&a["name"]) }))
                .collect(),
        ),
        ("Playlists", list("playlists").iter().map(playlist_row).collect()),
    ])
}

/// An artist's page: albums by year, then the top tracks.
fn artist_rows(v: &Value) -> Vec<Row> {
    let mut albums: Vec<Row> = v["albums"].as_array().into_iter().flatten().filter_map(album_row).collect();
    albums.sort_by_key(album_year);
    sections(vec![("Albums", albums), ("Top tracks", track_rows(&v["tracks"]))])
}

enum Command {
    Quit,
    Login,
    Settings,
    Refresh,
    Send(&'static str, Value),
}

fn parse_command(line: &str) -> Result<Command, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let source = |rest: &[&str]| match rest {
        [kind @ ("playlist" | "album" | "artist" | "mix"), id] => {
            Ok(json!({ "source_type": kind, "id": id }))
        }
        _ => Err("usage: play playlist|album|artist|mix <id>".to_string()),
    };
    let send = |cmd: &'static str, args: Value| -> Result<Command, String> { Ok(Command::Send(cmd, args)) };
    match words.as_slice() {
        ["q"] | ["quit"] => Ok(Command::Quit),
        ["r"] | ["refresh"] => Ok(Command::Refresh),
        ["login"] => Ok(Command::Login),
        ["settings"] | ["set"] => Ok(Command::Settings),
        ["play", rest @ ..] => send("play_source", source(rest)?),
        ["shuffle-play", rest @ ..] => send("shuffle_source", source(rest)?),
        ["repeat", m @ ("off" | "all" | "one")] => send("repeat", json!({ "mode": m })),
        ["shuffle"] => send("shuffle", json!({})),
        ["shuffle", "on"] => send("shuffle", json!({ "on": true })),
        ["shuffle", "off"] => send("shuffle", json!({ "on": false })),
        ["vol", v] => {
            let pct: f64 = v.parse().map_err(|_| "usage: vol <0-100>")?;
            send("volume", json!({ "level": (pct / 100.0).clamp(0.0, 1.0) }))
        }
        ["seek", s] => {
            let secs: f64 = s.parse().map_err(|_| "usage: seek <seconds>")?;
            send("seek", json!({ "position_seconds": secs }))
        }
        ["next"] => send("next", Value::Null),
        ["prev"] => send("previous", Value::Null),
        [] => Err(String::new()),
        _ => Err(format!("unknown command: {line}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_json(i: u64) -> Value {
        json!({ "id": i, "title": format!("t{i}"), "artist": "a", "durationSeconds": 60 })
    }

    fn reply(app: &mut App, expect: Expect, v: Value) {
        let id = 1000 + app.expecting.len() as u64;
        app.sent(id, expect);
        app.on_reply(id, Ok(v));
    }

    fn app_with_queue(n: u64) -> App {
        let mut app = App::new(Config::default());
        let upcoming: Vec<Value> = (2..=n).map(track_json).collect();
        reply(&mut app, Expect::Queue, json!({ "current": track_json(1), "upcoming": upcoming }));
        app.outbox.clear();
        app
    }

    fn press(app: &mut App, keys: &str) {
        for k in super::super::keys::parse_seq(keys).unwrap() {
            app.on_key(k);
        }
    }

    /// Answer the outbox's last request with `v`, as the event loop would.
    fn answer_last(app: &mut App, v: Value) {
        let (_, _, expect) = app.outbox.pop().unwrap();
        reply(app, expect, v);
    }

    fn sent_cmds(app: &App) -> Vec<(&'static str, Value)> {
        app.outbox.iter().map(|(c, a, _)| (*c, a.clone())).collect()
    }

    #[test]
    fn moves_with_workman_keys() {
        let mut app = app_with_queue(5);
        press(&mut app, "nn");
        assert_eq!(app.queue_selected, 2);
        press(&mut app, "e");
        assert_eq!(app.queue_selected, 1);
        press(&mut app, "G");
        assert_eq!(app.queue_selected, 4);
        press(&mut app, "gg");
        assert_eq!(app.queue_selected, 0);
    }

    #[test]
    fn enter_jumps_to_upcoming_entry() {
        let mut app = app_with_queue(5);
        press(&mut app, "nnn<CR>");
        assert_eq!(sent_cmds(&app), vec![("jump", json!({ "index": 2 }))]);
    }

    #[test]
    fn dd_removes_upcoming_but_not_current() {
        let mut app = app_with_queue(3);
        press(&mut app, "dd");
        assert!(app.outbox.is_empty());
        assert!(app.message.as_ref().unwrap().error);
        press(&mut app, "ndd");
        assert_eq!(sent_cmds(&app), vec![("remove", json!({ "track_id": 2 }))]);
    }

    #[test]
    fn command_line_sends_play_and_quits() {
        let mut app = app_with_queue(1);
        press(&mut app, ";play playlist abc<CR>");
        assert_eq!(sent_cmds(&app), vec![("play_source", json!({ "source_type": "playlist", "id": "abc" }))]);
        press(&mut app, ";bogus<CR>");
        assert!(app.message.as_ref().unwrap().error);
        press(&mut app, ";q<CR>");
        assert!(app.quit);
    }

    #[test]
    fn repeated_seeks_add_up() {
        let mut app = app_with_queue(1);
        app.on_event("seeked", &json!({ "positionSeconds": 30.0 }));
        press(&mut app, "oo");
        let target = |v: &Value| v["position_seconds"].as_f64().unwrap().round();
        assert_eq!(target(&app.outbox[0].1), 35.0);
        assert_eq!(target(&app.outbox[1].1), 40.0);
    }

    #[test]
    fn next_key_clears_message() {
        let mut app = app_with_queue(1);
        app.error("boom");
        press(&mut app, "g");
        assert!(app.message.is_none());
        assert_eq!(app.pending_keys().len(), 1);
    }

    #[test]
    fn playlists_tab_loads_opens_and_plays_from_track() {
        let mut app = app_with_queue(1);
        press(&mut app, ",o");
        assert_eq!(app.tab, PLAYLISTS);
        assert_eq!(app.outbox.last().unwrap().0, "playlists");
        answer_last(
            &mut app,
            json!({ "playlists": [{ "uuid": "u1", "title": "One", "tracks": 3 }, { "uuid": "u2", "title": "Two", "tracks": 5 }] }),
        );
        press(&mut app, "n<CR>");
        assert_eq!(
            sent_cmds(&app).last().unwrap(),
            &("source_tracks", json!({ "source_type": "playlist", "id": "u2" }))
        );
        assert!(app.view().unwrap().loading);
        answer_last(&mut app, json!({ "tracks": [track_json(10), track_json(11), track_json(12)] }));
        assert_eq!(app.view().unwrap().title, "Two");
        app.outbox.clear();
        press(&mut app, "nn<CR>");
        assert_eq!(
            sent_cmds(&app),
            vec![("play_source", json!({ "source_type": "playlist", "id": "u2", "start": 2 }))]
        );
        press(&mut app, "<BS>");
        assert_eq!(app.stacks[PLAYLISTS].len(), 1);
        // the root view is never popped
        press(&mut app, "<BS>");
        assert_eq!(app.stacks[PLAYLISTS].len(), 1);
    }

    #[test]
    fn a_and_shift_a_queue_tracks() {
        let mut app = app_with_queue(1);
        press(&mut app, "/zu<CR>");
        answer_last(&mut app, json!({ "tracks": [track_json(7)], "albums": [], "artists": [], "playlists": [] }));
        app.outbox.clear();
        press(&mut app, "aA");
        assert_eq!(
            sent_cmds(&app),
            vec![
                ("play_tracks", json!({ "track_ids": [7], "action": "queue" })),
                ("play_tracks", json!({ "track_ids": [7], "action": "play_next" })),
            ]
        );
    }

    #[test]
    fn search_shows_sections_and_skips_headers() {
        let mut app = app_with_queue(1);
        press(&mut app, "/zu<CR>");
        assert_eq!(app.tab, SEARCH);
        assert_eq!(sent_cmds(&app).last().unwrap(), &("search", json!({ "query": "zu", "limit": 20 })));
        answer_last(
            &mut app,
            json!({
                "tracks": [track_json(7)],
                "albums": [{ "id": 5, "title": "Carboniferous", "artist": "Zu" }],
                "artists": [{ "id": 9, "name": "Zu" }],
                "playlists": []
            }),
        );
        let view = app.view().unwrap();
        assert_eq!(view.rows[0], Row::Header("Albums".into()));
        assert_eq!(view.selected, 1, "selection starts on the first album, not the header");
        // n moves from the album over the "Tracks" header to the track
        press(&mut app, "n");
        let view = app.view().unwrap();
        assert!(matches!(view.rows[view.selected], Row::Track(_)));
        app.outbox.clear();
        press(&mut app, "e<CR>");
        assert_eq!(sent_cmds(&app), vec![("source_tracks", json!({ "source_type": "album", "id": "5" }))]);
    }

    #[test]
    fn search_track_without_source_plays_just_that_track() {
        let mut app = app_with_queue(1);
        press(&mut app, "/x<CR>");
        answer_last(&mut app, json!({ "tracks": [track_json(7)] }));
        app.outbox.clear();
        press(&mut app, "<CR>");
        assert_eq!(sent_cmds(&app), vec![("play_tracks", json!({ "track_ids": [7], "action": "play_now" }))]);
    }

    #[test]
    fn stale_reply_does_not_overwrite_a_newer_view() {
        let mut app = app_with_queue(1);
        press(&mut app, "/old<CR>");
        let (_, _, old) = app.outbox.pop().unwrap();
        press(&mut app, "/new<CR>");
        reply(&mut app, old, json!({ "tracks": [track_json(1)] }));
        assert!(app.view().unwrap().loading, "the /old reply must not fill the /new view");
    }

    #[test]
    fn state_event_updates_status_and_requests_queue() {
        let mut app = app_with_queue(1);
        app.on_event(
            "state",
            &json!({ "state": "paused", "track": null, "positionSeconds": 12.0, "volume": 0.5, "repeat": "all", "shuffle": true }),
        );
        assert_eq!(app.status.state, "paused");
        assert_eq!(app.position(), 12.0);
        assert_eq!(app.outbox[0].0, "queue");
    }

    fn album_json(id: u64, artist: &str, year: Option<u16>) -> Value {
        json!({ "id": id, "title": format!("al{id}"), "artist": artist, "year": year, "type": "ALBUM" })
    }

    fn album_ids(rows: &[Row]) -> Vec<Option<u64>> {
        rows.iter()
            .map(|r| match r {
                Row::Album { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn search_groups_albums_by_artist_then_year() {
        let mut app = app_with_queue(1);
        press(&mut app, "/zu<CR>");
        answer_last(
            &mut app,
            json!({
                "albums": [
                    album_json(1, "Zu", Some(2009)),
                    album_json(2, "Ruins", Some(1990)),
                    album_json(3, "Zu", Some(2002)),
                    album_json(4, "Zu", None),
                ],
                "tracks": [track_json(7)],
            }),
        );
        let view = app.view().unwrap();
        // header, Zu by year (unknown last), blank, Ruins, then the tracks
        assert_eq!(
            album_ids(&view.rows),
            vec![None, Some(3), Some(1), Some(4), None, Some(2), None, None]
        );
        assert_eq!(view.rows[4], Row::Header(String::new()));
        assert_eq!(view.rows[6], Row::Header("Tracks".into()));
        // n from the last Zu album skips the blank separator
        press(&mut app, "nnn");
        assert_eq!(app.view().unwrap().selected, 5);
    }

    #[test]
    fn artist_page_lists_albums_then_plays_top_tracks_from_selection() {
        let mut app = app_with_queue(1);
        press(&mut app, "/zu<CR>");
        answer_last(&mut app, json!({ "artists": [{ "id": 9, "name": "Zu" }] }));
        app.outbox.clear();
        press(&mut app, "<CR>");
        assert_eq!(sent_cmds(&app), vec![("artist", json!({ "id": "9" }))]);
        answer_last(
            &mut app,
            json!({
                "albums": [album_json(2, "Zu", Some(2009)), album_json(1, "Zu", Some(2002))],
                "tracks": [track_json(10), track_json(11)],
            }),
        );
        let view = app.view().unwrap();
        assert_eq!(view.title, "Zu");
        assert_eq!(album_ids(&view.rows), vec![None, Some(1), Some(2), None, None, None]);
        assert_eq!(view.rows[3], Row::Header("Top tracks".into()));
        // the second top track: start counts tracks only, not the albums above
        press(&mut app, "G<CR>");
        assert_eq!(
            sent_cmds(&app),
            vec![("play_source", json!({ "source_type": "artist", "id": "9", "start": 1 }))]
        );
    }

    #[test]
    fn login_opens_the_page_then_sends_the_pasted_address() {
        let mut app = app_with_queue(1);
        press(&mut app, ";login<CR>");
        assert_eq!(sent_cmds(&app), vec![("login_start", Value::Null)]);
        answer_last(&mut app, json!({ "url": "https://login.tidal.com/authorize?x=1" }));
        assert_eq!(app.open_url.as_deref(), Some("https://login.tidal.com/authorize?x=1"));
        assert!(matches!(app.mode, Mode::Prompt(PromptKind::Login, _)));
        // pasted text arrives as plain keys; letters typed here are not commands
        press(&mut app, "https://tidal.com/a?code=nq<CR>");
        assert_eq!(
            sent_cmds(&app),
            vec![("login_finish", json!({ "redirect_url": "https://tidal.com/a?code=nq" }))]
        );
        answer_last(&mut app, json!({ "state": "stopped", "loggedIn": true }));
        assert!(app.status.logged_in);
        assert_eq!(app.message.as_ref().unwrap().text, "logged in");
    }

    #[test]
    fn esc_cancels_the_login_prompt() {
        let mut app = app_with_queue(1);
        press(&mut app, ";login<CR>");
        answer_last(&mut app, json!({ "url": "u" }));
        press(&mut app, "abc<Esc>");
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.outbox.is_empty());
    }

    #[test]
    fn logged_out_status_says_how_to_log_in_once() {
        let mut app = App::new(Config::default());
        let status = json!({ "state": "stopped", "loggedIn": false });
        app.on_event("state", &status);
        assert!(!app.status.logged_in);
        assert_eq!(app.message.as_ref().unwrap().text, "not logged in to Tidal: ;login");
        app.message = None;
        app.on_event("state", &status);
        assert!(app.message.is_none(), "not repeated on every state event");
    }

    fn app_with_settings() -> App {
        let mut app = app_with_queue(1);
        app.on_event("state", &json!({ "state": "stopped", "loggedIn": true }));
        app.outbox.clear();
        press(&mut app, ";settings<CR>");
        assert_eq!(sent_cmds(&app), vec![("settings", Value::Null)]);
        answer_last(
            &mut app,
            json!({
                "maxQuality": "LOSSLESS", "gapless": true, "exclusiveMode": false, "bitPerfect": false,
                "exclusiveDevice": "hw:1", "devices": [{ "id": "hw:1", "name": "a" }, { "id": "hw:2", "name": "b" }],
                "loggedIn": true
            }),
        );
        app
    }

    #[test]
    fn settings_page_toggles_and_cycles() {
        let mut app = app_with_settings();
        assert_eq!(app.settings.as_ref().unwrap().selected, 1, "starts on the first setting, not the header");
        press(&mut app, "<CR>n<CR>nnn<CR>");
        assert_eq!(
            sent_cmds(&app),
            vec![
                ("set_setting", json!({ "name": "max_quality", "value": "HIGH" })),
                ("set_setting", json!({ "name": "gapless", "value": false })),
                ("set_setting", json!({ "name": "exclusive_device", "value": "hw:2" })),
            ]
        );
        // n past the last device skips the "Account" header
        press(&mut app, "n");
        assert_eq!(SETTING_ROWS[app.settings.as_ref().unwrap().selected], Setting::Account);
    }

    #[test]
    fn logout_needs_a_second_enter_and_other_keys_cancel() {
        let mut app = app_with_settings();
        press(&mut app, "G<CR>");
        assert!(app.outbox.is_empty());
        press(&mut app, "e<CR>");
        app.outbox.clear();
        press(&mut app, "n<CR><CR>");
        assert_eq!(sent_cmds(&app).last().unwrap(), &("logout", Value::Null));
    }

    #[test]
    fn back_or_tab_switch_closes_settings() {
        let mut app = app_with_settings();
        press(&mut app, "<BS>");
        assert!(app.settings.is_none());
        let mut app = app_with_settings();
        press(&mut app, ",o");
        assert!(app.settings.is_none());
        assert_eq!(app.tab, PLAYLISTS);
    }
}
