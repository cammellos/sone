//! Command-line client for `soned`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::{json, Value};

const USAGE: &str = "usage: sonectl <command>

  status                      what is playing
  play playlist|album|artist|mix <id>
  play track <id>...          replace the queue with these tracks
  shuffle-play playlist|album|artist|mix <id>
  add <track id>...           append to the queue
  next-up <track id>...       play these right after the current track
  pause | resume | toggle | stop
  next | prev
  seek <seconds>
  vol <0-100>
  repeat off|all|one
  shuffle [on|off]
  queue [limit]               current and upcoming tracks
  jump <n>                    play the nth upcoming track (1 = next)
  remove <track id>           drop it from the upcoming tracks
  clear                       drop all upcoming tracks
  login                       sign in to Tidal in the browser
  logout                      forget the login (stops playback)
  settings                    quality, gapless, exclusive mode, output devices
  set <name> <value>          max_quality HI_RES_LOSSLESS|LOSSLESS|HIGH, gapless on|off,
                              exclusive_mode on|off, bit_perfect on|off, exclusive_device <id>
  playlists                   your playlists (uuid, tracks, title)
  rename <uuid> <name>        rename one of your playlists
  search <query>              tracks, albums, artists and playlists
  watch                       print state changes as JSON lines
  raw <cmd> [json args]       send any daemon command";

fn main() {
    // Exit quietly when the reader goes away (`sonectl search x | head`)
    // instead of panicking on the failed write.
    // SAFETY: called before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket = std::env::var_os("SONE_SOCKET")
        .map(Into::into)
        .unwrap_or_else(tauri_app_lib::daemon::default_socket_path);

    if args == ["login"] {
        return login(&socket);
    }

    let (cmd, req_args, print): (&str, Value, fn(&Value)) = match parse(&args) {
        Ok(x) => x,
        Err(msg) => {
            eprintln!("{msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let stream = UnixStream::connect(&socket).unwrap_or_else(|e| {
        eprintln!("sonectl: cannot reach soned at {}: {e}", socket.display());
        std::process::exit(1);
    });
    let mut writer = stream.try_clone().expect("clone socket");
    let mut lines = BufReader::new(stream).lines();

    let req = json!({ "id": 1, "cmd": cmd, "args": req_args });
    writeln!(writer, "{req}").expect("write request");

    let reply: Value = match lines.next() {
        Some(Ok(line)) => serde_json::from_str(&line).expect("daemon sent invalid JSON"),
        _ => {
            eprintln!("sonectl: daemon closed the connection");
            std::process::exit(1);
        }
    };
    if reply["ok"] != json!(true) {
        eprintln!("sonectl: {}", reply["error"].as_str().unwrap_or("error"));
        std::process::exit(1);
    }
    print(&reply["result"]);

    if cmd == "subscribe" {
        for line in lines.map_while(Result::ok) {
            println!("{line}");
        }
    }
}

/// One request on its own connection; exits with the daemon's error.
fn call(socket: &std::path::Path, cmd: &str, args: Value) -> Value {
    let stream = UnixStream::connect(socket).unwrap_or_else(|e| {
        eprintln!("sonectl: cannot reach soned at {}: {e}", socket.display());
        std::process::exit(1);
    });
    let mut writer = stream.try_clone().expect("clone socket");
    writeln!(writer, "{}", json!({ "id": 1, "cmd": cmd, "args": args })).expect("write request");
    let line = BufReader::new(stream).lines().next().and_then(Result::ok).unwrap_or_else(|| {
        eprintln!("sonectl: daemon closed the connection");
        std::process::exit(1);
    });
    let reply: Value = serde_json::from_str(&line).expect("daemon sent invalid JSON");
    if reply["ok"] != json!(true) {
        eprintln!("sonectl: {}", reply["error"].as_str().unwrap_or("error"));
        std::process::exit(1);
    }
    reply["result"].clone()
}

/// PKCE login: open Tidal's sign-in page, then hand the daemon the address
/// the browser ends on.
fn login(socket: &std::path::Path) {
    let url = call(socket, "login_start", Value::Null)["url"].as_str().unwrap_or_default().to_string();
    println!("Sign in to Tidal at:\n\n  {url}\n");
    let opened = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if opened {
        println!("(opened in your browser)");
    }
    println!("Then paste the address the browser ends on (https://tidal.com/android/login/auth?code=…):");
    let mut pasted = String::new();
    if std::io::stdin().read_line(&mut pasted).is_err() || pasted.trim().is_empty() {
        eprintln!("sonectl: nothing pasted, login cancelled");
        std::process::exit(1);
    }
    let status = call(socket, "login_finish", json!({ "redirect_url": pasted.trim() }));
    println!("logged in");
    print_status(&status);
}

type Parsed<'a> = (&'a str, Value, fn(&Value));

