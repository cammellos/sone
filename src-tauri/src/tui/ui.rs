//! Drawing: tabline, the tab's content, statusline, command line. Reads the
//! app, changes nothing but the list height it records for paging.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::app::{App, Mode, PromptKind, Row, Setting, View, QUEUE, SETTING_ROWS, TABS};
use super::config::Colors;
use super::keys::{Action, Key};

pub fn draw(app: &mut App, frame: &mut Frame) {
    let c = app.config.colors;
    frame.render_widget(Block::default().style(Style::default().bg(c.bg).fg(c.text)), frame.area());

    let [tabline, body, statusline, cmdline] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_tabline(app, frame, tabline);
    app.list_height = body.height as usize;
    if app.settings.is_some() {
        draw_settings(app, frame, body);
    } else if app.tab == QUEUE {
        draw_queue(app, frame, body);
    } else {
        draw_view(app, frame, body);
    }
    draw_statusline(app, frame, statusline);
    draw_cmdline(app, frame, cmdline);
}

fn draw_tabline(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    let mut spans: Vec<Span> = TABS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let label = format!(" {name} ");
            if i == app.tab {
                Span::styled(label, Style::default().fg(c.bg).bg(c.primary).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(label, Style::default().fg(c.text_dim).bg(c.surface))
            }
        })
        .collect();
    // Breadcrumb of the current tab's views: "One › Two".
    let crumbs: Vec<&str> = if app.settings.is_some() {
        vec!["Settings"]
    } else {
        app.stacks[app.tab].iter().map(|v| v.title.as_str()).filter(|t| !t.is_empty()).collect()
    };
    if !crumbs.is_empty() {
        spans.push(Span::styled(format!("  {}", crumbs.join(" › ")), Style::default().fg(c.text_header)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(c.surface)), area);
}

fn hint(app: &App, frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(Paragraph::new(format!(" {text}")).style(Style::default().fg(app.config.colors.text_dim)), area);
}

fn draw_queue(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    if app.queue_rows() == 0 {
        let search = app.config.keymap.describe(Action::Search).unwrap_or_default();
        let tab = app.config.keymap.describe(Action::NextTab).unwrap_or_default();
        hint(app, frame, area, &format!("queue is empty — {search} to search, {tab} for playlists"));
        return;
    }
    let width = area.width as usize;
    let mut items: Vec<ListItem> = Vec::with_capacity(app.queue_rows());
    if let Some(t) = &app.queue_current {
        let marker = if app.status.state == "playing" { "▶" } else { "‖" };
        let style = Style::default().fg(c.primary).add_modifier(Modifier::BOLD);
        items.push(line_item(&format!(" {marker} {} – {}", t.artist, t.title), &mmss(t.duration), width, style));
    }
    for t in &app.upcoming {
        items.push(line_item(&format!("   {} – {}", t.artist, t.title), &mmss(t.duration), width, Style::default().fg(c.text)));
    }
    render_list(frame, area, items, app.queue_selected, &c);
}

fn draw_view(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    let Some(view) = app.view() else {
        let search = app.config.keymap.describe(Action::Search).unwrap_or_default();
        hint(app, frame, area, &format!("{search} to search Tidal"));
        return;
    };
    if view.loading {
        return hint(app, frame, area, "loading…");
    }
    if view.rows.is_empty() {
        return hint(app, frame, area, "nothing here");
    }
    let width = area.width as usize;
    let items: Vec<ListItem> = view.rows.iter().map(|r| row_item(r, view, width, &c)).collect();
    render_list(frame, area, items, view.selected, &c);
}

fn row_item<'a>(row: &Row, view: &View, width: usize, c: &Colors) -> ListItem<'a> {
    let text = Style::default().fg(c.text);
    match row {
        Row::Header(name) => ListItem::new(format!(" {name}"))
            .style(Style::default().fg(c.text_header).add_modifier(Modifier::BOLD)),
        Row::Track(t) => {
            // Inside an album, the artist repeats on every line: titles only.
            let left = match &view.source {
                Some((kind, _)) if kind == "album" => format!("   {}", t.title),
                _ => format!("   {} – {}", t.artist, t.title),
            };
            line_item(&left, &mmss(t.duration), width, text)
        }
        Row::Playlist { title, tracks, .. } => {
            line_item(&format!("   {title}"), &format!("{tracks} tracks"), width, text)
        }
        Row::Album { title, artist, year, kind, .. } => {
            let year = year.map(|y| format!("{y} – ")).unwrap_or_default();
            let kind = match kind.as_deref() {
                Some("EP") => " (EP)",
                Some("SINGLE") => " (Single)",
                _ => "",
            };
            // On an artist's page, the artist repeats on every line too.
            let left = match &view.source {
                Some((k, _)) if k == "artist" => format!("   {year}{title}{kind}"),
                _ => format!("   {year}{artist} – {title}{kind}"),
            };
            line_item(&left, "", width, text)
        }
        Row::Artist { name, .. } => line_item(&format!("   {name}"), "artist", width, text),
    }
}

