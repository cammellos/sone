//! Headless SONE: plays the queue, no window. Control it with `sonectl`.
//!
//! Log in with `sonectl login`; the session is stored in the config
//! directory. Don't run it alongside the GUI: both drive the same audio
//! device and session.
//!
//! `--mcp-url-file` starts the MCP server from a file holding the URL clients
//! use (`http://127.0.0.1:<port>/<token>/mcp`), whatever the settings say.

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    // The handle must outlive main, or logging stops.
    let _logger = flexi_logger::Logger::try_with_env_or_str("info")
        .and_then(|l| l.start())
        .map_err(|e| eprintln!("soned: logger: {e}"))
        .ok();

    let mut args = std::env::args().skip(1);
    let mut socket = tauri_app_lib::daemon::default_socket_path();
    let mut config_dir = tauri_app_lib::engine::default_config_dir();
    let mut mcp_url_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or(socket),
            "--config-dir" => config_dir = args.next().map(PathBuf::from),
            "--mcp-url-file" => mcp_url_file = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: soned [--socket PATH] [--config-dir DIR] [--mcp-url-file PATH]");
                return;
            }
            other => {
                eprintln!("soned: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(config_dir) = config_dir else {
        eprintln!("soned: no config directory (set --config-dir)");
        std::process::exit(1);
    };

    let mcp = mcp_url_file.map(|path| {
        let url = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!("soned: {}: {e}", path.display());
            std::process::exit(1);
        });
        tauri_app_lib::daemon::parse_mcp_url(&url).unwrap_or_else(|| {
            eprintln!("soned: {}: not an MCP URL", path.display());
            std::process::exit(1);
        })
    });

    if let Err(e) = tauri_app_lib::daemon::run(&config_dir, &socket, mcp).await {
        eprintln!("soned: {e}");
        std::process::exit(1);
    }
}
