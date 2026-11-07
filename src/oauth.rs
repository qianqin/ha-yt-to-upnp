//! YouTube OAuth 2.0 TV device flow.
//!
//! Implements the same flow as YouTube.js's OAuth2 module:
//!   1. Scrape youtube.com/tv to extract client_id + client_secret
//!   2. Request a device code (user visits youtube.com/activate, enters code)
//!   3. Poll for access_token + refresh_token
//!   4. Auto-refresh access_token when expired
//!
//! Premium subscribers with a valid Bearer token get stream_protection_status=1,
//! bypassing the PoToken requirement entirely. This eliminates the need for the
//! bgutil-pot sidecar (~66MB RAM) on the Pi Zero 2 W.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Persistent OAuth credentials (saved to disk).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub access_token: String,
    /// ISO 8601 expiry time for the access token.
    pub expiry_date: String,
}

impl OAuthCredentials {
    /// Whether we have a refresh token (i.e. user has completed login).
    pub fn is_logged_in(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// Whether the access token has expired (or will within 60s).
    pub fn is_expired(&self) -> bool {
        if self.access_token.is_empty() || self.expiry_date.is_empty() {
            return true;
        }
        let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(&self.expiry_date) else {
            return true;
        };
        let now = chrono::Utc::now();
        expiry < now + chrono::Duration::seconds(60)
    }
}

/// Device code response from YouTube.
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    #[allow(dead_code)]
    expires_in: u64,
    interval: u64,
    #[serde(default)]
    error_code: Option<String>,
}

/// Token response from YouTube.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Runtime OAuth state, shared across the application.
pub struct OAuthState {
    pub credentials: RwLock<OAuthCredentials>,
    http: reqwest::Client,
}

// ---------------------------------------------------------------------------
// YouTube OAuth endpoints
// ---------------------------------------------------------------------------

const DEVICE_CODE_URL: &str = "https://www.youtube.com/o/oauth2/device/code";
const TOKEN_URL: &str = "https://www.youtube.com/o/oauth2/token";
const TV_URL: &str = "https://www.youtube.com/tv";
const TV_USER_AGENT: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version";

// Grant types
const DEVICE_GRANT_TYPE: &str = "http://oauth.net/grant_type/device/1.0";
const REFRESH_GRANT_TYPE: &str = "refresh_token";

// OAuth scope — includes paid content access for Premium
const OAUTH_SCOPE: &str = "http://gdata.youtube.com https://www.googleapis.com/auth/youtube-paid-content";

// ---------------------------------------------------------------------------
// Client ID extraction
// ---------------------------------------------------------------------------

/// Scrape youtube.com/tv to extract the OAuth client_id and client_secret.
///
/// YouTube.js does this by:
/// 1. Fetching /tv to find the base.js script URL
/// 2. Fetching base.js and extracting client_id/secret via regex
async fn extract_client_credentials(
    http: &reqwest::Client,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    // Step 1: Fetch the TV page
    let tv_html = http
        .get(TV_URL)
        .header("User-Agent", TV_USER_AGENT)
        .header("Referer", "https://www.youtube.com/tv")
        .header("Accept-Language", "en-US")
        .send()
        .await?
        .text()
        .await?;

    // Step 2: Find the base.js script URL
    // Pattern: <script id="base-js" src="/s/player/.../tv-player-ias.vflset/tv-player-ias.js" ...>
    // or simply <script ... src="..." ...> where src contains the base JS
    let base_js_url = extract_base_js_url(&tv_html)?;

    let full_url = base_js_full_url(&base_js_url)?;

    tracing::debug!("[oauth] fetching base.js from {}", full_url);

    // Step 3: Fetch base.js
    let base_js = http
        .get(&full_url)
        .header("User-Agent", TV_USER_AGENT)
        .header("Referer", "https://www.youtube.com/tv")
        .send()
        .await?
        .text()
        .await?;

    // Step 4: Extract client_id and client_secret
    // YouTube.js regex: clientId:"(?<client_id>[^"]+)",[^"]*?:"(?<client_secret>[^"]+)"
    extract_credentials_from_js(&base_js)
}

