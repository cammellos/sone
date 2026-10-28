//! Key bindings: actions, key sequences in nvim notation, and the matcher
//! that turns keystrokes into actions.
//!
//! Notation: plain characters (`n`, `G`, `,o`, `dd`) and `<...>` names:
//! `<C-x>` (Ctrl), `<CR>`/`<Enter>`, `<Esc>`, `<Space>`, `<Tab>`, `<S-Tab>`, `<BS>`,
//! `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<lt>` (a literal `<`). A `<` that
//! doesn't start one of those names is a literal `<`.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
}

macro_rules! actions {
    ($($variant:ident = $name:literal [$($default:literal),*]),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Action { $($variant),* }

        impl Action {
            pub const ALL: &'static [Action] = &[$(Action::$variant),*];

            pub fn name(self) -> &'static str {
                match self { $(Action::$variant => $name),* }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Action::$variant),)* _ => None }
            }

            fn defaults(self) -> &'static [&'static str] {
                match self { $(Action::$variant => &[$($default),*]),* }
            }
        }
    };
}

// Defaults follow the user's nvim Workman bindings: n/e down/up, y/o
// left/right, `;` for the command line, `,o`/`,y` for tabs. `<Tab>`/`<S-Tab>`
// are what sway's Meta+t/h send to the player window. `j`/`J` stay free for
// in-list find (next/previous match).
actions! {
    Down = "down" ["n", "<Down>"],
    Up = "up" ["e", "<Up>"],
    SeekBack = "seek_back" ["y", "<Left>"],
    SeekForward = "seek_forward" ["o", "<Right>"],
    Top = "top" ["gg"],
    Bottom = "bottom" ["G"],
    HalfPageDown = "half_page_down" ["<C-d>"],
    HalfPageUp = "half_page_up" ["<C-u>"],
    NextTab = "next_tab" [",o", "<Tab>"],
    PrevTab = "prev_tab" [",y", "<S-Tab>"],
    CommandLine = "command_line" [";"],
    PlaySelected = "play_selected" ["<CR>"],
    AddToQueue = "add_to_queue" ["a"],
    PlayNext = "play_next" ["A"],
    RemoveSelected = "remove_selected" ["dd"],
    Back = "back" ["<BS>"],
    Search = "search" ["/"],
    Toggle = "toggle" ["<Space>"],
    NextTrack = "next_track" [">"],
    PrevTrack = "prev_track" ["<"],
    VolumeDown = "volume_down" ["-"],
    VolumeUp = "volume_up" ["+", "="],
    Quit = "quit" ["q"],
}

pub fn parse_seq(s: &str) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>') {
                if let Some(k) = parse_special(&rest[1..end]) {
                    keys.push(k);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        keys.push(Key::Char(c));
        rest = &rest[c.len_utf8()..];
    }
    if keys.is_empty() {
        return Err("empty key sequence".into());
    }
    Ok(keys)
}

fn parse_special(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "cr" | "enter" | "return" => Key::Enter,
        "esc" => Key::Esc,
        "space" => Key::Char(' '),
        "tab" => Key::Tab,
        "s-tab" => Key::BackTab,
        "bs" | "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "lt" => Key::Char('<'),
        _ => {
            let c = lower.strip_prefix("c-")?;
            let mut chars = c.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Ctrl(ch)
        }
    })
}

pub enum Match {
    Action(Action),
    /// The keys so far start a longer binding: wait for more.
    Pending,
    None,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<(Vec<Key>, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::with_overrides(&HashMap::new()).0
    }
}

impl Keymap {
    /// The default bindings, with each action named in `overrides` bound to
    /// exactly those sequences instead. Returns the problems found (unknown
    /// actions, bad notation); those entries are skipped.
    pub fn with_overrides(overrides: &HashMap<String, Vec<String>>) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        for name in overrides.keys() {
            if Action::from_name(name).is_none() {
                errors.push(format!("unknown action in [keys]: {name}"));
            }
        }
        let mut bindings = Vec::new();
        for &action in Action::ALL {
            let seqs: Vec<&str> = match overrides.get(action.name()) {
                Some(v) => v.iter().map(String::as_str).collect(),
                None => action.defaults().to_vec(),
            };
            for s in seqs {
                match parse_seq(s) {
                    Ok(keys) => bindings.push((keys, action)),
                    Err(e) => errors.push(format!("[keys] {}: {s:?}: {e}", action.name())),
                }
            }
        }
        (Self { bindings }, errors)
    }

    /// Feed one keystroke. `pending` holds the keys typed so far.
    pub fn feed(&self, pending: &mut Vec<Key>, key: Key) -> Match {
        if key == Key::Esc && !pending.is_empty() {
            pending.clear();
            return Match::None;
        }
        pending.push(key);
        match self.lookup(pending) {
            Match::None if pending.len() > 1 => {
                // Like nvim: an unfinished sequence is dropped and the last
                // key is tried on its own.
                pending.clear();
                self.feed(pending, key)
            }
            Match::None => {
                pending.clear();
                Match::None
            }
            Match::Action(a) => {
                pending.clear();
                Match::Action(a)
            }
            Match::Pending => Match::Pending,
        }
    }

    fn lookup(&self, keys: &[Key]) -> Match {
        let mut exact = None;
        let mut longer = false;
        for (seq, action) in &self.bindings {
            if seq.as_slice() == keys {
                exact = Some(*action);
            } else if seq.starts_with(keys) {
                longer = true;
            }
        }
        match (exact, longer) {
            (_, true) => Match::Pending,
            (Some(a), false) => Match::Action(a),
            (None, false) => Match::None,
        }
    }

    /// The first sequence bound to `action`, for help text.
    pub fn describe(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(seq, _)| seq.iter().map(display_key).collect())
    }
}

