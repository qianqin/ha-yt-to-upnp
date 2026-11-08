//! Anonymous YouTube Music session extraction.
//!
//! Fetches the public music.youtube.com page (no cookies, no account) and
//! extracts the live INNERTUBE_CONTEXT from ytcfg, plus the player version
//! (base.js) for the signature timestamp and nsig decoding.
//!
//! yt-dlp does the same thing: it downloads the page first to get the full
//! client context (22+ fields) and visitor data. Without this, WEB_REMIX
//! /player returns UNPLAYABLE even with the cast's credential transfer
//! token (ctt), which is the only authentication this receiver uses.

const MUSIC_ORIGIN: &str = "https://music.youtube.com";
const PLAYER_ORIGIN: &str = "https://www.youtube.com";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// Live session data extracted from the public music.youtube.com page.
pub struct YtMusicSession {
    /// Full INNERTUBE_CONTEXT from ytcfg (22+ client fields, incl. visitorData).
    pub innertube_context: serde_json::Value,
    /// Signature timestamp (ytcfg STS or base.js), sent in playbackContext.
    pub signature_timestamp: Option<u64>,
    /// Raw base.js player JavaScript (for nsig solving).
    pub player_js: Option<String>,
    /// Player hash (base.js version identifier, e.g. "4e67f8a0").
    pub player_hash: Option<String>,
}

impl YtMusicSession {
    /// The session's INNERTUBE_CONTEXT with the cast's ctt (if any) added to
    /// `context.user.credentialTransferTokens`.
    pub fn player_context(&self, ctt: Option<&str>) -> serde_json::Value {
        let mut ctx = self.innertube_context.clone();
        if let Some(token) = ctt {
            if let Some(obj) = ctx.as_object_mut() {
                let user = obj.entry("user").or_insert(serde_json::json!({}));
                if let Some(user) = user.as_object_mut() {
                    user.insert(
                        "credentialTransferTokens".to_owned(),
                        serde_json::json!([{ "scope": "VIDEO", "token": token }]),
                    );
                }
            }
        }
        ctx
    }

    /// WEB_REMIX client version reported by the page (changes frequently).
    pub fn client_version(&self) -> Option<&str> {
        self.innertube_context["client"]["clientVersion"].as_str()
    }

    /// Anonymous visitor id issued with the page.
    pub fn visitor_data(&self) -> Option<&str> {
        self.innertube_context["client"]["visitorData"].as_str()
    }
}

/// Fetch the public music.youtube.com page anonymously and extract the session.
/// Returns `None` (after logging why) when the page cannot be used.
pub async fn fetch_music_session(http: &reqwest::Client) -> Option<YtMusicSession> {
    fetch_session_from(http, &format!("{MUSIC_ORIGIN}/"), PLAYER_ORIGIN).await
}

async fn fetch_session_from(
    http: &reqwest::Client,
    page_url: &str,
    player_origin: &str,
) -> Option<YtMusicSession> {
    let resp = match http
        .get(page_url)
        .header("User-Agent", BROWSER_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[ytcfg] music.youtube.com fetch failed: {}", e.without_url());
            return None;
        }
    };

    if !resp.status().is_success() {
        tracing::warn!("[ytcfg] music.youtube.com returned HTTP {}", resp.status());
        return None;
    }
    // Without a consent cookie, EU visitors are redirected to a consent page.
    if resp.url().host_str().is_some_and(|h| h.starts_with("consent.")) {
        tracing::warn!("[ytcfg] music.youtube.com redirected to the consent page");
        return None;
    }

    let page = resp.text().await.ok()?;
    let Some(config) = parse_music_page(&page) else {
        tracing::warn!(
            "[ytcfg] could not extract INNERTUBE_CONTEXT from page ({}B)",
            page.len()
        );
        return None;
    };

    let mut signature_timestamp = config.signature_timestamp;
    let mut player_js = None;
    match config.player_hash.as_deref() {
        Some(hash) => {
            if let Some(js) = fetch_player_js(http, player_origin, hash).await {
                signature_timestamp = signature_timestamp.or_else(|| extract_sts(&js));
                player_js = Some(js);
            }
        }
        None => tracing::warn!("[ytcfg] could not find player hash in page HTML"),
    }

    let session = YtMusicSession {
        innertube_context: config.innertube_context,
        signature_timestamp,
        player_js,
        player_hash: config.player_hash,
    };
    tracing::info!(
        "[ytcfg] anonymous session extracted: clientVersion={}, visitorData={}, sts={:?}, player_js={}",
        session.client_version().unwrap_or("?"),
        session.visitor_data().is_some(),
        session.signature_timestamp,
        session.player_js.is_some(),
    );
    Some(session)
}

