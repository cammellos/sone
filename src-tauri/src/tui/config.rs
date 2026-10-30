//! `~/.config/sone/tui.toml`. Everything is optional; what's missing keeps
//! its default.
//!
//! ```toml
//! # Click to select, double-click to play/open, wheel to scroll, click a tab
//! # to switch. Off by default: while on, selecting text in the terminal needs
//! # its override modifier (shift in kitty, foot, alacritty).
//! mouse = true
//!
//! [keys]
//! # action = sequence or [sequences], nvim notation; replaces that
//! # action's defaults. Actions: down up seek_back seek_forward top bottom
//! # half_page_down half_page_up next_tab prev_tab command_line search
//! # play_selected add_to_queue play_next remove_selected rename back toggle
//! # next_track prev_track volume_down volume_up quit
//! down = ["n", "<Down>"]
//!
//! [colors]
//! # "#rrggbb". Names and defaults from the system palette (Nord-based):
//! # bg surface panel faint text_dim text_header text text_bright primary
//! # error success warning accent2
//! primary = "#88c0d0"
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

use super::keys::Keymap;

macro_rules! colors {
    ($($field:ident = $hex:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        pub struct Colors { $(pub $field: Color),* }

        impl Default for Colors {
            fn default() -> Self {
                Self { $($field: parse_hex($hex).expect("default colour")),* }
            }
        }

        impl Colors {
            fn set(&mut self, name: &str, color: Color) -> bool {
                match name {
                    $(stringify!($field) => self.$field = color,)*
                    _ => return false,
                }
                true
            }
        }
    };
}

colors! {
    bg = "#2e3440",
    surface = "#3b4252",
    panel = "#434c5e",
    faint = "#4c566a",
    text_dim = "#888e99",
    text_header = "#9aa0aa",
    text = "#aeb4bf",
    text_bright = "#d8dee9",
    primary = "#88c0d0",
    error = "#c07c82",
    success = "#9caa92",
    warning = "#b1a58b",
    accent2 = "#aa92a6",
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub keymap: Keymap,
    pub colors: Colors,
    pub mouse: bool,
}

pub fn default_path() -> Option<PathBuf> {
    crate::engine::default_config_dir().map(|d| d.join("tui.toml"))
}

/// Load the config, or the defaults when the file doesn't exist. Problems are
/// returned alongside; the affected entries keep their defaults.
pub fn load(path: &Path) -> (Config, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (Config::default(), vec![format!("{}: {e}", path.display())]),
    }
}

pub fn parse(text: &str) -> (Config, Vec<String>) {
    let mut errors = Vec::new();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => return (Config::default(), vec![format!("tui.toml: {e}")]),
    };

    let mut overrides: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(keys) = table.get("keys") {
        match keys.as_table() {
            Some(keys) => {
                for (action, value) in keys {
                    let seqs = match value {
                        toml::Value::String(s) => Some(vec![s.clone()]),
                        toml::Value::Array(a) => a
                            .iter()
                            .map(|v| v.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>(),
                        _ => None,
                    };
                    match seqs {
                        Some(seqs) => {
                            overrides.insert(action.clone(), seqs);
                        }
                        None => errors.push(format!(
                            "[keys] {action}: expected a string or a list of strings"
                        )),
                    }
                }
            }
            None => errors.push("[keys] must be a table".into()),
        }
    }
    let (keymap, key_errors) = Keymap::with_overrides(&overrides);
    errors.extend(key_errors);

    let mut colors = Colors::default();
    if let Some(table) = table.get("colors") {
        match table.as_table() {
            Some(table) => {
                for (name, value) in table {
                    let parsed = value.as_str().and_then(parse_hex);
                    match parsed {
                        Some(c) if colors.set(name, c) => {}
                        Some(_) => errors.push(format!("unknown colour in [colors]: {name}")),
                        None => errors.push(format!("[colors] {name}: expected \"#rrggbb\"")),
                    }
                }
            }
            None => errors.push("[colors] must be a table".into()),
        }
    }

    let mouse = match table.get("mouse") {
        None => false,
        Some(toml::Value::Boolean(on)) => *on,
        Some(_) => {
            errors.push("mouse: expected true or false".into());
            false
        }
    };

    (Config { keymap, colors, mouse }, errors)
}

fn parse_hex(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::keys::{parse_seq, Action, Match};

    fn action_for(map: &Keymap, s: &str) -> Option<Action> {
        let mut pending = Vec::new();
        parse_seq(s).unwrap().into_iter().find_map(|k| match map.feed(&mut pending, k) {
            Match::Action(a) => Some(a),
            _ => None,
        })
    }

    #[test]
    fn empty_config_is_defaults() {
        let (cfg, errors) = parse("");
        assert!(errors.is_empty());
        assert_eq!(cfg.colors.bg, Color::Rgb(0x2e, 0x34, 0x40));
        assert_eq!(action_for(&cfg.keymap, "n"), Some(Action::Down));
    }

    #[test]
    fn overrides_keys_and_colors() {
        let (cfg, errors) = parse(
            r##"
            [keys]
            down = "j"
            quit = ["q", "ZZ"]
            [colors]
            primary = "#ff0000"
            "##,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(action_for(&cfg.keymap, "j"), Some(Action::Down));
        assert_eq!(action_for(&cfg.keymap, "ZZ"), Some(Action::Quit));
        assert_eq!(cfg.colors.primary, Color::Rgb(255, 0, 0));
        assert_eq!(cfg.colors.bg, Colors::default().bg);
    }

    #[test]
    fn reports_bad_entries_and_keeps_defaults() {
        let (cfg, errors) = parse(
            r##"
            [keys]
            down = 3
            [colors]
            primary = "blue"
            nope = "#000000"
            "##,
        );
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_eq!(action_for(&cfg.keymap, "n"), Some(Action::Down));
        assert_eq!(cfg.colors.primary, Colors::default().primary);
    }

    #[test]
    fn mouse_is_off_unless_turned_on() {
        assert!(!parse("").0.mouse);
        assert!(parse("mouse = true").0.mouse);
        let (cfg, errors) = parse("mouse = \"yes\"");
        assert!(!cfg.mouse);
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn invalid_toml_is_reported() {
        let (_, errors) = parse("[keys");
        assert_eq!(errors.len(), 1);
    }
}
