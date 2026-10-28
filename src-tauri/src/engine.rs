//! The playback engine without Tauri: audio pipeline, TIDAL client and the
//! settings SONE already stores. Used by the headless daemon (`soned`); the
//! GUI builds the same parts inside `AppState::new`.
//!
//! The engine never writes settings except refreshed auth tokens, and runs no
//! migrations: logging in and changing settings stay the GUI's job.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::audio::AudioPlayer;
use crate::cache::DiskCache;
use crate::crypto::Crypto;
use crate::events::EventSink;
use crate::stream::ResolvedStream;
use crate::tidal_api::{AuthTokens, TidalClient};
use crate::{AuthMethod, Settings, SignalPathTracker, SoneError};

/// Resolve credentials from saved settings, falling back to embedded defaults
/// matching the saved auth_method (LoginCode → device-code pair, Pkce → PKCE pair).
pub(crate) fn resolve_credentials(settings: &Settings) -> (String, String) {
    if !settings.client_id.is_empty() {
        return (settings.client_id.clone(), settings.client_secret.clone());
    }
    match settings.auth_method {
        AuthMethod::Pkce if crate::embedded_config::has_pkce_keys() => (
            crate::embedded_config::stream_key_c(),
            crate::embedded_config::stream_key_d(),
        ),
        _ => (
            crate::embedded_config::stream_key_a(),
            crate::embedded_config::stream_key_b(),
        ),
    }
}

pub struct Engine {
    pub audio_player: Arc<AudioPlayer>,
    pub tidal_client: Mutex<TidalClient>,
    pub signal_path: Arc<SignalPathTracker>,
    /// The GUI's cache directory: playlist edits made here invalidate the
    /// entries the GUI would otherwise show stale.
    pub disk_cache: DiskCache,
    pub settings: Settings,
}

impl Engine {
    /// Build the engine from the settings in `config_dir` (normally
    /// `~/.config/sone`). Fails when the settings cannot be read or hold no
    /// login: the daemon has no way to log in on its own.
    pub async fn new(sink: EventSink, config_dir: &Path) -> Result<Self, SoneError> {
        let settings_path = config_dir.join("settings.json");
        let crypto = Arc::new(Crypto::new(config_dir)?);
        let disk_cache = DiskCache::new(&config_dir.join("cache"), Arc::clone(&crypto));
        let settings = read_settings(&settings_path, &crypto).ok_or_else(|| {
            SoneError::NotConfigured(format!(
                "cannot read {}: start SONE once and log in",
                settings_path.display()
            ))
        })?;
        let tokens = settings
            .auth_tokens
            .clone()
            .ok_or(SoneError::NotAuthenticated)?;

        let host_caps = crate::audio::probe_host_caps();
        let proxied_http =
            crate::proxy_http::ProxiedHttp::from_settings(&settings.proxy, &host_caps);

        let signal_path = Arc::new(SignalPathTracker::new(sink.clone()));
        signal_path.set_audio_modes(settings.exclusive_mode, settings.bit_perfect);
        signal_path.set_normalization_enabled(settings.volume_normalization);

        let audio_player = Arc::new(AudioPlayer::new(
            sink,
            Arc::clone(&signal_path),
            settings.proxy.clone(),
        ));
        apply_audio_settings(&audio_player, &settings);

        let mut client = TidalClient::new(proxied_http);
        client.set_token_persist({
            let settings_path = settings_path.clone();
            let crypto = Arc::clone(&crypto);
            Arc::new(move |tokens: &AuthTokens| {
                crate::persist_auth_tokens(&settings_path, &crypto, tokens);
            })
        });
        let (id, secret) = resolve_credentials(&settings);
        client.tokens = Some(tokens);
        client.set_credentials(&id, &secret);
        if let Err(e) = client.get_session_info().await {
            log::warn!("[engine] session info failed (using default country code): {e}");
        }

        Ok(Self {
            audio_player,
            tidal_client: Mutex::new(client),
            signal_path,
            disk_cache,
            settings,
        })
    }

    /// Resolve a track's stream with the stored quality and normalization
    /// settings.
    pub async fn resolve(
        &self,
        track_id: u64,
        use_track_gain: bool,
    ) -> Result<ResolvedStream, SoneError> {
        crate::stream::resolve(
            &self.tidal_client,
            &self.settings.max_quality,
            self.settings.volume_normalization,
            track_id,
            use_track_gain,
        )
        .await
    }
}

/// The default config directory, `~/.config/sone`.
pub fn default_config_dir() -> Option<PathBuf> {
    crate::config_dir_for_env()
}

fn read_settings(path: &Path, crypto: &Crypto) -> Option<Settings> {
    let data = fs::read(path).ok()?;
    let plain = crypto.decrypt(&data).ok()?;
    serde_json::from_slice(&plain).ok()
}

fn apply_audio_settings(player: &AudioPlayer, settings: &Settings) {
    let results = [
        player.set_volume(settings.volume),
        player.set_gapless(settings.gapless),
        player.set_exclusive_mode(settings.exclusive_mode, settings.exclusive_device.clone()),
        player.set_bit_perfect(settings.exclusive_mode && settings.bit_perfect),
    ];
    for e in results.into_iter().filter_map(Result::err) {
        log::warn!("[engine] applying audio settings: {e}");
    }
}