/// Download base.js for `hash`. Logs and returns `None` on failure.
async fn fetch_player_js(http: &reqwest::Client, origin: &str, hash: &str) -> Option<String> {
    let url = format!("{origin}/s/player/{hash}/player_ias.vflset/en_US/base.js");
    tracing::info!("[ytcfg] fetching base.js for player hash={}", hash);

    let resp = match http
        .get(&url)
        .header("User-Agent", BROWSER_UA)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[ytcfg] base.js fetch failed: {}", e.without_url());
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!("[ytcfg] base.js returned HTTP {}", resp.status());
        return None;
    }
    let js = resp.text().await.ok()?;
    tracing::info!("[ytcfg] base.js size: {}B", js.len());
    Some(js)
}

/// What the music.youtube.com page itself tells us.
struct PageConfig {
    innertube_context: serde_json::Value,
    signature_timestamp: Option<u64>,
    player_hash: Option<String>,
}

/// Extract the ytcfg INNERTUBE_CONTEXT (with yt-dlp's standard additions),
/// STS and the base.js player hash from the page HTML.
fn parse_music_page(page: &str) -> Option<PageConfig> {
    let mut ctx = extract_json_field(page, "INNERTUBE_CONTEXT")?;
    if let Some(client) = ctx.get_mut("client").and_then(|c| c.as_object_mut()) {
        client.entry("hl").or_insert(serde_json::json!("en"));
        client.entry("timeZone").or_insert(serde_json::json!("UTC"));
        client.entry("utcOffsetMinutes").or_insert(serde_json::json!(0));
    }
    if let Some(obj) = ctx.as_object_mut() {
        obj.entry("user").or_insert(serde_json::json!({"lockedSafetyMode": false}));
        obj.entry("request").or_insert(serde_json::json!({"useSsl": true}));
    }
    Some(PageConfig {
        innertube_context: ctx,
        signature_timestamp: extract_number_field(page, "STS"),
        player_hash: extract_player_hash(page),
    })
}

/// Player hash from the first `/s/player/HASH/` reference in the page.
fn extract_player_hash(page: &str) -> Option<String> {
    page.split("/s/player/").skip(1).find_map(|part| {
        let hash: String = part.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        (hash.len() >= 6).then_some(hash)
    })
}

/// `signatureTimestamp:NNNNN` from base.js.
fn extract_sts(js: &str) -> Option<u64> {
    for part in js.split("signatureTimestamp").skip(1) {
        let Some(rest) = part.trim_start().strip_prefix(':') else { continue };
        let num: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
        if num.len() == 5 {
            if let Ok(val) = num.parse::<u64>() {
                tracing::info!("[ytcfg] extracted STS={} from base.js", val);
                return Some(val);
            }
        }
    }
    tracing::warn!("[ytcfg] could not find signatureTimestamp in base.js");
    None
}

// --- Internal helpers ---

/// Extract a JSON object field from ytcfg in the page HTML.
/// Looks for `"FIELD_NAME":{...}` patterns in ytcfg.set() blocks.
fn extract_json_field(page: &str, field: &str) -> Option<serde_json::Value> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split("ytcfg.set(") {
        let Some(field_pos) = chunk.find(&pattern) else { continue };
        let after_key = &chunk[field_pos + pattern.len()..];
        let Some(after_colon) = after_key.trim_start().strip_prefix(':') else { continue };
        if let Some(json_str) = find_matching_brace(after_colon.trim_start()) {
            if let Ok(val) = serde_json::from_str(json_str) {
                return Some(val);
            }
        }
    }
    None
}

/// Extract a numeric field from ytcfg: `"FIELD":12345`.
fn extract_number_field(page: &str, field: &str) -> Option<u64> {
    let pattern = format!("\"{}\"", field);
    for chunk in page.split(&pattern).skip(1) {
        let rest = chunk.trim_start();
        if let Some(rest) = rest.strip_prefix(':') {
            let num_str: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
            if !num_str.is_empty() {
                return num_str.parse().ok();
            }
        }
    }
    None
}

/// Find the substring containing a balanced JSON object starting with `{`.
fn find_matching_brace(s: &str) -> Option<&str> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    /// A trimmed-down anonymous music.youtube.com page: two ytcfg.set blocks
    /// (the context lives in the second one, as on the real page) and a
    /// base.js reference.
    pub const MUSIC_PAGE: &str = r#"<!DOCTYPE html><html><head>