fn parse(args: &[String]) -> Result<Parsed<'_>, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let ids = |rest: &[&str]| -> Result<Vec<u64>, String> {
        if rest.is_empty() {
            return Err("missing track id".into());
        }
        rest.iter()
            .map(|s| s.parse().map_err(|_| format!("not a track id: {s}")))
            .collect()
    };
    let source = |rest: &[&str]| -> Result<Value, String> {
        match rest {
            [kind @ ("playlist" | "album" | "artist" | "mix"), id] => {
                Ok(json!({ "source_type": kind, "id": id }))
            }
            _ => Err("expected playlist|album|artist|mix <id>".into()),
        }
    };
    Ok(match a.as_slice() {
        [] | ["status"] => ("status", Value::Null, print_status),
        ["play", "track", rest @ ..] => (
            "play_tracks",
            json!({ "track_ids": ids(rest)?, "action": "play_now" }),
            print_status,
        ),
        ["play", rest @ ..] => ("play_source", source(rest)?, print_status),
        ["shuffle-play", rest @ ..] => ("shuffle_source", source(rest)?, print_status),
        ["add", rest @ ..] => (
            "play_tracks",
            json!({ "track_ids": ids(rest)?, "action": "queue" }),
            print_status,
        ),
        ["next-up", rest @ ..] => (
            "play_tracks",
            json!({ "track_ids": ids(rest)?, "action": "play_next" }),
            print_status,
        ),
        [c @ ("pause" | "resume" | "toggle" | "stop" | "next")] => (c, Value::Null, print_status),
        ["prev"] => ("previous", Value::Null, print_status),
        ["seek", s] => (
            "seek",
            json!({ "position_seconds": s.parse::<f64>().map_err(|_| "seconds must be a number")? }),
            print_status,
        ),
        ["vol", v] => {
            let pct: f64 = v.parse().map_err(|_| "volume must be 0-100")?;
            ("volume", json!({ "level": (pct / 100.0).clamp(0.0, 1.0) }), print_status)
        }
        ["repeat", m @ ("off" | "all" | "one")] => ("repeat", json!({ "mode": m }), print_status),
        ["shuffle"] => ("shuffle", json!({}), print_status),
        ["shuffle", "on"] => ("shuffle", json!({ "on": true }), print_status),
        ["shuffle", "off"] => ("shuffle", json!({ "on": false }), print_status),
        ["queue"] => ("queue", json!({}), print_queue),
        ["queue", n] => (
            "queue",
            json!({ "limit": n.parse::<u64>().map_err(|_| "limit must be a number")? }),
            print_queue,
        ),
        ["jump", n] => {
            let n: u64 = n.parse().map_err(|_| "n must be a number")?;
            if n == 0 {
                return Err("n starts at 1 (the next track)".into());
            }
            ("jump", json!({ "index": n - 1 }), print_status)
        }
        ["remove", id] => (
            "remove",
            json!({ "track_id": id.parse::<u64>().map_err(|_| "not a track id")? }),
            print_status,
        ),
        ["clear"] => ("clear_queue", Value::Null, print_status),
        ["playlists"] => ("playlists", Value::Null, print_playlists),
        ["rename", uuid, name @ ..] if !name.is_empty() => (
            "rename_playlist",
            json!({ "uuid": uuid, "name": name.join(" ") }),
            print_renamed,
        ),
        ["logout"] => ("logout", Value::Null, print_status),
        ["settings"] => ("settings", Value::Null, print_settings),
        ["set", name, value] => {
            let value = match *value {
                "on" | "true" => json!(true),
                "off" | "false" => json!(false),
                v => json!(v),
            };
            ("set_setting", json!({ "name": name, "value": value }), print_settings)
        }
        ["search", q @ ..] if !q.is_empty() => ("search", json!({ "query": q.join(" ") }), print_search),
        ["watch"] => ("subscribe", Value::Null, print_json),
        ["raw", cmd] => (cmd, Value::Null, print_json),
        ["raw", cmd, json_args] => (
            cmd,
            serde_json::from_str(json_args).map_err(|e| format!("bad JSON args: {e}"))?,
            print_json,
        ),
        _ => return Err(format!("unknown command: {}", a.join(" "))),
    })
}

fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn track_line(t: &Value) -> String {
    format!(
        "{} – {}",
        t["artist"].as_str().unwrap_or("?"),
        t["title"].as_str().unwrap_or("?")
    )
}

fn print_status(s: &Value) {
    let state = s["state"].as_str().unwrap_or("?");
    match s.get("track").filter(|t| !t.is_null()) {
        Some(t) => println!(
            "[{state}] {}  {}/{}",
            track_line(t),
            mmss(s["positionSeconds"].as_f64().unwrap_or(0.0)),
            mmss(t["durationSeconds"].as_f64().unwrap_or(0.0)),
        ),
        None => println!("[{state}] nothing queued"),
    }
    if s["loggedIn"] == json!(false) {
        println!("not logged in: sonectl login");
    }
    println!(
        "volume {:.0}%  repeat {}  shuffle {}  {} upcoming",
        s["volume"].as_f64().unwrap_or(0.0) * 100.0,
        s["repeat"].as_str().unwrap_or("?"),
        if s["shuffle"] == json!(true) { "on" } else { "off" },
        s["upcoming"].as_u64().unwrap_or(0),
    );
}

fn print_queue(q: &Value) {
    match q.get("current").filter(|t| !t.is_null()) {
        Some(t) => println!("now  {}", track_line(t)),
        None => println!("now  (nothing)"),
    }
    for (i, t) in q["upcoming"].as_array().into_iter().flatten().enumerate() {
        println!("{:>3}  {}", i + 1, track_line(t));
    }
}

fn print_playlists(v: &Value) {
    for p in v["playlists"].as_array().into_iter().flatten() {
        println!(
            "{}  {:>4}  {}",
            p["uuid"].as_str().unwrap_or("?"),
            p["tracks"].as_u64().unwrap_or(0),
            p["title"].as_str().unwrap_or("?")
        );
    }
}

fn print_renamed(v: &Value) {
    println!("renamed to {}", v["title"].as_str().unwrap_or("?"));
}

fn print_search(v: &Value) {
    let section = |name: &str, key: &str, line: &dyn Fn(&Value) -> String| {
        let items = v[key].as_array().cloned().unwrap_or_default();
        if !items.is_empty() {
            println!("{name}");
            for item in &items {
                println!("  {}", line(item));
            }
        }
    };
    section("Albums", "albums", &|a| {
        let year = a["year"].as_u64().map(|y| y.to_string()).unwrap_or_else(|| "????".into());
        let kind = match a["type"].as_str() {
            Some("EP") => " (EP)",
            Some("SINGLE") => " (Single)",
            _ => "",
        };
        format!(
            "{:<10} {year} – {} – {}{kind}",
            a["id"],
            a["artist"].as_str().unwrap_or("?"),
            a["title"].as_str().unwrap_or("?")
        )
    });
    section("Tracks", "tracks", &|t| format!("{:<10} {}", t["id"], track_line(t)));
    section("Artists", "artists", &|a| format!("{:<10} {}", a["id"], a["name"].as_str().unwrap_or("?")));
    section("Playlists", "playlists", &|p| {
        format!("{}  {}", p["uuid"].as_str().unwrap_or("?"), p["title"].as_str().unwrap_or("?"))
    });
}

fn print_settings(v: &Value) {
    let on = |key: &str| if v[key] == json!(true) { "on" } else { "off" };
    println!("max_quality       {}", v["maxQuality"].as_str().unwrap_or("?"));
    println!("gapless           {}", on("gapless"));
    println!("exclusive_mode    {}", on("exclusiveMode"));
    println!("bit_perfect       {}", on("bitPerfect"));
    println!("exclusive_device  {}", v["exclusiveDevice"].as_str().unwrap_or("(default)"));
    for d in v["devices"].as_array().into_iter().flatten() {
        println!("  {:<24} {}", d["id"].as_str().unwrap_or("?"), d["name"].as_str().unwrap_or(""));
    }
    println!("logged in         {}", if v["loggedIn"] == json!(true) { "yes" } else { "no" });
}

fn print_json(v: &Value) {
    println!("{v}");
}
