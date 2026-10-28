//! The playback engine without Tauri: audio pipeline, TIDAL client and the
//! settings SONE already stores. Used by the headless daemon (`soned`); the
//! GUI builds the same parts inside `AppState::new`.
//!
//! The engine writes settings to save a login, a logout, refreshed auth
//! tokens and the few settings the daemon changes (`update_settings`). It
//! runs no migrations: that stays the GUI's job.

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
    /// What settings.json holds; `update_settings` changes both.
    settings: std::sync::Mutex<Settings>,
    settings_path: PathBuf,
    crypto: Arc<Crypto>,
}

impl Engine {
    /// Build the engine from the settings in `config_dir` (normally
    /// `~/.config/sone`); without a settings file, from the defaults and
    /// logged out (`login` fixes that). Fails when the file exists but can't
    /// be read, so it is never overwritten.
    pub async fn new(sink: EventSink, config_dir: &Path) -> Result<Self, SoneError> {
        let settings_path = config_dir.join("settings.json");
        fs::create_dir_all(config_dir)?;
        let crypto = Arc::new(Crypto::new(config_dir)?);
        let disk_cache = DiskCache::new(&config_dir.join("cache"), Arc::clone(&crypto));
        let settings = if settings_path.exists() {
            read_settings(&settings_path, &crypto).ok_or_else(|| {
                SoneError::NotConfigured(format!("cannot read {}", settings_path.display()))
            })?
        } else {
            Settings::default()
        };

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
        client.tokens = settings.auth_tokens.clone();
        client.set_credentials(&id, &secret);
        if client.tokens.is_some() {
            if let Err(e) = client.get_session_info().await {
                log::warn!("[engine] session info failed (using default country code): {e}");
            }
        } else {
            log::warn!("[engine] not logged in");
        }

        Ok(Self {
            audio_player,
            tidal_client: Mutex::new(client),
            signal_path,
            disk_cache,
            settings: std::sync::Mutex::new(settings),
            settings_path,
            crypto,
        })
    }

    pub async fn logged_in(&self) -> bool {
        self.tidal_client.lock().await.tokens.is_some()
    }

    /// Finish a PKCE login (`crate::pkce`) with the embedded credentials and
    /// save it the way the app's browser login does (`finish_embedded_pkce`).
    pub async fn login(&self, code: &str, code_verifier: &str, client_unique_key: &str) -> Result<(), SoneError> {
        if !crate::embedded_config::has_pkce_keys() {
            return Err(SoneError::NotConfigured("PKCE credentials are not embedded in this build".into()));
        }
        let mut client = self.tidal_client.lock().await;
        client.set_credentials(
            &crate::embedded_config::stream_key_c(),
            &crate::embedded_config::stream_key_d(),
        );
        let tokens = client
            .exchange_pkce_code(code, code_verifier, crate::pkce::PKCE_REDIRECT_URI, client_unique_key)
            .await?;
        if let Err(e) = client.get_session_info().await {
            log::warn!("[engine] session info failed after login: {e}");
        }
        drop(client);

        self.update_settings(|s| {
            s.auth_tokens = Some(tokens.clone());
            s.auth_method = AuthMethod::Pkce;
            s.legacy_auth_notice_count = 0;
            s.client_id = String::new();
            s.client_secret = String::new();
        })
    }

    /// Forget the login, as the app's logout does for what the engine has:
    /// the session, the saved tokens and the cache. Other settings stay.
    pub async fn logout(&self) -> Result<(), SoneError> {
        {
            let mut client = self.tidal_client.lock().await;
            client.tokens = None;
            client.country_code = "US".to_string();
        }
        self.update_settings(|s| {
            s.auth_tokens = None;
            s.last_track_id = None;
        })?;
        self.disk_cache.clear().await;
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Change settings and save them. The file is re-read first, so tokens
    /// the client refreshed and saved meanwhile are kept.
    pub fn update_settings(&self, change: impl Fn(&mut Settings)) -> Result<(), SoneError> {
        let mut stored = read_settings(&self.settings_path, &self.crypto).unwrap_or_default();
        change(&mut stored);
        let json = serde_json::to_string_pretty(&stored)?;
        fs::write(&self.settings_path, self.crypto.encrypt(json.as_bytes())?)?;
        change(&mut self.settings.lock().unwrap());
        Ok(())
    }

    /// Resolve a track's stream with the stored quality and normalization
    /// settings.
    pub async fn resolve(
        &self,
        track_id: u64,
        use_track_gain: bool,
    ) -> Result<ResolvedStream, SoneError> {
        let settings = self.settings();
        crate::stream::resolve(
            &self.tidal_client,
            &settings.max_quality,
            settings.volume_normalization,
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
