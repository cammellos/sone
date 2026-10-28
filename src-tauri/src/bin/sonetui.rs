//! Terminal client for `soned`. Quitting leaves the music playing.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config = tauri_app_lib::tui::config::default_path();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = args.next().map(Into::into),
            "-h" | "--help" => {
                println!("usage: sonetui [--config PATH]   (socket: $SONE_SOCKET or the daemon default)");
                return Ok(());
            }
            other => {
                eprintln!("sonetui: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let socket = std::env::var_os("SONE_SOCKET")
        .map(Into::into)
        .unwrap_or_else(tauri_app_lib::daemon::default_socket_path);

    // ratatui::init enters raw mode + the alternate screen and restores them
    // on panic; restore() covers the normal exit.
    let mut terminal = ratatui::init();
    let result = tauri_app_lib::tui::run(&mut terminal, socket, config).await;
    ratatui::restore();
    result
}
