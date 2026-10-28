//! PKCE login against Tidal (login.tidal.com), shared by the app's login
//! commands and the daemon's `login_start` / `login_finish`.

use base64::Engine;
use rand::RngExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const PKCE_REDIRECT_URI: &str = "https://tidal.com/android/login/auth";

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PkceAuthParams {
    pub authorize_url: String,
    pub code_verifier: String,
    pub client_unique_key: String,
}

pub fn build_pkce_params(client_id: &str) -> PkceAuthParams {
    let mut rng = rand::rng();
    let random_bytes: [u8; 32] = rng.random();
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());

    let client_unique_key = format!("{:016x}", rng.random::<u64>());

    let authorize_url = format!(
        "https://login.tidal.com/authorize?response_type=code&redirect_uri={}&client_id={}&lang=EN&appMode=android&client_unique_key={}&code_challenge={}&code_challenge_method=S256&restrict_signup=true",
        "https%3A%2F%2Ftidal.com%2Fandroid%2Flogin%2Fauth",
        client_id,
        client_unique_key,
        code_challenge,
    );

    PkceAuthParams {
        authorize_url,
        code_verifier,
        client_unique_key,
    }
}

/// The authorization code from what the user pasted: the address the login
/// ended on (`https://tidal.com/android/login/auth?code=…`) or the bare code.
pub fn code_from_redirect(pasted: &str) -> Option<String> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        return None;
    }
    if !pasted.contains("://") {
        let bare = !pasted.contains(|c: char| c.is_whitespace() || "?&=/".contains(c));
        return bare.then(|| pasted.to_string());
    }
    let url = reqwest::Url::parse(pasted).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .filter(|c| !c.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_from_redirect_reads_the_code() {
        let url = "https://tidal.com/android/login/auth?code=abc_123-x&state=na";
        assert_eq!(code_from_redirect(url).as_deref(), Some("abc_123-x"));
        assert_eq!(code_from_redirect("  abc_123-x\n").as_deref(), Some("abc_123-x"));
        assert_eq!(code_from_redirect("https://tidal.com/android/login/auth?error=x"), None);
        assert_eq!(code_from_redirect("not a code"), None);
        assert_eq!(code_from_redirect(""), None);
    }

    #[test]
    fn authorize_url_carries_challenge_and_key() {
        let p = build_pkce_params("cid");
        assert!(p.authorize_url.starts_with("https://login.tidal.com/authorize?"));
        assert!(p.authorize_url.contains("client_id=cid"));
        assert!(p.authorize_url.contains(&format!("client_unique_key={}", p.client_unique_key)));
    }
}
