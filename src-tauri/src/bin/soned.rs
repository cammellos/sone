//! Headless SONE: plays the queue, no window. Control it with `sonectl`.
//!
//! Needs a SONE login: start the GUI once and log in, the daemon reuses the
//! stored session. Don't run it alongside the GUI: both drive the same
//! audio device and session.

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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or(socket),
            "--config-dir" => config_dir = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: soned [--socket PATH] [--config-dir DIR]");
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

    if let Err(e) = tauri_app_lib::daemon::run(&config_dir, &socket).await {
        eprintln!("soned: {e}");
        std::process::exit(1);
    }
}
