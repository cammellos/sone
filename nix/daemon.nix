{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsNoGuiHook,
  openssl,
  dbus,
  glib-networking,
  alsa-lib,
  libpulseaudio,
  gst_all_1,
  src,
}:

# Headless SONE: `soned` (daemon), `sonectl` (CLI) and `sonetui` (TUI), built without the
# `gui` feature, so no Tauri, WebKit or GTK. Logs in with the session the GUI
# stored in ~/.config/sone.
rustPlatform.buildRustPackage {
  pname = "soned";
  version = (builtins.fromJSON (builtins.readFile ../src-tauri/tauri.conf.json)).version;

  inherit src;

  cargoRoot = "src-tauri";
  buildAndTestSubdir = "src-tauri";
  cargoLock.lockFile = ../src-tauri/Cargo.lock;

  buildNoDefaultFeatures = true;
  buildFeatures = [ "tui" ];
  cargoBuildFlags = [
    "--bin"
    "soned"
    "--bin"
    "sonectl"
    "--bin"
    "sonetui"
  ];
  cargoTestFlags = [ "--lib" ];

  nativeBuildInputs = [
    pkg-config
    # Wraps the binaries with GST_PLUGIN_SYSTEM_PATH_1_0 and GIO_EXTRA_MODULES
    # (glib-networking, for HTTPS streams); without them playback fails.
    wrapGAppsNoGuiHook
  ];

  buildInputs = [
    openssl
    dbus
    glib-networking
    alsa-lib
    libpulseaudio
    gst_all_1.gstreamer
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-good
    gst_all_1.gst-plugins-bad
    gst_all_1.gst-libav
  ];

  meta = {
    description = "Headless SONE daemon and CLI for TIDAL";
    homepage = "https://github.com/lullabyX/sone";
    license = lib.licenses.gpl3Only;
    mainProgram = "soned";
    platforms = [ "x86_64-linux" ];
  };
}