fn draw_settings(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    let Some(page) = &app.settings else { return };
    let v = &page.values;
    if v.is_null() {
        return hint(app, frame, area, "loading…");
    }
    let on = |key: &str| if v[key].as_bool().unwrap_or(false) { "on" } else { "off" }.to_string();
    let device = || {
        let id = v["exclusiveDevice"].as_str();
        let name = v["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["id"].as_str() == id)
            .and_then(|d| d["name"].as_str());
        match (name, id) {
            (Some(n), Some(id)) => format!("{n} ({id})"),
            (None, Some(id)) => id.to_string(),
            _ => "default".to_string(),
        }
    };
    let width = area.width as usize;
    let text = Style::default().fg(c.text);
    let items: Vec<ListItem> = SETTING_ROWS
        .iter()
        .map(|row| {
            let (label, value) = match row {
                Setting::Header(name) => {
                    return ListItem::new(format!(" {name}"))
                        .style(Style::default().fg(c.text_header).add_modifier(Modifier::BOLD))
                }
                Setting::MaxQuality => ("Max quality", v["maxQuality"].as_str().unwrap_or("?").to_string()),
                Setting::Gapless => ("Gapless", on("gapless")),
                Setting::ExclusiveMode => ("Exclusive mode", on("exclusiveMode")),
                Setting::BitPerfect => ("Bit-perfect", on("bitPerfect")),
                Setting::Device => ("Output device", device()),
                Setting::Account if app.status.logged_in => ("Log out", String::new()),
                Setting::Account => ("Log in", String::new()),
            };
            line_item(&format!("   {label}"), &value, width, text)
        })
        .collect();
    render_list(frame, area, items, page.selected, &c);
}

fn render_list(frame: &mut Frame, area: Rect, items: Vec<ListItem>, selected: usize, c: &Colors) {
    let list = List::new(items).highlight_style(Style::default().bg(c.panel).fg(c.text_bright));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// `left` and `right` on one line, `left` cut with "…" when it doesn't fit.
/// Widths are terminal columns, so wide (CJK) characters count double.
fn line_item<'a>(left: &str, right: &str, width: usize, style: Style) -> ListItem<'a> {
    ListItem::new(fit(left, right, width)).style(style)
}

fn fit(left: &str, right: &str, width: usize) -> String {
    let right = format!("{right} ");
    let room = width.saturating_sub(cols(&right));
    if cols(left) < room {
        format!("{left}{}{right}", " ".repeat(room - cols(left)))
    } else {
        let mut cut = String::new();
        for ch in left.chars() {
            if cols(&cut) + cols(ch.encode_utf8(&mut [0; 4])) > room.saturating_sub(2) {
                break;
            }
            cut.push(ch);
        }
        let pad = room.saturating_sub(cols(&cut) + 1);
        format!("{cut}…{}{right}", " ".repeat(pad))
    }
}

fn cols(s: &str) -> usize {
    Span::raw(s).width()
}

fn draw_statusline(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    let s = &app.status;
    let (mode, mode_color) = if !app.connected {
        ("OFFLINE", c.error)
    } else if !s.logged_in {
        ("LOGGED OUT", c.error)
    } else {
        match s.state.as_str() {
            "playing" => ("PLAYING", c.primary),
            "paused" => ("PAUSED", c.warning),
            _ => ("STOPPED", c.faint),
        }
    };
    let now = s.track.as_ref().map(|t| format!(" {} – {}", t.artist, t.title)).unwrap_or_default();
    let progress = s
        .track
        .as_ref()
        .map(|t| format!("{}/{}", mmss(app.position().min(t.duration)), mmss(t.duration)))
        .unwrap_or_default();
    let right = format!(
        "{progress}  vol {:.0}%  rep {}  shuf {} ",
        s.volume * 100.0,
        s.repeat,
        if s.shuffle { "on" } else { "off" }
    );
    let left_len = mode.len() + 2 + cols(&now);
    let pad = (area.width as usize).saturating_sub(left_len + cols(&right));
    let line = Line::from(vec![
        Span::styled(format!(" {mode} "), Style::default().fg(c.bg).bg(mode_color).add_modifier(Modifier::BOLD)),
        Span::styled(now, Style::default().fg(c.text_bright)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(c.text_dim)),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(c.surface)), area);
}

fn draw_cmdline(app: &App, frame: &mut Frame, area: Rect) {
    let c = app.config.colors;
    let line = match (&app.mode, &app.message) {
        (Mode::Prompt(kind, text), _) => {
            let prompt = match kind {
                PromptKind::Command => app.config.keymap.describe(Action::CommandLine),
                PromptKind::Search => app.config.keymap.describe(Action::Search),
                PromptKind::Login => Some("paste the address the browser ended on: ".into()),
            }
            .unwrap_or_else(|| ":".into());
            let x = area.x + (prompt.chars().count() + text.chars().count()) as u16;
            frame.set_cursor_position((x, area.y));
            Line::from(vec![Span::styled(prompt, Style::default().fg(c.primary)), Span::raw(text.clone())])
        }
        (Mode::Normal, Some(m)) => {
            Line::styled(m.text.clone(), Style::default().fg(if m.error { c.error } else { c.text }))
        }
        (Mode::Normal, None) => {
            let pending: String = app.pending_keys().iter().map(key_label).collect();
            let pad = (area.width as usize).saturating_sub(pending.chars().count() + 1);
            Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(pending, Style::default().fg(c.text_dim))])
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn key_label(k: &Key) -> String {
    match k {
        Key::Char(c) => c.to_string(),
        Key::Ctrl(c) => format!("^{c}"),
        _ => String::new(),
    }
}

fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::{cols, fit};

    #[test]
    fn fit_fills_the_width_and_keeps_the_right_side() {
        for left in ["   Zu – Ostia", "   Backspace – 被丟失的他", &"   x".repeat(40)] {
            let line = fit(left, "3:16", 40);
            assert_eq!(cols(&line), 40, "{line:?}");
            assert!(line.ends_with("3:16 "), "{line:?}");
        }
    }

    #[test]
    fn fit_cuts_wide_text_with_an_ellipsis() {
        let line = fit(&"被".repeat(30), "1:00", 20);
        assert!(line.contains('…'), "{line:?}");
        assert!(cols(&line) <= 20, "{line:?}");
    }
}
