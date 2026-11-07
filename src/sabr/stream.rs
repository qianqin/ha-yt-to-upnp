//! Core SABR streaming implementation.
//!
//! Handles the complete SABR audio streaming flow:
//!   InnerTube /player call -> format selection -> SABR request loop ->
//!   UMP response parsing -> audio byte extraction.
//!
//! Called from an axum HTTP handler. Takes a video ID and returns a stream
//! of audio bytes. The caller pipes these bytes as an HTTP response to MPD.

use std::collections::HashMap;

use bytes::Bytes;
use futures_util::StreamExt;
use prost::Message;
use tokio::sync::mpsc;

use super::proto::misc;
use super::proto::vs;
use super::ump::{self, UmpParser};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Metadata about the audio stream being served.
pub struct SabrStreamInfo {
    pub title: String,
    pub duration_secs: u64,
    pub mime_type: String,
}

/// Error type for SABR streaming.
pub type SabrError = Box<dyn std::error::Error + Send + Sync>;

// ---------------------------------------------------------------------------
// InnerTube client configuration
// ---------------------------------------------------------------------------

struct InnerTubeClient {
    client_name: &'static str,
    client_version: &'static str,
    device_make: &'static str,
    device_model: &'static str,
    os_name: &'static str,
    os_version: &'static str,
    user_agent: &'static str,
    client_name_id: i32,
    android_sdk_version: Option<i32>,
}

const IOS_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "IOS",
    client_version: "21.02.3",
    device_make: "Apple",
    device_model: "iPhone16,2",
    os_name: "iPhone",
    os_version: "18.1.0.22B83",
    user_agent: "com.google.ios.youtube/21.02.3 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)",
    client_name_id: 5,
    android_sdk_version: None,
};

const ANDROID_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "ANDROID",
    client_version: "19.44.38",
    device_make: "Google",
    device_model: "Pixel 8",
    os_name: "Android",
    os_version: "14",
    user_agent: "com.google.android.youtube/19.44.38 (Linux; U; Android 14; en_US; Pixel 8) gzip",
    client_name_id: 3,
    android_sdk_version: Some(34),
};

/// YouTube Music web client. Used when ctt (credential transfer token) is
/// available from a cast session, since IOS/ANDROID reject the ctt with HTTP 400.
const WEB_REMIX_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "WEB_REMIX",
    client_version: "1.20260114.03.00",
    device_make: "",
    device_model: "",
    os_name: "",
    os_version: "",
    user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    client_name_id: 67,
    android_sdk_version: None,
};

/// Regular YouTube web client. Used with GVS PoToken to bypass 60s throttle.
/// This is what yt-dlp uses for SABR streaming.
const WEB_CLIENT: InnerTubeClient = InnerTubeClient {
    client_name: "WEB",
    client_version: "2.20241126.01.00",
    device_make: "",
    device_model: "",
    os_name: "",
    os_version: "",
    user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    client_name_id: 1,
    android_sdk_version: None,
};

// ---------------------------------------------------------------------------
// PoToken (Proof of Origin) — fetched from bgutil-pot sidecar
// ---------------------------------------------------------------------------

/// GVS PoToken + visitor_data pair.
/// The GVS token is content-bound (to video_id) per YouTube's 2026 experiment.
/// visitor_data is generated locally (protobuf) to establish a session context.
struct GvsToken {
    po_token: String,
    visitor_data: String,
}

/// Generate a visitor_data string like YouTube.js does:
/// protobuf { field1: random_11_chars, field5: unix_timestamp }
/// then base64url-encode it.
fn generate_visitor_data() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let chars: Vec<u8> = (0..11)
        .map(|_| {
            let idx = rng.gen_range(0..62u8);
            match idx {
                0..=25 => b'A' + idx,
                26..=51 => b'a' + (idx - 26),
                _ => b'0' + (idx - 52),
            }
        })
        .collect();
    let random_id = String::from_utf8(chars).unwrap();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    // Manual protobuf encoding:
    // field 1 (string) = tag 0x0a, then varint length, then bytes
    // field 5 (int64)  = tag 0x28, then varint value
    let mut buf = Vec::with_capacity(32);
    // field 1: tag = (1 << 3) | 2 = 0x0a
    buf.push(0x0a);
    buf.push(random_id.len() as u8);
    buf.extend_from_slice(random_id.as_bytes());
    // field 5: tag = (5 << 3) | 0 = 0x28
    buf.push(0x28);
    // varint encode timestamp
    let mut val = timestamp as u64;
    loop {
        let byte = (val & 0x7f) as u8;
        val >>= 7;
        if val == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }

    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

/// Fetch a PoToken from the bgutil-pot HTTP server (localhost:4416).
///
/// `content_binding` controls what the token is bound to:
///   - `Some(video_id)` → content-bound (for WEB/MWEB clients)
///   - `None` / empty → session-bound / visitor_data-bound (for WEB_REMIX)
async fn fetch_pot_token(http: &reqwest::Client, content_binding: Option<&str>) -> Option<GvsToken> {
    let body = match content_binding {
        Some(cb) if !cb.is_empty() => serde_json::json!({"content_binding": cb}),
        _ => serde_json::json!({}),
    };

    let resp = http
        .post("http://127.0.0.1:4416/get_pot")
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("[sabr] bgutil-pot returned HTTP {}", resp.status());
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let po_token = json["poToken"].as_str()?.to_owned();

    // Generate visitor_data locally (like YouTube.js does) instead of using
    // bgutil-pot's contentBinding field, which contains the video ID — NOT valid visitor_data.
    let visitor_data = generate_visitor_data();

    tracing::info!(
        "[sabr] got PoToken (len={}) binding={}, visitor_data len={}",
        po_token.len(),
        content_binding.unwrap_or("<session>"),
        visitor_data.len(),
    );
    Some(GvsToken { po_token, visitor_data })
}

// ---------------------------------------------------------------------------
// InnerTube /player response data
// ---------------------------------------------------------------------------

struct PlayerData {
    server_abr_streaming_url: String,
    ustreamer_config: Vec<u8>,
    formats: Vec<AdaptiveFormat>,
    title: String,
    duration_secs: u64,
}

// ---------------------------------------------------------------------------
// Credential host check
// ---------------------------------------------------------------------------

/// True only for https URLs on youtube.com / googlevideo.com or a subdomain.
///
/// Streaming URLs come from server responses (player response, SABR
/// redirects), so cookies and auth headers must never follow them blindly.
pub(crate) fn is_trusted_google_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    // domain() is None for IP literals.
    let Some(host) = parsed.domain() else {
        return false;
    };
    ["youtube.com", "googlevideo.com"].iter().any(|trusted| {
        host == *trusted
            || host.strip_suffix(trusted).is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// Host part of a URL for log/error messages (never the full URL: query
/// strings carry tokens).
pub(crate) fn host_for_log(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unparseable>".to_owned())
}

/// Fail closed: a SABR request body carries the PoToken and playback cookie,
/// so an off-Google streaming URL is refused outright.
fn check_streaming_url(url: &str) -> Result<(), SabrError> {
    if is_trusted_google_host(url) {
        return Ok(());
    }
    let host = host_for_log(url);
    tracing::warn!("[sabr] refusing untrusted streaming host {}", host);
    Err(format!("untrusted streaming host {}", host).into())
}

/// Client for credentialed YouTube requests. The default reqwest policy
/// follows up to 10 redirects to any host and re-sends POST bodies on
/// 307/308; this one only follows redirects that stay on Google hosts.
fn credential_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if is_trusted_google_host(attempt.url().as_str()) {
                attempt.follow()
            } else {
                tracing::warn!("[sabr] not following redirect to {}", host_for_log(attempt.url().as_str()));
                attempt.stop()
            }
        });
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0")
            .redirect(policy)
            .build()
            .expect("static reqwest client config is valid")
    })
}