/// Absolute base.js URL to fetch; relative ones are on www.youtube.com.
///
/// An absolute URL comes from the /tv page and is only fetched from a Google
/// host; errors and logs name the host only.
fn base_js_full_url(base_js_url: &str) -> Result<String, String> {
    if base_js_url.starts_with("http") {
        if crate::sabr::stream::is_trusted_google_host(base_js_url) {
            return Ok(base_js_url.to_string());
        }
        let host = reqwest::Url::parse(base_js_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "<unparseable>".to_owned());
        tracing::warn!("[oauth] refusing base.js on untrusted host {}", host);
        Err(format!("base.js URL on untrusted host {}", host))
    } else {
        Ok(format!("https://www.youtube.com{}", base_js_url))
    }
}

fn extract_base_js_url(html: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Look for: <script id="base-js" src="..." ...>
    // or: <script ... src="...base.js..." ...>
    // YouTube.js pattern: <script\s+id="base-js"\s+src="([^"]+)"
    for line in html.lines() {
        if line.contains("id=\"base-js\"") || line.contains("id='base-js'") {
            if let Some(start) = line.find("src=\"") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('"') {
                    return Ok(rest[..end].to_string());
                }
            }
            if let Some(start) = line.find("src='") {
                let rest = &line[start + 5..];
                if let Some(end) = rest.find('\'') {
                    return Ok(rest[..end].to_string());
                }
            }
        }
    }

    // Fallback: look for any script src that looks like a player JS
    for line in html.lines() {
        if let Some(start) = line.find("src=\"") {
            let rest = &line[start + 5..];
            if let Some(end) = rest.find('"') {
                let url = &rest[..end];
                if url.contains("player") && url.ends_with(".js") {
                    return Ok(url.to_string());
                }
            }
        }
    }

    Err("could not find base.js URL in /tv page".into())
}

fn extract_credentials_from_js(
    js: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    // YouTube.js regex: clientId:"(?<client_id>[^"]+)",[^"]*?:"(?<client_secret>[^"]+)"
    // We search for: clientId:"..." followed shortly by :"..."
    if let Some(cid_pos) = js.find("clientId:\"") {
        let rest = &js[cid_pos + 10..]; // skip 'clientId:"'
        if let Some(cid_end) = rest.find('"') {
            let client_id = rest[..cid_end].to_string();

            // Now find the client_secret — it's the next quoted value after a ":"
            let after_cid = &rest[cid_end + 1..];
            // Look for :"..." pattern within the next ~200 chars
            let search_range = &after_cid[..after_cid.floor_char_boundary(200)];
            if let Some(sec_pos) = search_range.find(":\"") {
                let sec_rest = &search_range[sec_pos + 2..];
                if let Some(sec_end) = sec_rest.find('"') {
                    let client_secret = sec_rest[..sec_end].to_string();

                    if !client_id.is_empty() && !client_secret.is_empty() {
                        tracing::info!("[oauth] extracted client credentials");
                        return Ok((client_id, client_secret));
                    }
                }
            }
        }
    }

    Err("could not extract client_id/client_secret from base.js".into())
}

// ---------------------------------------------------------------------------
// Visitor data generation
// ---------------------------------------------------------------------------

/// Generate a visitor_data protobuf (same as YouTube.js ProtoUtils).
///
/// Structure: VisitorData { id: random 11-char string, timestamp: unix seconds }
/// Encoded as protobuf, then base64url.
pub fn generate_visitor_data() -> String {
    use rand::Rng;

    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();

    // Random 11-character ID
    let id: String = (0..11)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Manual protobuf encoding (field 1 = string, field 2 = int64)
    // This avoids needing a .proto file for this simple message.
    let mut buf = Vec::new();

    // Field 1 (string): wire type 2 (length-delimited), field number 1
    // Tag = (1 << 3) | 2 = 0x0A
    buf.push(0x0A);
    buf.push(id.len() as u8);
    buf.extend_from_slice(id.as_bytes());

    // Field 2 (int64): wire type 0 (varint), field number 2
    // Tag = (2 << 3) | 0 = 0x10
    buf.push(0x10);
    encode_varint(&mut buf, timestamp);

    // Base64url encode (YouTube uses URL-safe with no padding)
    use base64::Engine;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf);

    // URL-encode (YouTube.js does encodeURIComponent)
    encoded
}

fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth flow
// ---------------------------------------------------------------------------

impl OAuthState {
    pub fn new(http: reqwest::Client, credentials: OAuthCredentials) -> Arc<Self> {
        Arc::new(Self {
            credentials: RwLock::new(credentials),
            http,
        })
    }

    /// Get a valid access token, refreshing if expired.
    /// Returns None if not logged in.
    pub async fn get_access_token(&self) -> Option<String> {
        {
            let creds = self.credentials.read().await;
            if !creds.is_logged_in() {
                return None;
            }
            if !creds.is_expired() {
                return Some(creds.access_token.clone());
            }
        }

        // Token expired — refresh it
        match self.refresh_access_token().await {
            Ok(()) => {
                let creds = self.credentials.read().await;
                Some(creds.access_token.clone())
            }
            Err(e) => {
                tracing::error!("[oauth] token refresh failed: {}", e);
                None
            }
        }
    }

    /// Run the full device login flow. Blocks until user completes auth.
    ///
    /// Prints instructions to stdout for the user.
    pub async fn device_login(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Step 1: Get or extract client credentials
        let (client_id, client_secret) = {
            let creds = self.credentials.read().await;
            if !creds.client_id.is_empty() && !creds.client_secret.is_empty() {
                (creds.client_id.clone(), creds.client_secret.clone())
            } else {
                drop(creds);
                tracing::info!("[oauth] extracting client credentials from youtube.com/tv");
                let result = extract_client_credentials(&self.http).await?;
                let mut creds = self.credentials.write().await;
                creds.client_id = result.0.clone();
                creds.client_secret = result.1.clone();
                result
            }
        };

        // Step 2: Request device code
        let device_code_resp: DeviceCodeResponse = self
            .http
            .post(DEVICE_CODE_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "scope": OAUTH_SCOPE,
                "device_id": uuid::Uuid::new_v4().to_string(),
                "device_model": "ytlr::",
            }))
            .send()
            .await?
            .json()
            .await?;

        if let Some(ref err) = device_code_resp.error_code {
            return Err(format!("device code error: {}", err).into());
        }

        // Step 3: Show user instructions
        tracing::info!(
            "[oauth] Go to {} and enter code: {}",
            device_code_resp.verification_url,
            device_code_resp.user_code,
        );
        // Also print to stdout so it's visible even without RUST_LOG
        println!();
        println!("=== YouTube Login ===");
        println!("Go to: {}", device_code_resp.verification_url);
        println!("Enter code: {}", device_code_resp.user_code);
        println!("Waiting for authorization...");
        println!();

        // Step 4: Poll for tokens
        let poll_interval = std::time::Duration::from_secs(device_code_resp.interval.max(5));

        loop {
            tokio::time::sleep(poll_interval).await;

            let resp: TokenResponse = self
                .http
                .post(TOKEN_URL)
                .json(&serde_json::json!({
                    "client_id": client_id,
                    "client_secret": client_secret,
                    "code": device_code_resp.device_code,
                    "grant_type": DEVICE_GRANT_TYPE,
                }))
                .send()
                .await?
                .json()
                .await?;

            if let Some(ref err) = resp.error {
                match err.as_str() {
                    "authorization_pending" => {
                        tracing::debug!("[oauth] waiting for user authorization...");
                        continue;
                    }
                    "slow_down" => {
                        tracing::debug!("[oauth] rate limited, slowing down");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    "access_denied" => {
                        return Err("user denied authorization".into());
                    }
                    "expired_token" => {
                        return Err("device code expired — please try again".into());
                    }
                    other => {
                        return Err(format!("OAuth error: {}", other).into());
                    }
                }
            }

            // Success — we have tokens
            if let (Some(access_token), Some(refresh_token)) =
                (resp.access_token, resp.refresh_token)
            {
                let expiry = chrono::Utc::now()
                    + chrono::Duration::seconds(resp.expires_in.unwrap_or(3600) as i64);

                let mut creds = self.credentials.write().await;
                creds.access_token = access_token;
                creds.refresh_token = refresh_token;
                creds.expiry_date = expiry.to_rfc3339();

                tracing::info!("[oauth] login successful! Token expires at {}", creds.expiry_date);
                println!("Login successful!");
                return Ok(());
            }
        }
    }

    /// Refresh the access token using the stored refresh token.
    async fn refresh_access_token(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (client_id, client_secret, refresh_token) = {
            let creds = self.credentials.read().await;
            (
                creds.client_id.clone(),
                creds.client_secret.clone(),
                creds.refresh_token.clone(),
            )
        };

        tracing::info!("[oauth] refreshing access token...");

        let resp: TokenResponse = self
            .http
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
                "refresh_token": refresh_token,
                "grant_type": REFRESH_GRANT_TYPE,
            }))
            .send()
            .await?
            .json()
            .await?;

        if let Some(ref err) = resp.error {
            return Err(format!("token refresh failed: {}", err).into());
        }

        if let Some(access_token) = resp.access_token {
            let expiry = chrono::Utc::now()
                + chrono::Duration::seconds(resp.expires_in.unwrap_or(3600) as i64);

            let mut creds = self.credentials.write().await;
            creds.access_token = access_token;
            creds.expiry_date = expiry.to_rfc3339();

            tracing::info!("[oauth] token refreshed, expires at {}", creds.expiry_date);
            Ok(())
        } else {
            Err("refresh response missing access_token".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

/// Path for OAuth credentials: `$YTCAST_DATA_DIR/ytcast-oauth.json`, else
/// `~/.config/ytcast-oauth.json`.
pub fn credentials_path() -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    credentials_path_for(crate::storage::data_dir_env().as_deref())
}

fn credentials_path_for(
    data_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let config_dir =
        crate::storage::config_dir(data_dir).ok_or("could not determine config directory")?;
    Ok(config_dir.join("ytcast-oauth.json"))
}

/// Load OAuth credentials from disk.
pub fn load_credentials() -> OAuthCredentials {
    match credentials_path() {
        Ok(p) => load_credentials_from(&p),
        Err(_) => OAuthCredentials::default(),
    }
}

fn load_credentials_from(path: &std::path::Path) -> OAuthCredentials {
    match std::fs::read_to_string(path) {
        Ok(data) => {
            crate::storage::warn_if_loose_permissions(path);
            match serde_json::from_str(&data) {
                Ok(creds) => {
                    tracing::info!("[oauth] loaded credentials from {}", path.display());
                    creds
                }
                // serde_json errors carry line/column only, never the input.
                Err(e) => {
                    tracing::warn!("[oauth] failed to parse {}: {}", path.display(), e);
                    OAuthCredentials::default()
                }
            }
        }
        Err(_) => {
            tracing::info!("[oauth] no credentials at {}", path.display());
            OAuthCredentials::default()
        }
    }
}

/// Save OAuth credentials to disk (atomic write, private 0600 file).
pub fn save_credentials(
    creds: &OAuthCredentials,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    save_credentials_to(creds, &credentials_path()?)
}

fn save_credentials_to(
    creds: &OAuthCredentials,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let json = serde_json::to_string_pretty(creds)?;
    crate::storage::write_private_atomic(path, json.as_bytes())?;
    tracing::info!("[oauth] saved credentials to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::*;

    fn sample() -> OAuthCredentials {
        OAuthCredentials {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            refresh_token: "REFRESH-TOKEN-VALUE".into(),
            access_token: "ACCESS-TOKEN-VALUE".into(),
            expiry_date: "2030-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn credential_extraction_logs_no_credential_fragments() {
        let js = r#"x={clientId:"CLIENT-ID-1234567890-abcdef.apps.example",y:"CLIENT-SECRET-xyz"}"#;
        let mut extracted = None;
        let logs = capture_logs(|| extracted = Some(extract_credentials_from_js(js).unwrap()));
        let (client_id, client_secret) = extracted.unwrap();
        assert_eq!(client_id, "CLIENT-ID-1234567890-abcdef.apps.example");
        assert_eq!(client_secret, "CLIENT-SECRET-xyz");
        assert!(logs.contains("extracted client credentials"), "{logs}");
        assert!(!logs.contains("CLIENT-ID"), "{logs}");
        assert!(!logs.contains("CLIENT-SECRET"), "{logs}");
    }

    #[test]
    fn credential_extraction_survives_multibyte_char_at_search_cut() {
        // The secret search window ends at byte 200 after the client_id's
        // closing quote; put a 3-byte char across that cut.
        let after_cid = format!(",y:\"SEC\"{}\u{20ac}rest", "a".repeat(191));
        assert!(!after_cid.is_char_boundary(200));
        let js = format!("x={{clientId:\"ID\"{after_cid}}}");
        let (client_id, client_secret) = extract_credentials_from_js(&js).unwrap();
        assert_eq!((client_id.as_str(), client_secret.as_str()), ("ID", "SEC"));
    }

    #[test]
    fn base_js_url_must_be_on_a_google_host() {
        assert_eq!(
            base_js_full_url("/s/player/abc/base.js").unwrap(),
            "https://www.youtube.com/s/player/abc/base.js"
        );
        assert_eq!(
            base_js_full_url("https://www.youtube.com/s/player/abc/base.js").unwrap(),
            "https://www.youtube.com/s/player/abc/base.js"
        );
        for url in ["https://evil.example/s/player/abc/base.js?k=SECRET", "http://www.youtube.com/base.js"] {
            let err = base_js_full_url(url).unwrap_err();
            assert!(!err.contains("SECRET") && !err.contains("/s/player"), "{err}");
        }
        let mut result = None;
        let logs = capture_logs(|| {
            result = Some(base_js_full_url("https://evil.example/s/player/abc/base.js?k=SECRET"))
        });
        assert!(result.unwrap().unwrap_err().contains("evil.example"));
        assert!(logs.contains("evil.example"), "{logs}");
        assert!(!logs.contains("SECRET"), "{logs}");
    }

    // Needles are split with concat! so this test does not match itself.
    #[test]
    fn credential_log_formats_are_gone() {
        let src = include_str!("oauth.rs");
        for needle in [
            concat!("extracted client_id", "={}"),
            concat!("secret", "={}..."),
            concat!("&client_secret[..", "client_secret.len()"),
            concat!("info!(\"[oauth] fetching base.js", " from"),
        ] {
            assert!(!src.contains(needle), "credential-logging format string found: {needle}");
        }
    }

    #[test]
    fn path_unchanged_without_data_dir() {
        assert_eq!(
            credentials_path_for(None).unwrap(),
            dirs::config_dir().unwrap().join("ytcast-oauth.json")
        );
    }

    #[test]
    fn saved_under_data_dir_privately_and_read_back() {
        let tmp = TempDir::new("oauth-roundtrip");
        let path = credentials_path_for(Some(tmp.path())).unwrap();
        assert_eq!(path, tmp.path().join("ytcast-oauth.json"));

        save_credentials_to(&sample(), &path).unwrap();
        let loaded = load_credentials_from(&path);
        assert_eq!(loaded.refresh_token, "REFRESH-TOKEN-VALUE");
        assert_eq!(loaded.access_token, "ACCESS-TOKEN-VALUE");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn save_and_load_never_log_token_values() {
        let tmp = TempDir::new("oauth-nolog");
        let path = tmp.path().join("ytcast-oauth.json");
        let logs = capture_logs(|| {
            save_credentials_to(&sample(), &path).unwrap();
            load_credentials_from(&path);
        });
        assert!(!logs.contains("TOKEN-VALUE"), "{logs}");
        assert!(!logs.contains("csecret"), "{logs}");
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_loose_file_makes_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("oauth-overwrite");
        let path = tmp.path().join("ytcast-oauth.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_credentials_to(&sample(), &path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn loading_loose_file_warns_with_path_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("oauth-loose");
        let path = tmp.path().join("ytcast-oauth.json");
        std::fs::write(&path, serde_json::to_string(&sample()).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut loaded = OAuthCredentials::default();
        let logs = capture_logs(|| loaded = load_credentials_from(&path));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(path.to_str().unwrap()), "{logs}");
        assert!(!logs.contains("TOKEN-VALUE"), "{logs}");
        // Loose permissions warn but do not refuse.
        assert_eq!(loaded.refresh_token, "REFRESH-TOKEN-VALUE");
    }
}