<script nonce="x">ytcfg.set({"EXPERIMENT_FLAGS":{"a":true},"HL":"en"});</script>
<script nonce="x">ytcfg.set({"INNERTUBE_API_VERSION":"v1","INNERTUBE_CONTEXT":{"client":{"hl":"de","gl":"DE","visitorData":"CgtWSVNJVE9SLUlE","userAgent":"Mozilla/5.0","clientName":"WEB_REMIX","clientVersion":"1.20261102.01.00","osName":"Windows","platform":"DESKTOP","originalUrl":"https://music.youtube.com/","configInfo":{"appInstallData":"x{y}\"z"}},"user":{"lockedSafetyMode":false},"request":{"useSsl":true}},"LOGGED_IN":false,"STS":20391,"VISITOR_DATA":"CgtWSVNJVE9SLUlE"});</script>
<script src="/s/player/4e67f8a0/player_ias.vflset/en_US/base.js" nonce="x"></script>
</head><body></body></html>"#;

    pub fn session() -> super::YtMusicSession {
        let config = super::parse_music_page(MUSIC_PAGE).unwrap();
        super::YtMusicSession {
            innertube_context: config.innertube_context,
            signature_timestamp: config.signature_timestamp,
            player_js: Some("var player;".to_owned()),
            player_hash: config.player_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::MUSIC_PAGE;
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn parses_anonymous_page() {
        let config = parse_music_page(MUSIC_PAGE).unwrap();
        let client = &config.innertube_context["client"];
        assert_eq!(client["clientName"], "WEB_REMIX");
        assert_eq!(client["clientVersion"], "1.20261102.01.00");
        assert_eq!(client["visitorData"], "CgtWSVNJVE9SLUlE");
        // Page values win over yt-dlp's defaults; missing ones are added.
        assert_eq!(client["hl"], "de");
        assert_eq!(client["timeZone"], "UTC");
        assert_eq!(client["configInfo"]["appInstallData"], "x{y}\"z");
        assert_eq!(config.innertube_context["user"]["lockedSafetyMode"], false);
        assert_eq!(config.signature_timestamp, Some(20391));
        assert_eq!(config.player_hash.as_deref(), Some("4e67f8a0"));
    }

    #[test]
    fn page_without_context_is_rejected() {
        assert!(parse_music_page("<html>Your browser is deprecated</html>").is_none());
    }

    #[test]
    fn missing_user_object_gets_defaults() {
        let page = r#"ytcfg.set({"INNERTUBE_CONTEXT":{"client":{"clientName":"WEB_REMIX"}}});"#;
        let config = parse_music_page(page).unwrap();
        assert_eq!(config.innertube_context["user"]["lockedSafetyMode"], false);
        assert_eq!(config.innertube_context["request"]["useSsl"], true);
        assert_eq!(config.signature_timestamp, None);
        assert_eq!(config.player_hash, None);
    }

    #[test]
    fn sts_from_player_js() {
        assert_eq!(extract_sts("a=1;signatureTimestamp:20391,b"), Some(20391));
        assert_eq!(extract_sts("signatureTimestamp: 123"), None);
        assert_eq!(extract_sts("nothing here"), None);
    }

    #[test]
    fn player_context_carries_ctt_in_user() {
        let session = super::test_fixtures::session();
        let ctx = session.player_context(Some("cast-token"));
        assert_eq!(
            ctx["user"]["credentialTransferTokens"],
            serde_json::json!([{ "scope": "VIDEO", "token": "cast-token" }])
        );
        assert_eq!(ctx["user"]["lockedSafetyMode"], false);
        assert_eq!(ctx["client"]["visitorData"], "CgtWSVNJVE9SLUlE");
        assert!(session.player_context(None)["user"].get("credentialTransferTokens").is_none());
    }

    /// Serves `page` at `/` and `base_js` under `/s/player/`, recording every
    /// raw request (lowercased).
    async fn spawn_fake_youtube(page: String, base_js: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let req = String::from_utf8_lossy(&req).to_ascii_lowercase();
                let body = if req.starts_with("get /s/player/") { base_js } else { page.as_str() };
                record.lock().unwrap().push(req);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (origin, seen)
    }

    #[tokio::test]
    async fn anonymous_fetch_sends_no_credentials_and_keeps_player_js() {
        let (origin, seen) = spawn_fake_youtube(MUSIC_PAGE.to_owned(), "var p;signatureTimestamp:20391;").await;
        let http = reqwest::Client::new();

        let session = fetch_session_from(&http, &format!("{origin}/"), &origin).await.unwrap();

        assert_eq!(session.player_hash.as_deref(), Some("4e67f8a0"));
        assert_eq!(session.player_js.as_deref(), Some("var p;signatureTimestamp:20391;"));
        assert_eq!(session.signature_timestamp, Some(20391));
        assert_eq!(session.visitor_data(), Some("CgtWSVNJVE9SLUlE"));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1].starts_with("get /s/player/4e67f8a0/player_ias.vflset/en_us/base.js"), "{seen:?}");
        for req in &seen {
            assert!(!req.contains("\r\ncookie:"), "{req}");
            assert!(!req.contains("\r\nauthorization:"), "{req}");
        }
    }

    #[tokio::test]
    async fn sts_falls_back_to_player_js_when_page_has_none() {
        let page = MUSIC_PAGE.replace("\"STS\":20391,", "");
        let (origin, _seen) = spawn_fake_youtube(page, "signatureTimestamp:20400").await;
        let http = reqwest::Client::new();

        let session = fetch_session_from(&http, &format!("{origin}/"), &origin).await.unwrap();

        assert_eq!(session.signature_timestamp, Some(20400));
    }

    #[tokio::test]
    async fn unusable_page_yields_no_session() {
        let (origin, _seen) = spawn_fake_youtube("<html>deprecated</html>".to_owned(), "").await;
        let http = reqwest::Client::new();
        assert!(fetch_session_from(&http, &format!("{origin}/"), &origin).await.is_none());
    }
}