#[derive(Debug, Clone)]
struct AdaptiveFormat {
    itag: i32,
    last_modified: u64,
    xtags: Option<String>,
    mime_type: String,
    bitrate: u64,
}

// ---------------------------------------------------------------------------
// InnerTube /player call
// ---------------------------------------------------------------------------

/// Build a minimal InnerTube context from hardcoded client constants.
/// Used when no YtMusicSession is available (IOS/ANDROID/WEB, or WEB_REMIX without cookies).
fn build_minimal_context(
    itclient: &InnerTubeClient,
    ctt: Option<&str>,
    visitor_data: Option<&str>,
) -> serde_json::Value {
    let mut client_obj = serde_json::json!({
        "clientName": itclient.client_name,
        "clientVersion": itclient.client_version,
        "hl": "en",
        "gl": "US",
        "timeZone": "UTC",
        "utcOffsetMinutes": 0,
    });

    if let Some(vd) = visitor_data {
        if !vd.is_empty() {
            client_obj["visitorData"] = serde_json::json!(vd);
        }
    }

    if !itclient.device_make.is_empty() {
        client_obj["deviceMake"] = serde_json::json!(itclient.device_make);
        client_obj["deviceModel"] = serde_json::json!(itclient.device_model);
        client_obj["osName"] = serde_json::json!(itclient.os_name);
        client_obj["osVersion"] = serde_json::json!(itclient.os_version);
    }

    if let Some(sdk) = itclient.android_sdk_version {
        client_obj["androidSdkVersion"] = serde_json::json!(sdk);
    }

    let mut context = serde_json::json!({ "client": client_obj });

    if let Some(token) = ctt {
        context["user"] = serde_json::json!({
            "credentialTransferTokens": [{
                "scope": "VIDEO",
                "token": token,
            }]
        });
    }

    context
}

async fn innertube_player(
    http: &reqwest::Client,
    video_id: &str,
    itclient: &InnerTubeClient,
    ctt: Option<&str>,
    visitor_data: Option<&str>,
    po_token: Option<&str>,
    yt_cookies: Option<&crate::cookies::YtCookies>,
    yt_session: Option<&crate::cookies::YtMusicSession>,
    oauth_token: Option<&str>,
) -> Result<PlayerData, SabrError> {
    let is_web_remix = itclient.client_name == "WEB_REMIX";

    // --- Build context ---
    // When we have a live YtMusicSession (from fetching music.youtube.com),
    // use the full 22+ field INNERTUBE_CONTEXT. Without this, WEB_REMIX /player
    // returns UNPLAYABLE. yt-dlp always fetches the page first for this reason.
    let (context, sig_timestamp) = if is_web_remix {
        if let Some(session) = yt_session {
            let mut ctx = session.innertube_context.clone();
            // Merge ctt credentials into the existing user object
            if let Some(token) = ctt {
                if let Some(user) = ctx.get_mut("user").and_then(|u| u.as_object_mut()) {
                    user.insert(
                        "credentialTransferTokens".to_owned(),
                        serde_json::json!([{
                            "scope": "VIDEO",
                            "token": token,
                        }]),
                    );
                }
            }
            (ctx, session.signature_timestamp)
        } else {
            // No session — build minimal context (may fail with UNPLAYABLE)
            (build_minimal_context(itclient, ctt, visitor_data), None)
        }
    } else {
        // Non-WEB_REMIX (IOS, ANDROID, WEB): always use minimal context
        (build_minimal_context(itclient, ctt, visitor_data), None)
    };

    // --- Build playbackContext ---
    let mut playback_ctx = serde_json::json!({
        "contentPlaybackContext": {
            "html5Preference": "HTML5_PREF_WANTS",
        }
    });
    if let Some(sts) = sig_timestamp {
        playback_ctx["contentPlaybackContext"]["signatureTimestamp"] = serde_json::json!(sts);
    }

    // --- Build request body ---
    let mut body = serde_json::json!({
        "videoId": video_id,
        "context": context,
        "contentCheckOk": true,
        "racyCheckOk": true,
        "playbackContext": playback_ctx,
    });

    if let Some(token) = po_token {
        body["serviceIntegrityDimensions"] = serde_json::json!({
            "poToken": token,
        });
    }

    // --- URL ---
    // Authenticated WEB_REMIX doesn't send API key (matches yt-dlp).
    let url = if is_web_remix {
        if yt_cookies.is_some() {
            "https://music.youtube.com/youtubei/v1/player?prettyPrint=false"
        } else {
            "https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
        }
    } else {
        "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
    };
    debug_assert!(is_trusted_google_host(url));

    // Client version from live session (changes frequently) or hardcoded constant.
    let client_version = if let Some(session) = yt_session {
        session.innertube_context.get("client")
            .and_then(|c| c.get("clientVersion"))
            .and_then(|v| v.as_str())
            .unwrap_or(itclient.client_version)
    } else {
        itclient.client_version
    };

    tracing::info!(
        "[sabr] innertube /player for {} using {} v{}{}{}{}{}{}",
        video_id,
        itclient.client_name,
        client_version,
        if ctt.is_some() { " +ctt" } else { "" },
        if po_token.is_some() { " +potoken" } else { "" },
        if oauth_token.is_some() { " +oauth" } else { "" },
        if yt_cookies.is_some() { " +cookies" } else { "" },
        if yt_session.is_some() { " +session" } else { "" },
    );

    let mut req = http
        .post(url)
        .header("User-Agent", itclient.user_agent)
        .header("Content-Type", "application/json")
        .header("X-YouTube-Client-Name", itclient.client_name_id.to_string())
        .header("X-YouTube-Client-Version", client_version);

    match itclient.client_name {
        "WEB_REMIX" => {
            req = req
                .header("Origin", "https://music.youtube.com")
                .header("Referer", "https://music.youtube.com/");

            // Visitor data — prefer page's visitorData, fall back to generated
            if let Some(session) = yt_session {
                if let Some(vd) = session.innertube_context.get("client")
                    .and_then(|c| c.get("visitorData"))
                    .and_then(|v| v.as_str())
                {
                    req = req.header("X-Goog-Visitor-Id", vd);
                }
            } else if let Some(vd) = visitor_data {
                if !vd.is_empty() {
                    req = req.header("X-Goog-Visitor-Id", vd);
                }
            }

            // Auth: OAuth Bearer > cookie-based triple SAPISIDHASH
            if let Some(token) = oauth_token {
                req = req.header("Authorization", format!("Bearer {}", token));
            } else if let Some(c) = yt_cookies {
                let user_sid = yt_session.and_then(|s| s.user_session_id.as_deref());
                req = req
                    .header("Cookie", &c.cookie_header)
                    .header(
                        "Authorization",
                        crate::cookies::full_sapisidhash(c, "https://music.youtube.com", user_sid),
                    )
                    .header("X-Origin", "https://music.youtube.com");

                // Session-specific headers from ytcfg
                if let Some(session) = yt_session {
                    if let Some(ref idx) = session.session_index {
                        req = req.header("X-Goog-Authuser", idx.as_str());
                    }
                    if let Some(ref uid) = session.user_session_id {
                        req = req.header("X-Goog-Pageid", uid.as_str());
                    }
                    if session.logged_in {
                        req = req.header("X-Youtube-Bootstrap-Logged-In", "true");
                    }
                }
            }
        }
        "WEB" | "MWEB" => {
            req = req
                .header("Origin", "https://www.youtube.com")
                .header("Referer", "https://www.youtube.com/");
            if let Some(vd) = visitor_data {
                if !vd.is_empty() {
                    req = req.header("X-Goog-Visitor-Id", vd);
                }
            }
            if let Some(token) = oauth_token {
                req = req.header("Authorization", format!("Bearer {}", token));
            } else if let Some(c) = yt_cookies {
                req = req
                    .header("Cookie", &c.cookie_header)
                    .header("Authorization", crate::cookies::full_sapisidhash(c, "https://www.youtube.com", None))
                    .header("X-Origin", "https://www.youtube.com");
            }
        }
        _ => {
            if let Some(token) = oauth_token {
                req = req.header("Authorization", format!("Bearer {}", token));
            }
        }
    }

    let resp = req.json(&body).send().await.map_err(|e| e.without_url())?;

    if !resp.status().is_success() {
        return Err(format!(
            "innertube /player returned HTTP {}",
            resp.status()
        )
        .into());
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| e.without_url())?;

    let status = json["playabilityStatus"]["status"]
        .as_str()
        .unwrap_or("UNKNOWN");
    if status != "OK" {
        let reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("unknown reason");
        return Err(format!(
            "video {} not playable: {} ({})",
            video_id, status, reason
        )
        .into());
    }

    let server_abr_url = json["streamingData"]["serverAbrStreamingUrl"]
        .as_str()
        .ok_or("missing serverAbrStreamingUrl")?
        .to_owned();

    let ustreamer_b64 = json["playerConfig"]["mediaCommonConfig"]
        ["mediaUstreamerRequestConfig"]["videoPlaybackUstreamerConfig"]
        .as_str()
        .unwrap_or("");

    let ustreamer_config = if ustreamer_b64.is_empty() {
        Vec::new()
    } else {
        // YouTube uses URL-safe base64 (- and _ instead of + and /)
        // with optional padding. Use lenient decoding to handle both.
        use base64::Engine;
        use base64::engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode};
        const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
            &base64::alphabet::URL_SAFE,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        URL_SAFE_LENIENT
            .decode(ustreamer_b64)
            .unwrap_or_default()
    };

    let formats_json = json["streamingData"]["adaptiveFormats"]
        .as_array()
        .ok_or("missing adaptiveFormats")?;

    let mut formats = Vec::with_capacity(formats_json.len());
    for f in formats_json {
        let itag = f["itag"].as_i64().unwrap_or(0) as i32;
        let last_modified = f["lastModified"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let xtags = f["xtags"].as_str().map(|s| s.to_owned());
        let mime_type = f["mimeType"].as_str().unwrap_or("").to_owned();
        let bitrate = f["bitrate"].as_u64().unwrap_or(0);

        formats.push(AdaptiveFormat {
            itag,
            last_modified,
            xtags,
            mime_type,
            bitrate,
        });
    }

    let title = json["videoDetails"]["title"]
        .as_str()
        .unwrap_or("Unknown")
        .to_owned();
    let duration_secs = json["videoDetails"]["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(PlayerData {
        server_abr_streaming_url: server_abr_url,
        ustreamer_config,
        formats,
        title,
        duration_secs,
    })
}