fn display_key(k: &Key) -> String {
    match k {
        Key::Char(' ') => "<Space>".into(),
        Key::Char(c) => c.to_string(),
        Key::Ctrl(c) => format!("<C-{c}>"),
        Key::Enter => "<CR>".into(),
        Key::Esc => "<Esc>".into(),
        Key::Tab => "<Tab>".into(),
        Key::BackTab => "<S-Tab>".into(),
        Key::Backspace => "<BS>".into(),
        Key::Up => "<Up>".into(),
        Key::Down => "<Down>".into(),
        Key::Left => "<Left>".into(),
        Key::Right => "<Right>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(map: &Keymap, s: &str) -> Vec<Action> {
        let mut pending = Vec::new();
        parse_seq(s)
            .unwrap()
            .into_iter()
            .filter_map(|k| match map.feed(&mut pending, k) {
                Match::Action(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_nvim_notation() {
        assert_eq!(parse_seq("<C-d>").unwrap(), vec![Key::Ctrl('d')]);
        assert_eq!(parse_seq(",o").unwrap(), vec![Key::Char(','), Key::Char('o')]);
        assert_eq!(parse_seq("gg").unwrap(), vec![Key::Char('g'), Key::Char('g')]);
        assert_eq!(parse_seq("<CR>").unwrap(), vec![Key::Enter]);
        assert_eq!(parse_seq("<Space>").unwrap(), vec![Key::Char(' ')]);
        assert_eq!(parse_seq("<lt>").unwrap(), vec![Key::Char('<')]);
        // a `<` that isn't a known name is literal
        assert_eq!(parse_seq("<").unwrap(), vec![Key::Char('<')]);
        assert_eq!(parse_seq("<x>").unwrap(), vec![Key::Char('<'), Key::Char('x'), Key::Char('>')]);
        assert!(parse_seq("").is_err());
    }

    #[test]
    fn defaults_follow_workman_bindings() {
        let map = Keymap::default();
        assert_eq!(feed_all(&map, "n"), vec![Action::Down]);
        assert_eq!(feed_all(&map, "e"), vec![Action::Up]);
        assert_eq!(feed_all(&map, ",o"), vec![Action::NextTab]);
        assert_eq!(feed_all(&map, "gg"), vec![Action::Top]);
        assert_eq!(feed_all(&map, "dd"), vec![Action::RemoveSelected]);
        assert_eq!(feed_all(&map, "<C-d>"), vec![Action::HalfPageDown]);
        assert_eq!(feed_all(&map, "<"), vec![Action::PrevTrack]);
        assert_eq!(feed_all(&map, "<Tab>"), vec![Action::NextTab]);
        assert_eq!(feed_all(&map, "<S-Tab>"), vec![Action::PrevTab]);
    }

    #[test]
    fn sequence_waits_then_fires() {
        let map = Keymap::default();
        let mut pending = Vec::new();
        assert!(matches!(map.feed(&mut pending, Key::Char('g')), Match::Pending));
        assert!(matches!(map.feed(&mut pending, Key::Char('g')), Match::Action(Action::Top)));
        assert!(pending.is_empty());
    }

    #[test]
    fn broken_sequence_retries_last_key() {
        let map = Keymap::default();
        // `g` then `n`: no `gn` binding, so `n` acts on its own
        assert_eq!(feed_all(&map, "gn"), vec![Action::Down]);
    }

    #[test]
    fn esc_clears_pending() {
        let map = Keymap::default();
        let mut pending = Vec::new();
        map.feed(&mut pending, Key::Char('d'));
        assert!(matches!(map.feed(&mut pending, Key::Esc), Match::None));
        assert!(pending.is_empty());
        assert_eq!(feed_all(&map, "d<Esc>d"), vec![]);
    }

    #[test]
    fn override_replaces_only_that_action() {
        let overrides = HashMap::from([
            ("down".to_string(), vec!["j".to_string()]),
            ("bogus".to_string(), vec!["x".to_string()]),
        ]);
        let (map, errors) = Keymap::with_overrides(&overrides);
        assert_eq!(feed_all(&map, "j"), vec![Action::Down]);
        assert_eq!(feed_all(&map, "n"), vec![]);
        assert_eq!(feed_all(&map, "e"), vec![Action::Up]);
        assert_eq!(errors, vec!["unknown action in [keys]: bogus"]);
    }

    #[test]
    fn describe_uses_notation() {
        let map = Keymap::default();
        assert_eq!(map.describe(Action::HalfPageDown).as_deref(), Some("<C-d>"));
        assert_eq!(map.describe(Action::Toggle).as_deref(), Some("<Space>"));
    }
}