/// Result from get_player_data: player metadata + GVS token + which client succeeded.
struct PlayerResult {
    player: PlayerData,
    gvs_token: Option<GvsToken>,
    client_config: &'static InnerTubeClient,
}

/// Call InnerTube /player using WEB_REMIX + ctt + cookies.
///
/// WEB_REMIX SABR URLs have an `n` parameter (nsig challenge) that must be
/// decoded via ytdlp-ejs before the CDN accepts them. The caller handles
/// nsig decoding after this function returns.
///
/// Fallback: IOS (no nsig needed, but 60s throttle without valid PoToken).
async fn get_player_data(
    http: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    yt_cookies: Option<&crate::cookies::YtCookies>,
    yt_session: Option<&crate::cookies::YtMusicSession>,
    _oauth_token: Option<&str>,
) -> Result<PlayerResult, SabrError> {
    let gvs = fetch_pot_token(http, Some(video_id)).await;

    // --- Attempt 1: WEB_REMIX + ctt + cookies ---
    // Requires nsig decoding of SABR URL (handled by caller).
    if yt_session.is_some() {
        tracing::info!("[sabr] WEB_REMIX /player (ctt={}, cookies={}, session=true)",
            ctt.is_some(), yt_cookies.is_some());
        match innertube_player(
            http, video_id, &WEB_REMIX_CLIENT, ctt, None, None,
            yt_cookies, yt_session, None,
        ).await {
            Ok(data) => {
                tracing::info!("[sabr] WEB_REMIX /player OK");
                return Ok(PlayerResult { player: data, gvs_token: gvs, client_config: &WEB_REMIX_CLIENT });
            }
            Err(e) => {
                tracing::warn!("[sabr] WEB_REMIX /player failed: {}", e);
            }
        }
    }

    // --- Fallback: IOS ---
    // No nsig needed but 60s throttle (stream_protection_status=2).
    tracing::info!("[sabr] IOS /player fallback (potoken={})", gvs.is_some());
    let data = innertube_player(http, video_id, &IOS_CLIENT, None, None, None, None, None, None).await?;
    tracing::info!("[sabr] IOS /player OK");
    Ok(PlayerResult { player: data, gvs_token: gvs, client_config: &IOS_CLIENT })
}

// ---------------------------------------------------------------------------
// nsig (n-parameter) decoding via ytdlp-ejs
// ---------------------------------------------------------------------------

/// Decode the `n` parameter in a SABR URL using ytdlp-ejs (SWC + QuickJS).
///
/// WEB_REMIX SABR URLs contain an `n` query parameter that must be transformed
/// using a function embedded in YouTube's base.js player JavaScript. Without
/// decoding, the CDN returns 403 Forbidden.
///
/// This is a blocking operation (QuickJS eval of full base.js) so we run it in
/// a spawn_blocking context. Uses ~15-20MB RAM (vs ytdlp-ejs SWC: 200MB+).
fn decode_nsig(player_js: &str, n_value: &str, player_hash: &str) -> Result<String, SabrError> {
    crate::nsig::decode_nsig(player_js, n_value, player_hash)
        .map_err(|e| -> SabrError { format!("nsig: {}", e).into() })
}

/// Replace the `n` parameter in a SABR URL with the decoded value.
fn replace_n_param(url: &str, decoded_n: &str) -> String {
    // Parse URL, find &n=XXX or ?n=XXX, replace value
    if let Some(n_start) = url.find("&n=").or_else(|| url.find("?n=")) {
        let prefix_end = n_start + 3; // skip "&n=" or "?n="
        let rest = &url[prefix_end..];
        let n_end = rest.find('&').unwrap_or(rest.len());
        format!("{}{}{}", &url[..prefix_end], decoded_n, &rest[n_end..])
    } else {
        url.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Audio format selection
// ---------------------------------------------------------------------------

/// Choose the best audio format, preferring Opus/WebM at the highest bitrate.
fn choose_audio_format(formats: &[AdaptiveFormat]) -> Option<&AdaptiveFormat> {
    formats
        .iter()
        .filter(|f| f.mime_type.starts_with("audio/"))
        .max_by_key(|f| {
            let codec_priority: u64 = if f.mime_type.contains("opus") {
                1_000_000_000
            } else if f.mime_type.contains("mp4a") {
                500_000_000
            } else {
                0
            };
            codec_priority + f.bitrate
        })
}

/// Choose a video format for the SABR discard trick.
///
/// The SABR protocol requires both audio AND video formats in the request,
/// even for audio-only streaming. We select the lowest-quality video and
/// send a "fully buffered" BufferedRange (MAX values) so the server knows
/// not to send any video data.
fn choose_video_format(formats: &[AdaptiveFormat]) -> Option<&AdaptiveFormat> {
    formats
        .iter()
        .filter(|f| f.mime_type.starts_with("video/"))
        .min_by_key(|f| f.bitrate)
}

// ---------------------------------------------------------------------------
// SABR streaming state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct HeaderInfo {
    itag: i32,
    is_init_seg: bool,
    sequence_number: i32,
    start_ms: i64,
    duration_ms: i64,
    is_audio: bool,
}

/// Max i32 — used for the video discard trick.
const MAX_I32: i32 = i32::MAX;
const MAX_I64: i64 = MAX_I32 as i64;

struct SabrState {
    server_url: String,
    request_number: u32,
    playback_cookie: Option<Vec<u8>>,
    backoff_time_ms: u32,
    header_map: HashMap<u32, HeaderInfo>,
    // Audio tracking
    audio_itag: i32,
    audio_format_id: misc::FormatId,
    audio_max_segment: i32,
    audio_downloaded_ms: i64,
    audio_init_sent: bool,
    audio_initialized: bool,
    audio_total_end_ms: i64,
    audio_end_segment: i64,
    // Video format (for discard trick — we never play video)
    video_format_id: misc::FormatId,
    // SABR context
    sabr_contexts: HashMap<i32, Vec<u8>>,
    sabr_contexts_send: std::collections::HashSet<i32>,
    sabr_contexts_discard: std::collections::HashSet<i32>,
    ustreamer_config: Vec<u8>,
    client_config: &'static InnerTubeClient,
    po_token: Option<Vec<u8>>,
    cookie_header: Option<String>,
    done: bool,
}

impl SabrState {
    fn new(
        server_url: String,
        audio_format: &AdaptiveFormat,
        video_format: &AdaptiveFormat,
        ustreamer_config: Vec<u8>,
        client_config: &'static InnerTubeClient,
    ) -> Self {
        let audio_format_id = misc::FormatId {
            itag: Some(audio_format.itag),
            last_modified: Some(audio_format.last_modified),
            xtags: audio_format.xtags.clone(),
        };
        let video_format_id = misc::FormatId {
            itag: Some(video_format.itag),
            last_modified: Some(video_format.last_modified),
            xtags: video_format.xtags.clone(),
        };

        Self {
            server_url,
            request_number: 0,
            playback_cookie: None,
            backoff_time_ms: 0,
            header_map: HashMap::new(),
            audio_itag: audio_format.itag,
            audio_format_id,
            audio_max_segment: -1,
            audio_downloaded_ms: 0,
            audio_init_sent: false,
            audio_initialized: false,
            audio_total_end_ms: 0,
            audio_end_segment: 0,
            video_format_id,
            sabr_contexts: HashMap::new(),
            sabr_contexts_send: std::collections::HashSet::new(),
            sabr_contexts_discard: std::collections::HashSet::new(),
            ustreamer_config,
            client_config,
            po_token: None,
            cookie_header: None,
            done: false,
        }
    }

    /// Build a SABR VideoPlaybackAbrRequest protobuf.
    ///
    /// Matches the TypeScript googlevideo reference behavior:
    /// - Always includes a video format with MAX discard BufferedRange
    /// - Only includes audio in selectedFormatIds after FORMAT_INIT received
    /// - Always includes both in preferredAudio/VideoFormatIds
    fn build_request(&self) -> vs::VideoPlaybackAbrRequest {
        let client_info = vs::streamer_context::ClientInfo {
            device_make: Some(self.client_config.device_make.to_owned()),
            device_model: Some(self.client_config.device_model.to_owned()),
            client_name: Some(self.client_config.client_name_id),
            client_version: Some(self.client_config.client_version.to_owned()),
            os_name: Some(self.client_config.os_name.to_owned()),
            os_version: Some(self.client_config.os_version.to_owned()),
            android_sdk_version: self.client_config.android_sdk_version,
            ..Default::default()
        };

        let sabr_ctxs: Vec<vs::streamer_context::SabrContext> = self
            .sabr_contexts
            .iter()
            .filter(|(ct, _)| {
                !self.sabr_contexts_discard.contains(ct)
                    && self.sabr_contexts_send.contains(ct)
            })
            .map(|(ct, v)| vs::streamer_context::SabrContext {
                r#type: Some(*ct),
                value: Some(v.clone()),
            })
            .collect();

        let streamer_context = vs::StreamerContext {
            client_info: Some(client_info),
            po_token: self.po_token.clone(),
            playback_cookie: self.playback_cookie.clone(),
            sabr_contexts: sabr_ctxs,
            ..Default::default()
        };

        // -- BufferedRanges --
        let mut buffered_ranges = Vec::new();

        // Audio: only report after we've received at least one segment.
        if self.audio_max_segment >= 0 {
            buffered_ranges.push(vs::BufferedRange {
                format_id: self.audio_format_id.clone(),
                start_time_ms: 0,
                duration_ms: self.audio_downloaded_ms,
                start_segment_index: 0,
                end_segment_index: self.audio_max_segment,
                time_range: Some(vs::TimeRange {
                    start_ticks: Some(0),
                    duration_ticks: Some(self.audio_downloaded_ms),
                    timescale: Some(1000),
                }),
                ..Default::default()
            });
        }

        // Video: always "fully buffered" (discard trick).
        // This tells the server we have ALL video data, so it won't send any.
        buffered_ranges.push(vs::BufferedRange {
            format_id: self.video_format_id.clone(),
            start_time_ms: 0,
            duration_ms: MAX_I64,
            start_segment_index: MAX_I32,
            end_segment_index: MAX_I32,
            time_range: Some(vs::TimeRange {
                start_ticks: Some(0),
                duration_ticks: Some(MAX_I64),
                timescale: Some(1000),
            }),
            ..Default::default()
        });

        // -- selectedFormatIds --
        // Per TS reference: video (discarded) is always in selectedFormatIds.
        // Audio is only added after FORMAT_INITIALIZATION_METADATA is received.
        let mut selected_format_ids = vec![self.video_format_id.clone()];
        if self.audio_initialized {
            selected_format_ids.push(self.audio_format_id.clone());
        }

        // Report player_time_ms close to the buffer end so the server
        // sees a small buffer and keeps sending segments. With player_time_ms=0
        // the server sees 60s+ of buffer and throttles to a crawl.
        let player_time = self.audio_downloaded_ms.saturating_sub(10_000).max(0);

        let client_abr_state = vs::ClientAbrState {
            enabled_track_types_bitfield: Some(1), // 1 = audio only
            playback_rate: Some(1.0),
            bandwidth_estimate: Some(5_000_000),
            player_time_ms: Some(player_time),
            visibility: Some(1),
            player_state: Some(1),
            ..Default::default()
        };

        vs::VideoPlaybackAbrRequest {
            client_abr_state: Some(client_abr_state),
            selected_format_ids,
            buffered_ranges,
            video_playback_ustreamer_config: if self.ustreamer_config.is_empty() {
                None
            } else {
                Some(self.ustreamer_config.clone())
            },
            preferred_audio_format_ids: vec![self.audio_format_id.clone()],
            preferred_video_format_ids: vec![self.video_format_id.clone()],
            streamer_context: Some(streamer_context),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// UMP part handlers
// ---------------------------------------------------------------------------

fn handle_format_init_metadata(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let meta = vs::FormatInitializationMetadata::decode(data)?;
    let itag = meta.format_id.as_ref().and_then(|f| f.itag).unwrap_or(0);
    let mime_type = meta.mime_type.clone().unwrap_or_default();
    let is_audio = itag == state.audio_itag;

    tracing::info!(
        "[sabr] format init: itag={}, mime={}, audio={}, end_seg={:?}, end_ms={:?}",
        itag, mime_type, is_audio, meta.end_segment_number, meta.end_time_ms,
    );

    if is_audio {
        state.audio_initialized = true;
        state.audio_total_end_ms = meta.end_time_ms.unwrap_or(0);
        state.audio_end_segment = meta.end_segment_number.unwrap_or(0);
    }

    Ok(())
}

fn handle_media_header(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let hdr = vs::MediaHeader::decode(data)?;
    let header_id = hdr.header_id.unwrap_or(0);
    let itag = hdr.itag.unwrap_or(0);
    let is_audio = itag == state.audio_itag;

    // Compute duration_ms: prefer the direct field, fall back to timeRange.
    // The TS reference does this same fallback.
    let duration_ms = hdr.duration_ms.unwrap_or_else(|| {
        if let Some(ref tr) = hdr.time_range {
            let ticks = tr.duration_ticks.unwrap_or(0);
            let timescale = tr.timescale.unwrap_or(1000);
            if timescale > 0 {
                (ticks * 1000) / timescale as i64
            } else {
                0
            }
        } else {
            0
        }
    });
    let start_ms = hdr.start_ms.unwrap_or_else(|| {
        if let Some(ref tr) = hdr.time_range {
            let ticks = tr.start_ticks.unwrap_or(0);
            let timescale = tr.timescale.unwrap_or(1000);
            if timescale > 0 {
                (ticks * 1000) / timescale as i64
            } else {
                0
            }
        } else {
            0
        }
    });

    tracing::debug!(
        "[sabr] media header: id={}, itag={}, init={:?}, seq={:?}, start={}ms, dur={}ms, tr={:?}, audio={}",
        header_id, itag, hdr.is_init_seg, hdr.sequence_number,
        start_ms, duration_ms, hdr.time_range, is_audio,
    );

    state.header_map.insert(header_id, HeaderInfo {
        itag,
        is_init_seg: hdr.is_init_seg.unwrap_or(false),
        sequence_number: hdr.sequence_number.unwrap_or(0),
        start_ms,
        duration_ms,
        is_audio,
    });

    Ok(())
}

/// Handle MEDIA part. First byte is header_id, rest is media payload.
fn handle_media(state: &mut SabrState, data: &[u8]) -> Option<Bytes> {
    if data.is_empty() {
        return None;
    }

    let header_id = data[0] as u32;
    let payload = &data[1..];

    let info = match state.header_map.get(&header_id) {
        Some(i) => i.clone(),
        None => {
            tracing::warn!("[sabr] media: unknown header_id={}", header_id);
            return None;
        }
    };

    if !info.is_audio || payload.is_empty() {
        return None;
    }

    if info.is_init_seg {
        state.audio_init_sent = true;
    }

    Some(Bytes::copy_from_slice(payload))
}

/// Handle MEDIA_END. First byte is header_id. Update buffered-range tracking.
fn handle_media_end(state: &mut SabrState, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let header_id = data[0] as u32;
    let info = match state.header_map.get(&header_id) {
        Some(i) => i.clone(),
        None => return,
    };
    if !info.is_audio || info.is_init_seg {
        return;
    }
    if info.sequence_number > state.audio_max_segment {
        state.audio_max_segment = info.sequence_number;
    }
    let seg_end = info.start_ms + info.duration_ms;
    if seg_end > state.audio_downloaded_ms {
        state.audio_downloaded_ms = seg_end;
    }

    tracing::debug!(
        "[sabr] audio seg {} done ({}ms-{}ms), progress {}ms/{}ms",
        info.sequence_number, info.start_ms, info.start_ms + info.duration_ms,
        state.audio_downloaded_ms, state.audio_total_end_ms,
    );
}

fn handle_next_request_policy(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let policy = vs::NextRequestPolicy::decode(data)?;
    state.backoff_time_ms = policy.backoff_time_ms.unwrap_or(0) as u32;
    if let Some(ref cookie) = policy.playback_cookie {
        state.playback_cookie = Some(cookie.encode_to_vec());
    }
    tracing::debug!(
        "[sabr] next request: backoff={}ms, cookie={}",
        state.backoff_time_ms, policy.playback_cookie.is_some(),
    );
    Ok(())
}

fn handle_sabr_redirect(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let redirect = vs::SabrRedirect::decode(data)?;
    if let Some(url) = redirect.url {
        check_streaming_url(&url)?;
        tracing::info!("[sabr] redirect to new streaming URL");
        state.server_url = url;
    }
    Ok(())
}

fn handle_sabr_error(data: &[u8]) -> Result<(), SabrError> {
    let err = vs::SabrError::decode(data)?;
    let t = err.r#type.as_deref().unwrap_or("unknown");
    let c = err.code.unwrap_or(0);
    Err(format!("SABR error: type={}, code={}", t, c).into())
}

fn handle_context_update(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let update = vs::SabrContextUpdate::decode(data)?;
    let ctx_type = update.r#type.unwrap_or(0);
    let send = update.send_by_default.unwrap_or(false);
    if let Some(value) = update.value {
        state.sabr_contexts.insert(ctx_type, value);
    }
    if send {
        state.sabr_contexts_send.insert(ctx_type);
    }
    tracing::debug!("[sabr] context update: type={}, send={}", ctx_type, send);
    Ok(())
}

fn handle_context_sending_policy(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let policy = vs::SabrContextSendingPolicy::decode(data)?;
    for t in &policy.start_policy {
        state.sabr_contexts_send.insert(*t);
    }
    for t in &policy.stop_policy {
        state.sabr_contexts_send.remove(t);
    }
    for t in &policy.discard_policy {
        state.sabr_contexts_discard.insert(*t);
        state.sabr_contexts.remove(t);
    }
    tracing::debug!(
        "[sabr] sending policy: start={:?}, stop={:?}, discard={:?}",
        policy.start_policy, policy.stop_policy, policy.discard_policy,
    );
    Ok(())
}

fn handle_stream_protection(data: &[u8]) -> Result<(), SabrError> {
    let sps = vs::StreamProtectionStatus::decode(data)?;
    let status = sps.status.unwrap_or(0);
    match status {
        0 => tracing::info!("[sabr] stream_protection_status=0 (no protection)"),
        1 => tracing::info!("[sabr] stream_protection_status=1 (OK — Premium/PoToken accepted!)"),
        2 => tracing::warn!("[sabr] stream_protection_status=2 (GRACE PERIOD — ~60s then throttle)"),
        3 => tracing::error!("[sabr] stream_protection_status=3 (HARD BLOCK — no more media)"),
        n => tracing::warn!("[sabr] stream_protection_status={} (unknown)", n),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Single SABR request/response cycle
// ---------------------------------------------------------------------------

/// Error bodies are reported by length only: they can echo request data, and
/// slicing them by bytes is not UTF-8 safe.
fn http_error_message(status: reqwest::StatusCode, state: &SabrState, body_len: usize) -> String {
    format!(
        "SABR HTTP {} (client={}, has_potoken={}, has_cookie={}): body {} bytes",
        status, state.client_config.client_name,
        state.po_token.is_some(), state.playback_cookie.is_some(),
        body_len,
    )
}

async fn sabr_request_cycle(
    http: &reqwest::Client,
    state: &mut SabrState,
    tx: &mpsc::Sender<Bytes>,
) -> Result<bool, SabrError> {
    check_streaming_url(&state.server_url)?;
    let req_proto = state.build_request();
    let encoded = req_proto.encode_to_vec();

    let url = if state.server_url.contains('?') {
        format!("{}&rn={}", state.server_url, state.request_number)
    } else {
        format!("{}?rn={}", state.server_url, state.request_number)
    };

    tracing::info!(
        "[sabr] request #{} ({} bytes), progress {}ms/{}ms",
        state.request_number, encoded.len(),
        state.audio_downloaded_ms, state.audio_total_end_ms,
    );

    // CDN headers: content-type, accept, accept-encoding.
    // WEB_REMIX SABR URLs require cookies on CDN requests (403 without them).
    let mut req = http
        .post(&url)
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/vnd.yt-ump")
        .header("Accept-Encoding", "identity");

    if let Some(ref cookies) = state.cookie_header {
        req = req.header("Cookie", cookies);
    }

    // Signed streaming URLs are credentials; reqwest errors would print them.
    let resp = req.body(encoded).send().await.map_err(|e| e.without_url())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_len = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
        return Err(http_error_message(status, state, body_len).into());
    }

    let mut parser = UmpParser::new();
    let mut byte_stream = resp.bytes_stream();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk = chunk_result.map_err(|e| e.without_url())?;
        parser.push(&chunk);

        while let Some(part) = parser.next_part() {
            match part.part_type {
                ump::FORMAT_INITIALIZATION_METADATA => {
                    handle_format_init_metadata(state, &part.data)?;
                }
                ump::MEDIA_HEADER => {
                    handle_media_header(state, &part.data)?;
                }
                ump::MEDIA => {
                    if let Some(audio_bytes) = handle_media(state, &part.data) {
                        if tx.send(audio_bytes).await.is_err() {
                            tracing::info!("[sabr] receiver dropped, stopping");
                            return Ok(false);
                        }
                    }
                }
                ump::MEDIA_END => {
                    handle_media_end(state, &part.data);
                }
                ump::NEXT_REQUEST_POLICY => {
                    handle_next_request_policy(state, &part.data)?;
                }
                ump::SABR_REDIRECT => {
                    handle_sabr_redirect(state, &part.data)?;
                }
                ump::SABR_ERROR => {
                    handle_sabr_error(&part.data)?;
                }
                ump::SABR_CONTEXT_UPDATE => {
                    handle_context_update(state, &part.data)?;
                }
                ump::STREAM_PROTECTION_STATUS => {
                    handle_stream_protection(&part.data)?;
                }
                ump::SABR_CONTEXT_SENDING_POLICY => {
                    handle_context_sending_policy(state, &part.data)?;
                }
                other => {
                    tracing::debug!(
                        "[sabr] unhandled UMP type={}, {} bytes",
                        other, part.data.len(),
                    );
                }
            }
        }
    }

    state.request_number += 1;

    if state.audio_end_segment > 0
        && state.audio_max_segment >= (state.audio_end_segment as i32)
    {
        tracing::info!(
            "[sabr] all audio segments downloaded ({}/{})",
            state.audio_max_segment, state.audio_end_segment,
        );
        state.done = true;
        return Ok(false);
    }

    if state.audio_total_end_ms > 0
        && state.audio_downloaded_ms >= state.audio_total_end_ms
    {
        tracing::info!(
            "[sabr] all audio downloaded by time ({}ms/{}ms)",
            state.audio_downloaded_ms, state.audio_total_end_ms,
        );
        state.done = true;
        return Ok(false);
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Stream audio for a YouTube video via the SABR protocol.
///
/// Returns metadata about the stream and a channel receiver that yields
/// audio byte chunks. The init segment (WebM/Opus container headers) is sent
/// first, followed by media segments in order.
///
/// A background tokio task runs the SABR request loop. When all segments
/// have been downloaded, or an error occurs, the task ends and the channel
/// closes naturally.
///
/// Requests go through `credential_client()` so that redirects cannot carry
/// cookies or tokens off Google hosts.
pub async fn stream_audio(
    video_id: &str,
    ctt: Option<&str>,
    yt_cookies: Option<&crate::cookies::YtCookies>,
    yt_session: Option<&crate::cookies::YtMusicSession>,
    oauth: Option<&std::sync::Arc<crate::oauth::OAuthState>>,
) -> Result<(SabrStreamInfo, mpsc::Receiver<Bytes>), SabrError> {
    if ctt.is_some() {
        tracing::info!("[sabr] stream_audio for {} with ctt=true", video_id);
    }
    let client = credential_client();
    // Get OAuth token if available (auto-refreshes if expired)
    let oauth_token = if let Some(os) = oauth {
        os.get_access_token().await
    } else {
        None
    };

    // 1. Call InnerTube /player to get streaming metadata (+ GVS PoToken if bgutil-pot is running).
    let result = get_player_data(client, video_id, ctt, yt_cookies, yt_session, oauth_token.as_deref()).await?;
    let player = result.player;
    let client_config = result.client_config;
    // get_player_data already handled PoToken selection per attempt:
    // - Attempt 1 (cookies): no PoToken
    // - Attempt 2 (bgutil-pot): session-bound PoToken for SABR
    // - Attempt 3/fallback: no PoToken
    let gvs_po_token = result.gvs_token.map(|g| g.po_token);

    // 2. Pick the best audio format + a video format for the discard trick.
    let audio_fmt = choose_audio_format(&player.formats)
        .ok_or("no suitable audio format found")?
        .clone();
    let video_fmt = choose_video_format(&player.formats)
        .ok_or("no video format found (needed for SABR discard trick)")?
        .clone();

    tracing::info!(
        "[sabr] selected audio: itag={}, mime={}, bitrate={}",
        audio_fmt.itag, audio_fmt.mime_type, audio_fmt.bitrate,
    );
    tracing::info!(
        "[sabr] selected video (discard): itag={}, mime={}",
        video_fmt.itag, video_fmt.mime_type,
    );

    let info = SabrStreamInfo {
        title: player.title.clone(),
        duration_secs: player.duration_secs,
        mime_type: audio_fmt.mime_type.clone(),
    };

    // 3. Channel for streaming audio bytes to the caller.
    let (tx, rx) = mpsc::channel::<Bytes>(64);

    // 4. Decode nsig if WEB_REMIX SABR URL has `n` parameter.
    let mut server_url = player.server_abr_streaming_url;
    if client_config.client_name == "WEB_REMIX" {
        // Extract `n` param from URL
        let n_value = {
            let mut found = None;
            if let Some(query_start) = server_url.find('?') {
                for pair in server_url[query_start + 1..].split('&') {
                    if let Some(val) = pair.strip_prefix("n=") {
                        found = Some(val.to_owned());
                        break;
                    }
                }
            }
            found
        };

        if let Some(ref n_val) = n_value {
            if let Some(session) = yt_session {
                if let Some(ref js) = session.player_js {
                    tracing::info!("[sabr] decoding nsig: n len={}", n_val.len());
                    let js_clone = js.clone();
                    let n_clone = n_val.clone();
                    let hash_clone = session.player_hash.clone().unwrap_or_default();
                    match tokio::task::spawn_blocking(move || decode_nsig(&js_clone, &n_clone, &hash_clone)).await {
                        Ok(Ok(decoded)) => {
                            tracing::info!("[sabr] nsig decoded: len {} -> {}", n_val.len(), decoded.len());
                            server_url = replace_n_param(&server_url, &decoded);
                        }
                        Ok(Err(e)) => {
                            tracing::error!("[sabr] nsig decode failed: {}", e);
                            return Err(e);
                        }
                        Err(e) => {
                            tracing::error!("[sabr] nsig task panicked: {}", e);
                            return Err(format!("nsig task panicked: {}", e).into());
                        }
                    }
                } else {
                    tracing::warn!("[sabr] WEB_REMIX URL has n= but no player_js cached — CDN will 403");
                }
            } else {
                tracing::warn!("[sabr] WEB_REMIX URL has n= but no session — CDN will 403");
            }
        }
    }

    check_streaming_url(&server_url)?;

    // 5. Spawn the SABR streaming loop.
    let http = client.clone();
    let ustreamer_config = player.ustreamer_config;

    let video_fmt_clone = video_fmt.clone();
    let cookie_header = yt_cookies.map(|c| c.cookie_header.clone());
    tokio::spawn(async move {
        let result = sabr_loop(
            &http, server_url, &audio_fmt, &video_fmt_clone,
            ustreamer_config, client_config, gvs_po_token, cookie_header, &tx,
        ).await;

        match result {
            Ok(()) => tracing::info!("[sabr] streaming complete"),
            Err(e) => tracing::error!("[sabr] streaming error: {}", e),
        }
        // tx is dropped here, closing the channel.
    });

    Ok((info, rx))
}

/// Inner SABR request loop. Runs until all audio segments are downloaded,
/// the downstream receiver disconnects, or an unrecoverable error occurs.
///
/// Transient network errors are retried with exponential backoff (up to 5
/// retries). This matches the TypeScript reference behavior.
async fn sabr_loop(
    http: &reqwest::Client,
    server_url: String,
    audio_format: &AdaptiveFormat,
    video_format: &AdaptiveFormat,
    ustreamer_config: Vec<u8>,
    client_config: &'static InnerTubeClient,
    po_token: Option<String>,
    cookie_header: Option<String>,
    tx: &mpsc::Sender<Bytes>,
) -> Result<(), SabrError> {
    let mut state = SabrState::new(
        server_url, audio_format, video_format, ustreamer_config, client_config,
    );
    state.cookie_header = cookie_header;

    // Set PoToken for SABR StreamerContext (decoded from base64 to bytes).
    // bgutil-pot returns URL-safe base64 (uses `-` and `_`) with optional padding.
    if let Some(ref token) = po_token {
        use base64::Engine;
        use base64::engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode};
        const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
            &base64::alphabet::URL_SAFE,
            GeneralPurposeConfig::new()
                .with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        match URL_SAFE_LENIENT.decode(token) {
            Ok(bytes) => {
                tracing::info!("[sabr] PoToken set ({} bytes)", bytes.len());
                state.po_token = Some(bytes);
            }
            Err(e) => tracing::warn!("[sabr] failed to decode PoToken: {}", e),
        }
    }

    let mut consecutive_errors = 0u32;
    const MAX_RETRIES: u32 = 5;
    let mut stall_count = 0u32;

    loop {
        // Check if the downstream consumer (MPD/axum) has disconnected.
        // Without this, SABR loops run forever when tracks are skipped
        // because there's no media data to trigger the tx.send() check.
        if tx.is_closed() {
            tracing::info!("[sabr] receiver closed, stopping");
            break;
        }

        let prev_progress = state.audio_downloaded_ms;

        match sabr_request_cycle(http, &mut state, tx).await {
            Ok(should_continue) => {
                consecutive_errors = 0;
                if !should_continue || state.done {
                    break;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors > MAX_RETRIES {
                    return Err(format!(
                        "SABR failed after {} retries: {}", MAX_RETRIES, e
                    ).into());
                }
                let delay_ms = 500 * 2u64.pow(consecutive_errors - 1);
                tracing::warn!(
                    "[sabr] request error (attempt {}/{}): {}, retrying in {}ms",
                    consecutive_errors, MAX_RETRIES, e, delay_ms,
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
        }

        // Detect stalls: if the server keeps responding but never sends
        // new segments, stop after too many fruitless requests.
        if state.audio_downloaded_ms == prev_progress && prev_progress > 0 {
            stall_count += 1;
            if stall_count > 30 {
                tracing::warn!(
                    "[sabr] stalled at {}ms for {} requests, giving up",
                    state.audio_downloaded_ms, stall_count,
                );
                break;
            }
        } else {
            stall_count = 0;
        }

        // Respect the server-requested backoff before the next request.
        if state.backoff_time_ms > 0 {
            tracing::debug!("[sabr] backing off {}ms", state.backoff_time_ms);
            tokio::time::sleep(std::time::Duration::from_millis(
                state.backoff_time_ms as u64,
            )).await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn trusted_google_host_check() {
        let cases = [
            ("https://rr3---sn-abc.googlevideo.com/videoplayback", true),
            ("https://www.youtube.com/youtubei/v1/player", true),
            ("https://music.youtube.com/", true),
            ("https://youtube.com/", true),
            ("http://www.youtube.com/", false),
            ("https://evilyoutube.com/", false),
            ("https://youtube.com.evil.example/", false),
            ("https://googlevideo.com.attacker.example/", false),
            ("https://youtube.com@evil.example/", false),
            ("https://user:pass@www.youtube.com/", false),
            ("https://192.0.2.1/", false),
            ("https://[2001:db8::1]/", false),
            ("not-a-url", false),
        ];
        for (url, expected) in cases {
            assert_eq!(is_trusted_google_host(url), expected, "{url}");
        }
    }

    // -- helpers --------------------------------------------------------------

    fn test_state(server_url: &str) -> SabrState {
        let fmt = AdaptiveFormat {
            itag: 251,
            last_modified: 1,
            xtags: None,
            mime_type: "audio/webm".to_owned(),
            bitrate: 1,
        };
        SabrState::new(server_url.to_owned(), &fmt, &fmt, Vec::new(), &WEB_REMIX_CLIENT)
    }

    const GOOD_URL: &str = "https://rr1---sn-abc.googlevideo.com/videoplayback?a=1";
    const SECRET_COOKIE: &str = "SAPISID=secret-cookie-value";
    const SECRET_POTOKEN: &[u8] = b"secret-potoken-bytes";

    #[derive(Clone, Default)]
    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl LogBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Captures all tracing output on this thread while the guard lives.
    fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
        let logs = LogBuf::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// Reads one HTTP request (head plus content-length body) from `sock`.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            let n = sock.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return String::from_utf8_lossy(&req).into_owned();
            }
            req.extend_from_slice(&buf[..n]);
            if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&req[..head_end]).to_ascii_lowercase();
        let body_len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while req.len() < head_end + body_len {
            let n = sock.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&req).into_owned()
    }

    /// Accepts one HTTP request, sends `response` and returns the raw request.
    async fn serve_one(listener: tokio::net::TcpListener, response: String) -> String {
        let (mut sock, _) = listener.accept().await.unwrap();
        let req = read_request(&mut sock).await;
        sock.write_all(response.as_bytes()).await.unwrap();
        req
    }

    /// Plays an untrusted host: records every request that reaches it and
    /// answers 500, so a regression shows up as a recorded request, not a hang.
    fn spawn_recorder(listener: tokio::net::TcpListener) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let req = read_request(&mut sock).await;
                record.lock().unwrap().push(req);
                let _ = sock
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            }
        });
        seen
    }

    async fn within_2s<F: std::future::Future>(what: &str, fut: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(2), fut)
            .await
            .unwrap_or_else(|_| panic!("{what} did not finish within 2s"))
    }

    // -- SABR redirect -------------------------------------------------------

    #[test]
    fn redirect_to_foreign_host_is_rejected() {
        let (logs, _guard) = capture_logs();
        let mut state = test_state(GOOD_URL);
        let target = "https://evil.example/stream?token=secret-query-token";
        let data = vs::SabrRedirect { url: Some(target.to_owned()) }.encode_to_vec();

        let err = handle_sabr_redirect(&mut state, &data).unwrap_err();

        assert!(err.to_string().contains("evil.example"));
        assert!(!err.to_string().contains("secret-query-token"));
        assert_eq!(state.server_url, GOOD_URL);
        let logs = logs.text();
        assert!(logs.contains("evil.example"), "{logs}");
        assert!(!logs.contains("secret-query-token"), "{logs}");
    }

    #[test]
    fn redirect_to_googlevideo_is_followed() {
        let mut state = test_state(GOOD_URL);
        let target = "https://rr2---sn-def.googlevideo.com/videoplayback?b=2";
        let data = vs::SabrRedirect { url: Some(target.to_owned()) }.encode_to_vec();
        handle_sabr_redirect(&mut state, &data).unwrap();
        assert_eq!(state.server_url, target);
    }

    // -- initial streaming URL ---------------------------------------------------

    #[test]
    fn untrusted_initial_server_url_fails() {
        let (logs, _guard) = capture_logs();
        let err = check_streaming_url("https://evil.example/videoplayback?pot=secret-query-token")
            .unwrap_err();
        assert!(err.to_string().contains("evil.example"));
        let logs = logs.text();
        assert!(logs.contains("evil.example"), "{logs}");
        assert!(!logs.contains("secret-query-token"), "{logs}");
        check_streaming_url(GOOD_URL).unwrap();
    }

    #[tokio::test]
    async fn request_cycle_to_untrusted_host_sends_nothing() {
        let (logs, _guard) = capture_logs();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reached = spawn_recorder(listener);

        let mut state = test_state(&format!("http://{addr}/videoplayback"));
        state.cookie_header = Some(SECRET_COOKIE.to_owned());
        state.po_token = Some(SECRET_POTOKEN.to_vec());
        let (tx, _rx) = mpsc::channel(1);

        let result = within_2s("sabr_request_cycle", sabr_request_cycle(credential_client(), &mut state, &tx)).await;

        let reached = reached.lock().unwrap().clone();
        assert!(reached.is_empty(), "request reached the untrusted host: {reached:?}");
        assert!(result.is_err());
        let logs = logs.text();
        assert!(logs.contains("refusing untrusted streaming host 127.0.0.1"), "{logs}");
        assert!(!logs.contains("secret-cookie-value"), "{logs}");
        assert!(!logs.contains("secret-potoken-bytes"), "{logs}");
    }

    #[tokio::test]
    async fn request_cycle_errors_do_not_carry_signed_url() {
        let (logs, _guard) = capture_logs();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        // Trusted URL, but every connection is routed to a closed local port.
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{closed_addr}")).unwrap())
            .build()
            .unwrap();
        let mut state = test_state(
            "https://rr1---sn-abc.googlevideo.com/videoplayback?sig=secret-sig&lsig=secret-lsig",
        );
        let (tx, _rx) = mpsc::channel(1);

        let err = within_2s("sabr_request_cycle", sabr_request_cycle(&http, &mut state, &tx))
            .await
            .unwrap_err();
        tracing::error!("[sabr] streaming error: {}", err);

        for text in [err.to_string(), format!("{err:?}"), logs.text()] {
            assert!(!text.is_empty());
            assert!(!text.contains("secret-sig"), "{text}");
            assert!(!text.contains("secret-lsig"), "{text}");
        }
    }

    #[test]
    fn http_error_reports_body_length_not_text() {
        // 3-byte chars: byte 200 is not a char boundary.
        let body = format!("SECRET-BODY-{}", "\u{20ac}".repeat(80));
        assert!(body.len() > 200 && !body.is_char_boundary(200));
        let state = test_state(GOOD_URL);

        let msg = http_error_message(reqwest::StatusCode::FORBIDDEN, &state, body.len());

        assert!(msg.contains("403"), "{msg}");
        assert!(msg.contains(&format!("body {} bytes", body.len())), "{msg}");
        assert!(!msg.contains("SECRET-BODY"), "{msg}");
    }

    // -- HTTP redirects --------------------------------------------------------

    #[tokio::test]
    async fn credential_client_does_not_follow_307_to_foreign_host() {
        let (logs, _guard) = capture_logs();
        let foreign = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let foreign_addr = foreign.local_addr().unwrap();
        let reached = spawn_recorder(foreign);
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        let server = tokio::spawn(serve_one(
            origin,
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{foreign_addr}/steal\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            ),
        ));

        let send = credential_client()
            .post(format!("http://{origin_addr}/videoplayback"))
            .header("Cookie", SECRET_COOKIE)
            .body("secret-body")
            .send();
        let resp = within_2s("credential_client POST", send).await.unwrap();
        within_2s("origin server", server).await.unwrap();

        let reached = reached.lock().unwrap().clone();
        assert!(reached.is_empty(), "redirect was followed to the foreign host: {reached:?}");
        assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        let logs = logs.text();
        assert!(logs.contains("not following redirect to 127.0.0.1"), "{logs}");
        assert!(!logs.contains("secret-cookie-value"), "{logs}");
    }

    // -- secret-free logs ------------------------------------------------------

    #[tokio::test]
    async fn pot_token_log_shows_length_only() {
        let (logs, _guard) = capture_logs();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let json = r#"{"poToken":"secret-potoken-value-0123456789"}"#;
        let server = tokio::spawn(serve_one(
            proxy,
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{json}",
                json.len()
            ),
        ));
        // bgutil-pot lives at a fixed localhost URL; route it to the fake server.
        let http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::http(format!("http://{proxy_addr}")).unwrap())
            .build()
            .unwrap();

        let token = fetch_pot_token(&http, Some("vid")).await.unwrap();
        server.await.unwrap();

        assert_eq!(token.po_token, "secret-potoken-value-0123456789");
        let logs = logs.text();
        assert!(logs.contains(&format!("PoToken (len={})", token.po_token.len())), "{logs}");
        assert!(!logs.contains("secret-potoken"), "{logs}");
        assert!(!logs.contains(&token.visitor_data[..10]), "{logs}");
    }

    #[test]
    fn log_formats_do_not_include_secret_values() {
        let src = include_str!("stream.rs").split("#[cfg(test)]").next().unwrap();
        let forbidden = [
            "ctt={}...",
            "with ctt={}",
            "PoToken ({}",
            "visitor_data={}",
            "&po_token[..",
            "&token[..",
            "&visitor_data[..",
            "n={}...",
            "&decoded[..",
        ];
        for pattern in forbidden {
            assert!(!src.contains(pattern), "log format still leaks: {pattern}");
        }
    }
}
