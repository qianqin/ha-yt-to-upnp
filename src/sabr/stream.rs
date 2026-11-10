//! Core SABR streaming implementation.
//!
//! Handles the complete SABR audio streaming flow:
//!   InnerTube /player call -> format selection -> SABR request loop ->
//!   UMP response parsing -> audio byte extraction.
//!
//! Called from an axum HTTP handler. Takes a video ID and returns a stream
//! of audio bytes. The caller pipes these bytes as an HTTP response to MPD.

use std::collections::{BTreeMap, HashMap};

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
#[derive(Debug)]
pub struct SabrStreamInfo {
    pub title: String,
    pub duration_secs: u64,
    pub mime_type: String,
    /// Total size of the format, when the transport knows it (url transport).
    /// `/stream` then answers byte ranges, so MPD can seek.
    pub content_length: Option<u64>,
    /// Byte offset the stream starts at (a range request); 0 for SABR.
    pub start_offset: u64,
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
/// redirects), so the ctt and PoToken must never follow them blindly.
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
pub(crate) fn credential_client() -> &'static reqwest::Client {
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

#[derive(Debug, Clone, Default)]
struct AdaptiveFormat {
    itag: i32,
    last_modified: u64,
    xtags: Option<String>,
    mime_type: String,
    bitrate: u64,
    /// Direct media URL, or `signatureCipher` (url + s + sp); used by the
    /// url transport for Premium formats.
    url: Option<String>,
    signature_cipher: Option<String>,
    content_length: Option<u64>,
    is_drc: bool,
    audio_track_id: Option<String>,
}

// ---------------------------------------------------------------------------
// InnerTube /player call
// ---------------------------------------------------------------------------

/// Build a minimal anonymous InnerTube context from hardcoded client
/// constants, for the fallback clients (IOS/ANDROID/WEB). WEB_REMIX never uses
/// it: it always gets the page session's context (see `PlayerTarget`).
fn build_minimal_context(
    itclient: &InnerTubeClient,
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

    serde_json::json!({ "client": client_obj })
}

/// Which /player call to make. WEB_REMIX can only be built from a
/// `WebRemixAuth`, so it always carries both the ctt and the page session.
#[derive(Clone, Copy)]
enum PlayerTarget<'a> {
    WebRemix(crate::music_session::WebRemixAuth<'a>),
    /// An anonymous fallback client (IOS/ANDROID/WEB) with a minimal context.
    Anonymous(&'static InnerTubeClient),
}

async fn innertube_player(
    http: &reqwest::Client,
    video_id: &str,
    target: PlayerTarget<'_>,
    visitor_data: Option<&str>,
    po_token: Option<&str>,
    reload_token: Option<&str>,
) -> Result<PlayerData, SabrError> {
    let req = player_request(http, video_id, target, visitor_data, po_token, reload_token)?;
    let resp = http.execute(req).await.map_err(|e| e.without_url())?;
    parse_player_data(video_id, resp).await
}

/// Build the InnerTube /player request. The cast's ctt (in `context.user`)
/// is the only authentication; no stored credential is ever attached.
/// `reload_token` answers a RELOAD_PLAYER_RESPONSE (as googlevideo does).
fn player_request(
    http: &reqwest::Client,
    video_id: &str,
    target: PlayerTarget<'_>,
    visitor_data: Option<&str>,
    po_token: Option<&str>,
    reload_token: Option<&str>,
) -> Result<reqwest::Request, SabrError> {
    // --- Build context ---
    // WEB_REMIX uses the page session's full 22+ field INNERTUBE_CONTEXT plus
    // the ctt. Without that context, WEB_REMIX /player returns UNPLAYABLE;
    // yt-dlp always fetches the page first for this reason.
    let (itclient, context, sig_timestamp, session) = match target {
        PlayerTarget::WebRemix(auth) => (
            &WEB_REMIX_CLIENT,
            auth.session.player_context(Some(auth.ctt)),
            auth.session.signature_timestamp,
            Some(auth.session),
        ),
        PlayerTarget::Anonymous(itclient) => {
            (itclient, build_minimal_context(itclient, visitor_data), None, None)
        }
    };
    let is_web_remix = session.is_some();

    // --- Build playbackContext ---
    let mut playback_ctx = serde_json::json!({
        "contentPlaybackContext": {
            "html5Preference": "HTML5_PREF_WANTS",
        }
    });
    if let Some(sts) = sig_timestamp {
        playback_ctx["contentPlaybackContext"]["signatureTimestamp"] = serde_json::json!(sts);
    }
    if let Some(token) = reload_token {
        playback_ctx["reloadPlaybackContext"] =
            serde_json::json!({ "reloadPlaybackParams": { "token": token } });
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
    let url = if is_web_remix {
        "https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
    } else {
        "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
    };
    debug_assert!(is_trusted_google_host(url));

    // Client version from live session (changes frequently) or hardcoded constant.
    let client_version = session
        .and_then(|s| s.client_version())
        .unwrap_or(itclient.client_version);

    tracing::info!(
        "[sabr] innertube /player for {} using {} v{}{}{}{}",
        video_id,
        itclient.client_name,
        client_version,
        if is_web_remix { " +ctt +session" } else { "" },
        if po_token.is_some() { " +potoken" } else { "" },
        if reload_token.is_some() { " +reload" } else { "" },
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
            if let Some(vd) = session.and_then(|s| s.visitor_data()) {
                req = req.header("X-Goog-Visitor-Id", vd);
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
        }
        _ => {}
    }

    Ok(req.json(&body).build().map_err(|e| e.without_url())?)
}

async fn parse_player_data(video_id: &str, resp: reqwest::Response) -> Result<PlayerData, SabrError> {

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
            url: f["url"].as_str().map(str::to_owned),
            signature_cipher: f["signatureCipher"].as_str().map(str::to_owned),
            content_length: f["contentLength"].as_str().and_then(|s| s.parse().ok()),
            is_drc: f["isDrc"].as_bool().unwrap_or(false),
            audio_track_id: f["audioTrack"]["id"].as_str().map(str::to_owned),
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

/// Call InnerTube /player using WEB_REMIX + ctt (+ anonymous page session).
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
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<PlayerResult, SabrError> {
    let gvs = fetch_pot_token(http, Some(video_id)).await;

    // --- Attempt 1: WEB_REMIX + ctt ---
    // Requires nsig decoding of SABR URL (handled by caller).
    match crate::music_session::web_remix_auth(ctt, yt_session) {
        Ok(auth) => {
            tracing::info!("[sabr] WEB_REMIX /player (ctt=true, session=true)");
            match innertube_player(http, video_id, PlayerTarget::WebRemix(auth), None, None, None).await {
                Ok(data) => {
                    tracing::info!("[sabr] WEB_REMIX /player OK");
                    return Ok(PlayerResult { player: data, gvs_token: gvs, client_config: &WEB_REMIX_CLIENT });
                }
                Err(e) => {
                    tracing::warn!("[sabr] WEB_REMIX /player failed: {}", e);
                }
            }
        }
        Err(reason) => tracing::info!("[sabr] {} — skipping WEB_REMIX", reason),
    }

    // --- Fallback: IOS ---
    // No nsig needed but 60s throttle (stream_protection_status=2).
    tracing::info!("[sabr] IOS /player fallback (potoken={})", gvs.is_some());
    let data = innertube_player(http, video_id, PlayerTarget::Anonymous(&IOS_CLIENT), None, None, None).await?;
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

/// Solve the `n` parameter of a WEB_REMIX SABR URL with the session's
/// base.js. A URL without `n` is returned unchanged.
async fn solve_nsig(
    server_url: String,
    session: Option<&crate::music_session::YtMusicSession>,
) -> Result<String, SabrError> {
    solve_nsig_with(server_url, session, decode_nsig).await
}

/// The nsig solver entry point (`decode_nsig`; a fake in tests).
type NsigSolver = fn(&str, &str, &str) -> Result<String, SabrError>;

/// [`solve_nsig`] with an explicit solver; it runs under the solver permit.
async fn solve_nsig_with(
    server_url: String,
    session: Option<&crate::music_session::YtMusicSession>,
    solver: NsigSolver,
) -> Result<String, SabrError> {
    let n_value = server_url.split_once('?').and_then(|(_, query)| {
        query.split('&').find_map(|pair| pair.strip_prefix("n=")).map(str::to_owned)
    });
    let Some(n_val) = n_value else {
        return Ok(server_url);
    };
    let Some(session) = session else {
        tracing::warn!("[sabr] WEB_REMIX URL has n= but no session — CDN will 403");
        return Ok(server_url);
    };
    let Some(js) = session.player_js.clone() else {
        tracing::warn!("[sabr] WEB_REMIX URL has n= but no player_js cached — CDN will 403");
        return Ok(server_url);
    };

    tracing::info!("[sabr] decoding nsig: n len={}", n_val.len());
    let n_clone = n_val.clone();
    let hash = session.player_hash.clone().unwrap_or_default();
    let permit = solver_permit().await?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        solver(&js, &n_clone, &hash)
    })
    .await
    {
        Ok(Ok(decoded)) => {
            tracing::info!("[sabr] nsig decoded: len {} -> {}", n_val.len(), decoded.len());
            Ok(replace_n_param(&server_url, &decoded))
        }
        Ok(Err(e)) => {
            tracing::error!("[sabr] nsig decode failed: {}", e);
            Err(e)
        }
        Err(e) => {
            tracing::error!("[sabr] nsig task panicked: {}", e);
            Err(format!("nsig task panicked: {}", e).into())
        }
    }
}

/// All itags a /player response lists, for the log.
fn listed_itags(formats: &[AdaptiveFormat]) -> Vec<i32> {
    formats.iter().map(|f| f.itag).collect()
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

/// YouTube Music Premium audio, best first: 774 (Opus ~256k), 141 (AAC
/// 256k). A ctt /player response lists them when the casting account has
/// Premium. SABR does not serve them to this receiver's session (it answers
/// with reload requests, then `sabr.no_audio_selected`), but the format's own
/// URL does, as for the music.youtube.com web player (see `url_transport`).
const PREMIUM_ONLY_AUDIO_ITAGS: [i32; 2] = [774, 141];

/// `YTCAST_AUDIO_QUALITY`: `max` (default) tries Premium formats first,
/// `standard` always streams the regular format via SABR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AudioQuality {
    #[default]
    Max,
    Standard,
}

const AUDIO_QUALITY_ENV: &str = "YTCAST_AUDIO_QUALITY";

impl AudioQuality {
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("max") => Self::Max,
            Some("standard") => Self::Standard,
            Some(other) => {
                tracing::warn!("[audio] ignoring {}={:?}: expected max or standard", AUDIO_QUALITY_ENV, other);
                Self::Max
            }
        }
    }
}

/// Premium formats to try via their URL, best first: each listed premium
/// itag once, its non-DRC variant if there are several. Only for a /player
/// call made with the cast's ctt (WEB_REMIX) and quality `max`.
fn premium_candidates(formats: &[AdaptiveFormat], quality: AudioQuality, with_ctt: bool) -> Vec<&AdaptiveFormat> {
    if quality != AudioQuality::Max || !with_ctt {
        return Vec::new();
    }
    PREMIUM_ONLY_AUDIO_ITAGS
        .iter()
        .filter_map(|&itag| {
            formats
                .iter()
                .filter(|f| f.itag == itag && f.mime_type.starts_with("audio/"))
                .min_by_key(|f| f.is_drc)
        })
        .collect()
}

/// Every /player entry for `itag` (Premium itags can come in variants),
/// with the fields that tell them apart. xtags are format descriptors, not
/// secrets.
fn describe_variants(formats: &[AdaptiveFormat], itag: i32) -> String {
    let entries: Vec<String> = formats
        .iter()
        .filter(|f| f.itag == itag)
        .map(|f| {
            format!(
                "xtags={} isDrc={} audioTrack={} lm={}",
                f.xtags.as_deref().unwrap_or("-"),
                f.is_drc,
                f.audio_track_id.as_deref().unwrap_or("-"),
                f.last_modified,
            )
        })
        .collect();
    format!("{} variants: [{}]", itag, entries.join(", "))
}

/// Choose the SABR audio format: the best Opus/WebM, Premium formats skipped.
fn choose_audio_format(formats: &[AdaptiveFormat]) -> Result<&AdaptiveFormat, SabrError> {
    let audio = formats.iter().filter(|f| f.mime_type.starts_with("audio/"));
    if !formats.iter().any(|f| f.mime_type.starts_with("audio/")) {
        return Err("no suitable audio format found".into());
    }
    audio
        .filter(|f| !PREMIUM_ONLY_AUDIO_ITAGS.contains(&f.itag))
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
        .ok_or_else(|| {
            let itags: Vec<String> = PREMIUM_ONLY_AUDIO_ITAGS.iter().map(|i| i.to_string()).collect();
            format!(
                "only YouTube Music Premium audio formats (itag {}) listed; SABR does not serve them to this session",
                itags.join("/"),
            )
            .into()
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
    done: bool,
    /// A RELOAD_PLAYER_RESPONSE seen in the current response; acted on once
    /// the whole response has been read.
    pending_reload: Option<ReloadRequested>,
    stats: StreamStats,
    /// Client version for StreamerContext.clientInfo: the page session's for
    /// WEB_REMIX, so it matches the /player call.
    client_version: String,
    /// When the stream started, for the first-bytes log line.
    started: std::time::Instant,
}

/// Per-song tallies for the log.
#[derive(Debug, Default)]
struct StreamStats {
    media_headers_by_itag: BTreeMap<i32, u32>,
    /// Audio payload bytes forwarded to the consumer, per itag.
    audio_bytes_by_itag: BTreeMap<i32, u64>,
    /// STREAM_PROTECTION_STATUS values in arrival order, repeats collapsed.
    protection_statuses: Vec<i32>,
    reloads: u32,
}

impl SabrState {
    /// Use the page session's client version (WEB_REMIX) in clientInfo.
    fn with_client_version(mut self, version: Option<String>) -> Self {
        if let Some(version) = version {
            self.client_version = version;
        }
        self
    }

    fn new(
        server_url: String,
        audio_format: &AdaptiveFormat,
        video_format: &AdaptiveFormat,
        ustreamer_config: Vec<u8>,
        client_config: &'static InnerTubeClient,
    ) -> Self {
        let audio_format_id = format_id(audio_format);
        let video_format_id = format_id(video_format);

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
            done: false,
            pending_reload: None,
            stats: StreamStats::default(),
            client_version: client_config.client_version.to_owned(),
            started: std::time::Instant::now(),
        }
    }

    /// Build a SABR VideoPlaybackAbrRequest protobuf.
    ///
    /// Matches the TypeScript googlevideo reference behavior:
    /// - Always includes a video format with MAX discard BufferedRange
    /// - Only includes audio in selectedFormatIds after FORMAT_INIT received
    /// - Always includes both in preferredAudio/VideoFormatIds
    fn build_request(&self) -> vs::VideoPlaybackAbrRequest {
        // Empty device fields are left out, as googlevideo does.
        let non_empty = |v: &str| (!v.is_empty()).then(|| v.to_owned());
        let client_info = vs::streamer_context::ClientInfo {
            device_make: non_empty(self.client_config.device_make),
            device_model: non_empty(self.client_config.device_model),
            client_name: Some(self.client_config.client_name_id),
            client_version: Some(self.client_version.clone()),
            os_name: non_empty(self.client_config.os_name),
            os_version: non_empty(self.client_config.os_version),
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

        // -- selectedFormatIds (googlevideo: initialization_format_ids) --
        // Discard trick: video (discarded) is always listed. Audio is only
        // added after FORMAT_INITIALIZATION_METADATA is received.
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

fn format_id(format: &AdaptiveFormat) -> misc::FormatId {
    misc::FormatId {
        itag: Some(format.itag),
        last_modified: Some(format.last_modified),
        xtags: format.xtags.clone(),
    }
}

/// Point the stream at a reloaded /player response, as googlevideo does:
/// new streaming URL and ustreamer config, SABR contexts cleared, format ids
/// refreshed when the new response still lists the itag; progress, PoToken
/// and playback cookie carry over. Only a cookie that names another audio
/// format (`audio_fmt`) than the one streaming is dropped, so the new
/// session is not told a different format was selected.
fn apply_reload(state: &mut SabrState, player: &ReloadedPlayer) -> Result<(), SabrError> {
    check_streaming_url(&player.server_url)?;
    state.server_url = player.server_url.clone();
    state.ustreamer_config = player.ustreamer_config.clone();
    let cookie_audio = state
        .playback_cookie
        .as_deref()
        .and_then(|bytes| vs::PlaybackCookie::decode(bytes).ok())
        .and_then(|cookie| cookie.audio_fmt)
        .and_then(|fmt| fmt.itag);
    if cookie_audio.is_some_and(|itag| itag != state.audio_itag) {
        tracing::info!("[sabr] reload: dropping a playback cookie for itag {:?}", cookie_audio);
        state.playback_cookie = None;
    }
    state.sabr_contexts.clear();
    state.sabr_contexts_send.clear();
    state.sabr_contexts_discard.clear();
    for id in [&mut state.audio_format_id, &mut state.video_format_id] {
        let same_itag = |f: &&AdaptiveFormat| Some(f.itag) == id.itag;
        let same_variant = player.formats.iter().filter(same_itag).find(|f| f.xtags == id.xtags);
        if let Some(f) = same_variant.or_else(|| player.formats.iter().find(same_itag)) {
            *id = format_id(f);
        }
    }
    Ok(())
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

    *state.stats.media_headers_by_itag.entry(itag).or_default() += 1;
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
    if state.stats.audio_bytes_by_itag.is_empty() {
        tracing::info!(
            "audio: itag {} (standard) via sabr, first bytes after {} ms",
            info.itag, state.started.elapsed().as_millis(),
        );
    }
    *state.stats.audio_bytes_by_itag.entry(info.itag).or_default() += payload.len() as u64;

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

/// A SABR_ERROR part from the server (retried like transport errors).
#[derive(Debug)]
struct SabrServerError {
    error_type: String,
    code: i32,
}

impl std::fmt::Display for SabrServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SABR error: type={}, code={}", self.error_type, self.code)
    }
}

impl std::error::Error for SabrServerError {}

fn handle_sabr_error(data: &[u8]) -> Result<(), SabrError> {
    let err = vs::SabrError::decode(data)?;
    let error_type = err.r#type.unwrap_or_else(|| "unknown".to_owned());
    let code = err.code.unwrap_or(0);
    tracing::warn!("[sabr] SABR_ERROR type={} code={}", error_type, code);
    Err(SabrServerError { error_type, code }.into())
}

fn format_ids_for_log(ids: &[misc::FormatId]) -> String {
    let ids: Vec<String> = ids
        .iter()
        .map(|id| {
            format!(
                "{} lm={} xtags={}",
                id.itag.unwrap_or(0),
                id.last_modified.unwrap_or(0),
                id.xtags.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    format!("[{}]", ids.join(", "))
}

/// The format fields of a SABR request for the log: format ids and the itags
/// the playback cookie names. No tokens, cookie bytes or URLs.
fn describe_request_formats(req: &vs::VideoPlaybackAbrRequest) -> String {
    let cookie = req
        .streamer_context
        .as_ref()
        .and_then(|c| c.playback_cookie.as_deref())
        .and_then(|bytes| vs::PlaybackCookie::decode(bytes).ok());
    let cookie_itag = |fmt: Option<&misc::FormatId>| {
        fmt.and_then(|f| f.itag).map_or("-".to_owned(), |i| i.to_string())
    };
    let cookie = match cookie {
        Some(c) => format!("audio={} video={}", cookie_itag(c.audio_fmt.as_ref()), cookie_itag(c.video_fmt.as_ref())),
        None => "none".to_owned(),
    };
    format!(
        "selected audio {} selected video {} init {} cookie {}",
        format_ids_for_log(&req.preferred_audio_format_ids),
        format_ids_for_log(&req.preferred_video_format_ids),
        format_ids_for_log(&req.selected_format_ids),
        cookie,
    )
}

fn handle_playback_start_policy(data: &[u8]) -> Result<(), SabrError> {
    let policy = vs::PlaybackStartPolicy::decode(data)?;
    let readahead = |p: &Option<vs::playback_start_policy::MinReadaheadPolicy>| {
        p.as_ref().and_then(|p| p.min_readahead_ms).unwrap_or(0)
    };
    tracing::debug!(
        "[sabr] playback start policy: start_readahead_ms={}, start_timeout_ms={}, resume_readahead_ms={}, resume_timeout_ms={}",
        readahead(&policy.start_min_readahead_policy),
        policy.start_timeout_ms.unwrap_or(0),
        readahead(&policy.resume_min_readahead_policy),
        policy.resume_timeout_ms.unwrap_or(0),
    );
    Ok(())
}

/// The token is opaque server state for the next /player call; only its
/// length is logged.
fn handle_reload_player_response(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let ctx = vs::ReloadPlaybackContext::decode(data)?;
    let token = ctx.reload_playback_params.and_then(|p| p.token);
    tracing::info!(
        "[sabr] RELOAD_PLAYER_RESPONSE (token len={})",
        token.as_deref().map_or(0, str::len),
    );
    state.pending_reload = Some(ReloadRequested { token });
    Ok(())
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

/// Status 1 only says the session is not (yet) asking for attestation; it says
/// nothing about format entitlement, so the log just states the facts.
fn handle_stream_protection(state: &mut SabrState, data: &[u8]) -> Result<(), SabrError> {
    let sps = vs::StreamProtectionStatus::decode(data)?;
    let status = sps.status.unwrap_or(0);
    if state.stats.protection_statuses.last() != Some(&status) {
        state.stats.protection_statuses.push(status);
    }
    let sent = if state.po_token.is_some() { "PoToken sent" } else { "no PoToken sent" };
    match status {
        0 => tracing::info!("[sabr] stream_protection_status=0 (no protection, {})", sent),
        1 => tracing::info!("[sabr] stream_protection_status=1 (OK, {})", sent),
        2 => tracing::warn!("[sabr] stream_protection_status=2 (attestation pending, {} — ~60s then throttle)", sent),
        3 => tracing::error!("[sabr] stream_protection_status=3 (attestation required, {} — no more media)", sent),
        n => tracing::warn!("[sabr] stream_protection_status={} (unknown, {})", n, sent),
    }
    Ok(())
}

/// The server wants a fresh /player response (sent with `token` in
/// `playbackContext.reloadPlaybackContext`) before it serves more media.
/// Repeating the same SABR request only gets the same answer, so this is
/// never retried as is.
struct ReloadRequested {
    token: Option<String>,
}

impl std::fmt::Debug for ReloadRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadRequested")
            .field("token_len", &self.token.as_deref().map_or(0, str::len))
            .finish()
    }
}

impl std::fmt::Display for ReloadRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server requested a player response reload (RELOAD_PLAYER_RESPONSE); no media will follow")
    }
}

impl std::error::Error for ReloadRequested {}

/// Transport and transient errors are retried; a reload request is final.
fn is_retryable(e: &SabrError) -> bool {
    !e.is::<ReloadRequested>()
}

/// Apply one UMP part to the stream state. Returns audio bytes to forward.
fn handle_part(state: &mut SabrState, part: &ump::UmpPart) -> Result<Option<Bytes>, SabrError> {
    match part.part_type {
        ump::FORMAT_INITIALIZATION_METADATA => handle_format_init_metadata(state, &part.data)?,
        ump::MEDIA_HEADER => handle_media_header(state, &part.data)?,
        ump::MEDIA => return Ok(handle_media(state, &part.data)),
        ump::MEDIA_END => handle_media_end(state, &part.data),
        ump::NEXT_REQUEST_POLICY => handle_next_request_policy(state, &part.data)?,
        ump::SABR_REDIRECT => handle_sabr_redirect(state, &part.data)?,
        ump::SABR_ERROR => handle_sabr_error(&part.data)?,
        ump::RELOAD_PLAYER_RESPONSE => handle_reload_player_response(state, &part.data)?,
        ump::PLAYBACK_START_POLICY => handle_playback_start_policy(&part.data)?,
        ump::SABR_CONTEXT_UPDATE => handle_context_update(state, &part.data)?,
        ump::STREAM_PROTECTION_STATUS => handle_stream_protection(state, &part.data)?,
        ump::SABR_CONTEXT_SENDING_POLICY => handle_context_sending_policy(state, &part.data)?,
        other => tracing::debug!("[sabr] unhandled UMP type={}, {} bytes", other, part.data.len()),
    }
    Ok(None)
}

/// Called once a whole response has been read: logs its part sequence and
/// turns a RELOAD_PLAYER_RESPONSE seen in it into a `ReloadRequested` error.
fn finish_response(state: &mut SabrState, part_ids: &[u32]) -> Result<(), SabrError> {
    tracing::debug!(
        "[sabr] response #{} parts: {}",
        state.request_number,
        ump::summarize_part_ids(part_ids),
    );
    match state.pending_reload.take() {
        Some(reload) => Err(reload.into()),
        None => Ok(()),
    }
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
    tracing::debug!(
        "[sabr] request #{} formats: {}",
        state.request_number,
        describe_request_formats(&req_proto),
    );
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
    let req = http
        .post(&url)
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/vnd.yt-ump")
        .header("Accept-Encoding", "identity");

    // Signed streaming URLs are credentials; reqwest errors would print them.
    let resp = req.body(encoded).send().await.map_err(|e| e.without_url())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body_len = resp.bytes().await.map(|b| b.len()).unwrap_or(0);
        return Err(http_error_message(status, state, body_len).into());
    }

    let mut parser = UmpParser::new();
    let mut byte_stream = resp.bytes_stream();
    let mut part_ids = Vec::new();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk = chunk_result.map_err(|e| e.without_url())?;
        parser.push(&chunk);

        while let Some(part) = parser.next_part() {
            part_ids.push(part.part_type);
            if let Some(audio_bytes) = handle_part(state, &part)? {
                if tx.send(audio_bytes).await.is_err() {
                    tracing::info!("[sabr] receiver dropped, stopping");
                    return Ok(false);
                }
            }
        }
    }

    let finished = finish_response(state, &part_ids);
    state.request_number += 1;
    finished?;

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

/// Stream audio for a YouTube video.
///
/// With a cast ctt and `YTCAST_AUDIO_QUALITY=max` (default), Premium formats
/// listed by /player (with a known length) are tried first via their own URL
/// (774, then 141; see `url_transport`); otherwise, or if none delivers a
/// first byte, the regular format streams via SABR. Returns metadata and a
/// channel receiver of audio bytes, fed by a background task; the channel
/// closes when the song ends or fails, and dropping the receiver stops the
/// download.
///
/// `start_offset > 0` is a byte range request (MPD seeking). It is served
/// only from the format pinned when the song started at byte 0 (same itag,
/// same file, no new /player call); without a pin it fails, and it never
/// switches format mid-song. An offset at or past the end is
/// `RangeNotSatisfiable`.
///
/// Requests go through `credential_client()` so that redirects cannot carry
/// the ctt or tokens off Google hosts.
pub async fn stream_audio(
    video_id: &str,
    ctt: Option<&str>,
    yt_session: Option<std::sync::Arc<crate::music_session::YtMusicSession>>,
    start_offset: u64,
) -> Result<(SabrStreamInfo, mpsc::Receiver<Bytes>), SabrError> {
    stream_audio_in(pins::global(), video_id, ctt, yt_session, start_offset).await
}

/// Forget every pinned format: the cast ended or changed hands, and a pin
/// holds that cast's ctt.
pub fn clear_pins() {
    pins::global().clear();
}

/// [`stream_audio`] with an explicit pin store.
async fn stream_audio_in(
    store: &std::sync::Arc<pins::PinStore>,
    video_id: &str,
    ctt: Option<&str>,
    yt_session: Option<std::sync::Arc<crate::music_session::YtMusicSession>>,
    start_offset: u64,
) -> Result<(SabrStreamInfo, mpsc::Receiver<Bytes>), SabrError> {
    let started = std::time::Instant::now();
    // Pins made by this request must not outlive a cast change during it.
    let generation = store.generation();
    // A song this cast already streams via url (a seek, or MPD reopening it
    // at byte 0) continues with the same file.
    if let Some(pin) = ctt.and_then(|ctt| store.pinned(video_id, ctt)) {
        return stream_pinned(store, pin, start_offset, started).await;
    }
    if start_offset > 0 {
        return Err(format!("seek to byte {start_offset}: no pinned format for this song and cast").into());
    }
    store.unpin(video_id);
    if ctt.is_some() {
        tracing::info!("[sabr] stream_audio for {} with ctt=true", video_id);
    }
    let client = credential_client();

    // 1. Call InnerTube /player to get streaming metadata (+ GVS PoToken if bgutil-pot is running).
    // TODO(follow-up): make this /player fetch injectable, so the glue from
    // here into `start_premium` and the SABR start is unit-tested too.
    let result = get_player_data(client, video_id, ctt, yt_session.as_deref()).await?;
    let player = result.player;
    let client_config = result.client_config;
    let gvs_po_token = result.gvs_token.map(|g| g.po_token);

    // 2. Premium formats via their URL, best first.
    let with_ctt = client_config.client_name == "WEB_REMIX";
    if let (Some(ctt), Some(session)) = (ctt, yt_session.as_ref()) {
        let url_source = PremiumUrlSource::new(client.clone(), video_id, ctt, session.clone(), gvs_po_token.clone());
        let song = SongMeta { title: player.title.clone(), duration_secs: player.duration_secs };
        let premium = start_premium(store, generation, &url_source, &player.formats, with_ctt, env_var, &song, |format| {
            tracing::debug!("[audio] {}", describe_variants(&player.formats, format.itag));
            url_source.clone().stream(format, store.clone())
        })
        .await;
        if let Some((itag, info, rx)) = premium {
            tracing::info!(
                "audio: itag {} (premium) via url, first bytes after {} ms",
                itag, started.elapsed().as_millis(),
            );
            return Ok((info, rx));
        }
    }

    // 3. SABR: the regular audio format + a video format for the discard trick.
    let audio_fmt = choose_audio_format(&player.formats)?.clone();
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
        content_length: None,
        start_offset: 0,
    };

    // 4. Channel for streaming audio bytes to the caller.
    let (tx, rx) = mpsc::channel::<Bytes>(64);

    // 5. Decode nsig if WEB_REMIX SABR URL has `n` parameter.
    let server_url = if client_config.client_name == "WEB_REMIX" {
        solve_nsig(player.server_abr_streaming_url, yt_session.as_deref()).await?
    } else {
        player.server_abr_streaming_url
    };

    check_streaming_url(&server_url)?;

    // 6. Spawn the SABR streaming loop.
    let http = client.clone();
    let ustreamer_config = player.ustreamer_config;
    let session_version = sabr_client_version(client_config, yt_session.as_deref());
    let reload_auth = match (ctt, yt_session) {
        (Some(ctt), Some(session)) if client_config.client_name == "WEB_REMIX" => {
            Some((ctt.to_owned(), session))
        }
        _ => None,
    };
    let mut reloader = HttpReloader { http: http.clone(), video_id: video_id.to_owned(), auth: reload_auth };

    let video_fmt_clone = video_fmt.clone();
    tokio::spawn(async move {
        let result = sabr_loop(
            &http, server_url, &audio_fmt, &video_fmt_clone,
            ustreamer_config, client_config, session_version, gvs_po_token, &tx, &mut reloader, started,
        ).await;

        match result {
            Ok(()) => tracing::info!("[sabr] streaming complete"),
            Err(e) => tracing::error!("[sabr] streaming error: {}", e),
        }
        // tx is dropped here, closing the channel.
    });

    Ok((info, rx))
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The Premium formats to try for this song, from the `YTCAST_AUDIO_QUALITY`
/// setting read through `env`. Only formats with a known content length: a
/// seek (byte range) must be answerable.
fn premium_plan(
    formats: &[AdaptiveFormat],
    with_ctt: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<&AdaptiveFormat> {
    let quality = AudioQuality::parse(env(AUDIO_QUALITY_ENV).as_deref());
    premium_candidates(formats, quality, with_ctt)
        .into_iter()
        .filter(|f| f.content_length.is_some_and(|len| len > 0))
        .collect()
}

/// The page session's client version for SABR clientInfo, when the song
/// streams via WEB_REMIX (it must match the /player call).
fn sabr_client_version(
    client_config: &InnerTubeClient,
    session: Option<&crate::music_session::YtMusicSession>,
) -> Option<String> {
    match client_config.client_name {
        "WEB_REMIX" => session.and_then(|s| s.client_version()).map(str::to_owned),
        _ => None,
    }
}

/// Title and duration for the stream info and the pin.
struct SongMeta {
    title: String,
    duration_secs: u64,
}

/// The byte-0 Premium step after /player (the caller has already dropped
/// any older pin for the song): the Premium formats from the setting are
/// tried in order, and the one that delivers is pinned for this song and
/// cast, unless the cast changed since `generation`. `None` = SABR.
#[allow(clippy::too_many_arguments)]
async fn start_premium<'f, F, Fut>(
    store: &pins::PinStore,
    generation: u64,
    source: &PremiumUrlSource,
    formats: &'f [AdaptiveFormat],
    with_ctt: bool,
    env: impl Fn(&str) -> Option<String>,
    song: &SongMeta,
    attempt: F,
) -> Option<(i32, SabrStreamInfo, mpsc::Receiver<Bytes>)>
where
    F: FnMut(&'f AdaptiveFormat) -> Fut,
    Fut: std::future::Future<Output = Result<(mpsc::Receiver<Bytes>, String), SabrError>>,
{
    let candidates = premium_plan(formats, with_ctt, env);
    let AudioSource::Premium(format, (rx, url)) = pick_source(&candidates, attempt).await else {
        return None;
    };
    let pinned = store.pin_if_current(generation, pins::PinnedFormat {
        video_id: source.video_id.clone(),
        itag: format.itag,
        url,
        content_length: format.content_length.unwrap_or_default(),
        last_modified: format.last_modified,
        mime_type: format.mime_type.clone(),
        title: song.title.clone(),
        duration_secs: song.duration_secs,
        source: source.clone(),
    });
    if !pinned {
        tracing::info!("[audio] cast changed while itag {} started; not pinning it", format.itag);
    }
    let info = SabrStreamInfo {
        title: song.title.clone(),
        duration_secs: song.duration_secs,
        mime_type: format.mime_type.clone(),
        content_length: format.content_length,
        start_offset: 0,
    };
    Some((format.itag, info, rx))
}

/// The outcome of trying the Premium formats.
enum AudioSource<'f, T> {
    Premium(&'f AdaptiveFormat, T),
    Sabr,
}

/// Try each Premium format in order; the first that delivers is used, else
/// SABR. Failures are warnings naming what comes next. A decipher failure
/// goes straight to SABR: every format uses the same base.js.
async fn pick_source<'f, T, F, Fut>(formats: &[&'f AdaptiveFormat], mut attempt: F) -> AudioSource<'f, T>
where
    F: FnMut(&'f AdaptiveFormat) -> Fut,
    Fut: std::future::Future<Output = Result<T, SabrError>>,
{
    const SABR: &str = "the regular format via sabr";
    for (i, format) in formats.iter().enumerate() {
        match attempt(format).await {
            Ok(value) => return AudioSource::Premium(format, value),
            Err(e) if e.is::<super::url_transport::DecipherError>() => {
                tracing::warn!("audio: itag {} (premium) via url failed: {}; trying {}", format.itag, e, SABR);
                return AudioSource::Sabr;
            }
            Err(e) => {
                let next = formats.get(i + 1).map_or(SABR.to_owned(), |f| format!("itag {} via url", f.itag));
                tracing::warn!("audio: itag {} (premium) via url failed: {}; trying {}", format.itag, e, next);
            }
        }
    }
    AudioSource::Sabr
}

/// Continue the pinned format from `start_offset` (a seek, or byte 0 again):
/// same file, no /player call, no solver run, never another format.
async fn stream_pinned(
    store: &std::sync::Arc<pins::PinStore>,
    pin: pins::PinnedFormat,
    start_offset: u64,
    started: std::time::Instant,
) -> Result<(SabrStreamInfo, mpsc::Receiver<Bytes>), SabrError> {
    if start_offset >= pin.content_length {
        return Err(RangeNotSatisfiable { content_length: pin.content_length }.into());
    }
    let rx = super::url_transport::RangeFetch {
        http: pin.source.http.clone(),
        url: pin.url.clone(),
        content_length: Some(pin.content_length),
        chunk_size: URL_CHUNK_SIZE,
        itag: pin.itag,
        start_offset,
        refresher: Box::new(PremiumUrlRefresher::for_pin(&pin, store.clone())),
        read_timeout: super::url_transport::READ_TIMEOUT,
    }
    .start()
    .await?;
    tracing::info!(
        "audio: itag {} (premium) via url from byte {}, first bytes after {} ms",
        pin.itag, start_offset, started.elapsed().as_millis(),
    );
    let info = SabrStreamInfo {
        title: pin.title.clone(),
        duration_secs: pin.duration_secs,
        mime_type: pin.mime_type.clone(),
        content_length: Some(pin.content_length),
        start_offset,
    };
    Ok((info, rx))
}

/// A byte range starting at or past the end of the pinned format.
#[derive(Debug)]
pub struct RangeNotSatisfiable {
    pub content_length: u64,
}

impl std::fmt::Display for RangeNotSatisfiable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "range starts past the end ({} bytes)", self.content_length)
    }
}

impl std::error::Error for RangeNotSatisfiable {}

/// Which format a song streams with, so a seek (byte range request) gets
/// the same file. In memory only, at most 4 songs, each expiring 5 minutes
/// after last use (the download touches it at every range), keyed by video
/// and the cast's ctt, and cleared when the cast ends. A pin holds that ctt
/// for the URL refresh, like the stream itself; its URL is never logged.
mod pins {
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    pub(super) struct PinnedFormat {
        pub video_id: String,
        pub itag: i32,
        pub url: String,
        pub content_length: u64,
        pub last_modified: u64,
        pub mime_type: String,
        pub title: String,
        pub duration_secs: u64,
        pub source: super::PremiumUrlSource,
    }

    impl PinnedFormat {
        fn ctt(&self) -> &str {
            &self.source.ctt
        }
    }

    pub(super) struct PinStore {
        pins: Mutex<Vec<(PinnedFormat, Instant)>>,
        /// Bumped by `clear` (the cast ended or changed), under the same lock.
        generation: Mutex<u64>,
        max_pins: usize,
        ttl: Duration,
    }

    impl Default for PinStore {
        fn default() -> Self {
            Self {
                pins: Mutex::new(Vec::new()),
                generation: Mutex::new(0),
                max_pins: 4,
                ttl: Duration::from_secs(5 * 60),
            }
        }
    }

    pub(super) fn global() -> &'static Arc<PinStore> {
        static GLOBAL: LazyLock<Arc<PinStore>> = LazyLock::new(Arc::default);
        &GLOBAL
    }

    impl PinStore {
        fn locked(&self) -> std::sync::MutexGuard<'_, Vec<(PinnedFormat, Instant)>> {
            let mut pins = self.pins.lock().unwrap();
            let ttl = self.ttl;
            pins.retain(|(_, touched)| touched.elapsed() < ttl);
            pins
        }

        pub(super) fn generation(&self) -> u64 {
            *self.generation.lock().unwrap()
        }

        /// [`Self::pin`], unless `clear` ran since `generation` was read: a
        /// request that started under the previous cast must not pin.
        pub(super) fn pin_if_current(&self, generation: u64, format: PinnedFormat) -> bool {
            let current = self.generation.lock().unwrap();
            if *current != generation {
                return false;
            }
            self.pin(format);
            true
        }

        /// Pin `format` for its song, replacing an older pin for it; when full,
        /// the least recently used pin goes.
        pub(super) fn pin(&self, format: PinnedFormat) {
            let mut pins = self.locked();
            pins.retain(|(p, _)| p.video_id != format.video_id);
            if pins.len() >= self.max_pins {
                if let Some(oldest) = pins.iter().enumerate().min_by_key(|(_, (_, t))| *t).map(|(i, _)| i) {
                    pins.remove(oldest);
                }
            }
            pins.push((format, Instant::now()));
        }

        pub(super) fn unpin(&self, video_id: &str) {
            self.locked().retain(|(p, _)| p.video_id != video_id);
        }

        pub(super) fn clear(&self) {
            let mut generation = self.generation.lock().unwrap();
            *generation += 1;
            self.pins.lock().unwrap().clear();
        }

        /// The pin for `video_id` made by the cast with `ctt`, refreshing its
        /// expiry. A pin from another cast is dropped.
        pub(super) fn pinned(&self, video_id: &str, ctt: &str) -> Option<PinnedFormat> {
            let mut pins = self.locked();
            let i = pins.iter().position(|(p, _)| p.video_id == video_id)?;
            if pins[i].0.ctt() != ctt {
                pins.remove(i);
                return None;
            }
            pins[i].1 = Instant::now();
            Some(pins[i].0.clone())
        }

        /// Keep the pin alive while its song downloads.
        pub(super) fn touch(&self, video_id: &str, ctt: &str) {
            if let Some((_, touched)) = self.locked().iter_mut().find(|(p, _)| p.video_id == video_id && p.ctt() == ctt) {
                *touched = Instant::now();
            }
        }

        /// A refreshed URL, stored only into the pin for the same song, cast
        /// and file.
        pub(super) fn update_url(&self, video_id: &str, ctt: &str, itag: i32, last_modified: u64, url: &str) {
            if let Some((pin, _)) = self.locked().iter_mut().find(|(p, _)| {
                p.video_id == video_id && p.ctt() == ctt && p.itag == itag && p.last_modified == last_modified
            }) {
                pin.url = url.to_owned();
            }
        }

        #[cfg(test)]
        pub(super) fn age(&self, video_id: &str, by: Duration) {
            if let Some((_, touched)) = self.pins.lock().unwrap().iter_mut().find(|(p, _)| p.video_id == video_id) {
                *touched -= by;
            }
        }

        #[cfg(test)]
        pub(super) fn video_ids(&self) -> Vec<String> {
            self.locked().iter().map(|(p, _)| p.video_id.clone()).collect()
        }

        #[cfg(test)]
        pub(super) fn url_of(&self, video_id: &str) -> Option<String> {
            self.locked().iter().find(|(p, _)| p.video_id == video_id).map(|(p, _)| p.url.clone())
        }
    }
}

/// Byte-range size for the url transport (the web player uses ~0.5-1 MiB).
const URL_CHUNK_SIZE: u64 = 1024 * 1024;

/// One solver run at a time (Premium URLs and SABR nsig): each runs QuickJS
/// with up to 128-256 MiB, and parallel requests must not stack. The permit
/// is held by the blocking solver task itself, so a cancelled request does
/// not release it while QuickJS still runs.
async fn solver_permit() -> Result<tokio::sync::OwnedSemaphorePermit, SabrError> {
    static PERMITS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)));
    Ok(PERMITS.clone().acquire_owned().await.map_err(|_| "solver closed")?)
}

/// Everything needed to (re)resolve a Premium format URL for one song: the
/// cast's ctt stays here, in memory, only while the song (or its pin) lives.
#[derive(Clone)]
struct PremiumUrlSource {
    http: reqwest::Client,
    video_id: String,
    ctt: String,
    session: std::sync::Arc<crate::music_session::YtMusicSession>,
    po_token: Option<String>,
    /// Client version for `cver`: the page session's, as /player used.
    cver: String,
    decipher: std::sync::Arc<dyn super::url_transport::Decipher>,
}

impl PremiumUrlSource {
    fn new(
        http: reqwest::Client,
        video_id: &str,
        ctt: &str,
        session: std::sync::Arc<crate::music_session::YtMusicSession>,
        po_token: Option<String>,
    ) -> Self {
        let cver = session.client_version().unwrap_or(WEB_REMIX_CLIENT.client_version).to_owned();
        let decipher = std::sync::Arc::new(super::url_transport::PlayerJsDecipher::new(session.clone()));
        Self { http, video_id: video_id.to_owned(), ctt: ctt.to_owned(), session, po_token, cver, decipher }
    }

    /// Decipher the format's URL (signature into `sp`, `n`, plus pot, cpn,
    /// cver) on a blocking thread, one solver run at a time.
    async fn resolve(&self, format: &AdaptiveFormat) -> Result<String, SabrError> {
        use super::url_transport as ut;
        tracing::debug!(
            "[url] itag {} source {}",
            format.itag,
            ut::describe_source(format.url.as_deref(), format.signature_cipher.as_deref()),
        );
        let source = ut::format_source(format.url.as_deref(), format.signature_cipher.as_deref())?;
        let decipher = self.decipher.clone();
        let cver = self.cver.clone();
        let pot = self.po_token.clone();
        let permit = solver_permit().await?;
        let started = std::time::Instant::now();
        let url = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ut::resolve_url(&source, decipher.as_ref(), pot.as_deref(), &cver, &ut::generate_cpn())
        })
        .await
        .map_err(|e| format!("decipher task: {e}"))??;
        tracing::info!(
            "[url] itag {} deciphered in {} ms (host {}, params {})",
            format.itag, started.elapsed().as_millis(), host_for_log(&url),
            ut::param_names(&url).join(", "),
        );
        Ok(url)
    }

    /// Start streaming `format` from byte 0; `Err` before any byte. Also
    /// returns the resolved URL, for the pin.
    async fn stream(
        self,
        format: &AdaptiveFormat,
        store: std::sync::Arc<pins::PinStore>,
    ) -> Result<(mpsc::Receiver<Bytes>, String), SabrError> {
        let url = self.resolve(format).await?;
        let rx = super::url_transport::RangeFetch {
            http: self.http.clone(),
            url: url.clone(),
            content_length: format.content_length,
            chunk_size: URL_CHUNK_SIZE,
            itag: format.itag,
            start_offset: 0,
            refresher: Box::new(PremiumUrlRefresher {
                itag: format.itag,
                content_length: format.content_length.unwrap_or_default(),
                last_modified: format.last_modified,
                source: self,
                store,
            }),
            read_timeout: super::url_transport::READ_TIMEOUT,
        }
        .start()
        .await?;
        Ok((rx, url))
    }
}

/// Fresh URL for a song whose format URL stopped working (expired): a new
/// WEB_REMIX /player call with the same ctt, deciphered again. The refreshed
/// format must be the same file (length and last-modified), else the song
/// would continue with different bytes.
struct PremiumUrlRefresher {
    source: PremiumUrlSource,
    itag: i32,
    content_length: u64,
    last_modified: u64,
    store: std::sync::Arc<pins::PinStore>,
}

impl PremiumUrlRefresher {
    fn for_pin(pin: &pins::PinnedFormat, store: std::sync::Arc<pins::PinStore>) -> Self {
        Self {
            source: pin.source.clone(),
            itag: pin.itag,
            content_length: pin.content_length,
            last_modified: pin.last_modified,
            store,
        }
    }

    /// Store a refreshed URL in this song's pin.
    fn record_url(&self, url: &str) {
        let src = &self.source;
        self.store.update_url(&src.video_id, &src.ctt, self.itag, self.last_modified, url);
    }

    /// The listed format that is the same file as the one streaming.
    fn same_file<'f>(&self, formats: &'f [AdaptiveFormat]) -> Result<&'f AdaptiveFormat, SabrError> {
        formats
            .iter()
            .filter(|f| f.itag == self.itag)
            .find(|f| f.content_length == Some(self.content_length) && f.last_modified == self.last_modified)
            .ok_or_else(|| format!("refreshed /player has no itag {} with the same length and last-modified", self.itag).into())
    }
}

impl super::url_transport::UrlRefresher for PremiumUrlRefresher {
    fn refresh(&mut self) -> super::url_transport::RefreshFuture<'_> {
        Box::pin(async move {
            let src = &self.source;
            let auth = crate::music_session::web_remix_auth(Some(&src.ctt), Some(&src.session))?;
            let player = innertube_player(&src.http, &src.video_id, PlayerTarget::WebRemix(auth), None, None, None).await?;
            let format = self.same_file(&player.formats)?;
            let url = src.resolve(format).await?;
            self.record_url(&url);
            Ok(url)
        })
    }

    fn keep_alive(&self) {
        self.store.touch(&self.source.video_id, &self.source.ctt);
    }
}

/// Inner SABR request loop. Runs until all audio segments are downloaded,
/// the downstream receiver disconnects, or an unrecoverable error occurs.
///
/// Transient network errors are retried with exponential backoff (up to 5
/// retries). This matches the TypeScript reference behavior.
#[allow(clippy::too_many_arguments)]
async fn sabr_loop(
    http: &reqwest::Client,
    server_url: String,
    audio_format: &AdaptiveFormat,
    video_format: &AdaptiveFormat,
    ustreamer_config: Vec<u8>,
    client_config: &'static InnerTubeClient,
    session_version: Option<String>,
    po_token: Option<String>,
    tx: &mpsc::Sender<Bytes>,
    reloader: &mut HttpReloader,
    started: std::time::Instant,
) -> Result<(), SabrError> {
    let mut state = SabrState::new(
        server_url, audio_format, video_format, ustreamer_config, client_config,
    )
    .with_client_version(session_version);
    state.started = started;

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

    drive_loop(&mut state, tx, &mut HttpCycle { http, tx }, reloader).await
}

/// One SABR request/response round trip, boxed so `drive_loop` stays `Send`.
type CycleFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, SabrError>> + Send + 'a>>;

/// Performs one round trip for `drive_loop`; `Ok(false)` ends the stream.
trait SabrCycle {
    fn run<'a>(&'a mut self, state: &'a mut SabrState) -> CycleFuture<'a>;
}

struct HttpCycle<'c> {
    http: &'c reqwest::Client,
    tx: &'c mpsc::Sender<Bytes>,
}

impl SabrCycle for HttpCycle<'_> {
    fn run<'a>(&'a mut self, state: &'a mut SabrState) -> CycleFuture<'a> {
        Box::pin(sabr_request_cycle(self.http, state, self.tx))
    }
}

/// Streaming data from a reloaded /player response; the URL is nsig-solved
/// and on a trusted host.
struct ReloadedPlayer {
    server_url: String,
    ustreamer_config: Vec<u8>,
    formats: Vec<AdaptiveFormat>,
}

type ReloadFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<ReloadedPlayer, SabrError>> + Send + 'a>>;

/// Fetches a fresh /player response, with the server's reload token if any.
trait PlayerReloader {
    fn fetch<'a>(&'a mut self, token: Option<&'a str>) -> ReloadFuture<'a>;
}

/// Reloads via WEB_REMIX with the same ctt and page session as the first
/// /player call. The ctt lives here, in memory, only for this song's stream.
struct HttpReloader {
    http: reqwest::Client,
    video_id: String,
    /// `None` when the song did not stream via WEB_REMIX: no reload possible.
    auth: Option<(String, std::sync::Arc<crate::music_session::YtMusicSession>)>,
}

impl PlayerReloader for HttpReloader {
    fn fetch<'a>(&'a mut self, token: Option<&'a str>) -> ReloadFuture<'a> {
        Box::pin(async move {
            let (ctt, session) = self
                .auth
                .as_ref()
                .ok_or("player reload needs WEB_REMIX (cast ctt and YouTube Music session)")?;
            let auth = crate::music_session::web_remix_auth(Some(ctt), Some(session))?;
            let player = innertube_player(
                &self.http, &self.video_id, PlayerTarget::WebRemix(auth), None, None, token,
            )
            .await?;
            tracing::info!("[sabr] reloaded /player lists itags {:?}", listed_itags(&player.formats));
            let server_url = solve_nsig(player.server_abr_streaming_url, Some(session)).await?;
            check_streaming_url(&server_url)?;
            Ok(ReloadedPlayer {
                server_url,
                ustreamer_config: player.ustreamer_config,
                formats: player.formats,
            })
        })
    }
}

/// googlevideo reloads on every RELOAD_PLAYER_RESPONSE; this caps it per song
/// so a server that keeps asking cannot loop /player calls forever.
const MAX_RELOADS_PER_SONG: u32 = 2;

/// Answer a RELOAD_PLAYER_RESPONSE: reload up to the cap; past it the
/// stream ends with the reload error.
async fn handle_reload(
    state: &mut SabrState,
    reloader: &mut impl PlayerReloader,
    reload: ReloadRequested,
) -> Result<(), SabrError> {
    if state.stats.reloads >= MAX_RELOADS_PER_SONG {
        tracing::warn!(
            "[sabr] server still asks for a player reload after {} reloads, giving up",
            MAX_RELOADS_PER_SONG,
        );
        return Err(reload.into());
    }
    state.stats.reloads += 1;
    tracing::info!(
        "[sabr] reloading player response ({}/{})",
        state.stats.reloads, MAX_RELOADS_PER_SONG,
    );
    let player = reloader.fetch(reload.token.as_deref()).await?;
    apply_reload(state, &player)
}

/// Repeat SABR request cycles until the stream is done, the consumer is
/// gone, a non-retryable error arrives, or retries run out.
async fn drive_loop(
    state: &mut SabrState,
    tx: &mpsc::Sender<Bytes>,
    cycle: &mut impl SabrCycle,
    reloader: &mut impl PlayerReloader,
) -> Result<(), SabrError> {
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

        match cycle.run(state).await {
            Ok(should_continue) => {
                consecutive_errors = 0;
                if !should_continue || state.done {
                    break;
                }
            }
            Err(e) if e.is::<ReloadRequested>() => {
                let reload = *e.downcast::<ReloadRequested>().expect("checked by is()");
                handle_reload(state, reloader, reload).await?;
                continue;
            }
            Err(e) if !is_retryable(&e) => return Err(e),
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

/// For tests elsewhere (main.rs) that check the real, global pin store.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Tests that touch the global pin store (or clear it) hold this, so they
    /// do not race each other.
    pub(crate) fn global_pins_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pin a placeholder format for `video_id` in the global store.
    pub(crate) fn pin_global(video_id: &str, ctt: &str, url: &str, content_length: u64) {
        let session = Arc::new(crate::music_session::test_fixtures::session());
        let mut source = super::PremiumUrlSource::new(reqwest::Client::new(), video_id, ctt, session, None);
        source.ctt = ctt.to_owned();
        super::pins::global().pin(super::pins::PinnedFormat {
            video_id: video_id.to_owned(),
            itag: 141,
            url: url.to_owned(),
            content_length,
            last_modified: 1,
            mime_type: "audio/mp4".to_owned(),
            title: "Song".to_owned(),
            duration_secs: 200,
            source,
        });
    }

    pub(crate) fn global_has_pin(video_id: &str) -> bool {
        super::pins::global().video_ids().iter().any(|id| id == video_id)
    }
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

    #[test]
    fn host_for_log_drops_path_and_query() {
        assert_eq!(
            host_for_log("https://rr1---sn-abc.googlevideo.com/videoplayback?expire=1&sig=SECRET"),
            "rr1---sn-abc.googlevideo.com"
        );
        assert_eq!(host_for_log("not a url"), "<unparseable>");
    }

    // -- helpers --------------------------------------------------------------

    /// Make `format` the audio format and forget all audio progress. Tests use
    /// it to set up states for other formats.
    fn switch_audio(state: &mut SabrState, format: &AdaptiveFormat) {
        state.audio_itag = format.itag;
        state.audio_format_id = format_id(format);
        state.audio_max_segment = -1;
        state.audio_downloaded_ms = 0;
        state.audio_init_sent = false;
        state.audio_initialized = false;
        state.audio_total_end_ms = 0;
        state.audio_end_segment = 0;
        state.header_map.clear();
    }

    fn test_state(server_url: &str) -> SabrState {
        let fmt = AdaptiveFormat {
            itag: 251,
            last_modified: 1,
            mime_type: "audio/webm".to_owned(),
            bitrate: 1,
            ..Default::default()
        };
        SabrState::new(server_url.to_owned(), &fmt, &fmt, Vec::new(), &WEB_REMIX_CLIENT)
    }

    const GOOD_URL: &str = "https://rr1---sn-abc.googlevideo.com/videoplayback?a=1";
    const SECRET_POTOKEN: &[u8] = b"secret-potoken-bytes";

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
        let logs = crate::storage::test_util::LogCapture::start();
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
        let logs = crate::storage::test_util::LogCapture::start();
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
        let logs = crate::storage::test_util::LogCapture::start();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reached = spawn_recorder(listener);

        let mut state = test_state(&format!("http://{addr}/videoplayback"));
        state.po_token = Some(SECRET_POTOKEN.to_vec());
        let (tx, _rx) = mpsc::channel(1);

        let result = within_2s("sabr_request_cycle", sabr_request_cycle(credential_client(), &mut state, &tx)).await;

        let reached = reached.lock().unwrap().clone();
        assert!(reached.is_empty(), "request reached the untrusted host: {reached:?}");
        assert!(result.is_err());
        let logs = logs.text();
        assert!(logs.contains("refusing untrusted streaming host 127.0.0.1"), "{logs}");
        assert!(!logs.contains("secret-potoken-bytes"), "{logs}");
    }

    #[tokio::test]
    async fn request_cycle_errors_do_not_carry_signed_url() {
        let logs = crate::storage::test_util::LogCapture::start();
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
        let logs = crate::storage::test_util::LogCapture::start();
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
            .body("secret-body")
            .send();
        let resp = within_2s("credential_client POST", send).await.unwrap();
        within_2s("origin server", server).await.unwrap();

        let reached = reached.lock().unwrap().clone();
        assert!(reached.is_empty(), "redirect was followed to the foreign host: {reached:?}");
        assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        let logs = logs.text();
        assert!(logs.contains("not following redirect to 127.0.0.1"), "{logs}");
        assert!(!logs.contains("secret-body"), "{logs}");
    }

    // -- secret-free logs ------------------------------------------------------

    #[tokio::test]
    async fn pot_token_log_shows_length_only() {
        let logs = crate::storage::test_util::LogCapture::start();
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

    // -- stored-credential-free /player requests --------------------------------

    fn body_json(req: &reqwest::Request) -> serde_json::Value {
        serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[test]
    fn web_remix_player_body_carries_ctt_without_stored_credentials() {
        let session = crate::music_session::test_fixtures::session();
        let auth = crate::music_session::web_remix_auth(Some("cast-token"), Some(&session)).unwrap();

        let req = player_request(
            credential_client(), "dQw4w9WgXcQ", PlayerTarget::WebRemix(auth), None, None, None,
        )
        .unwrap();

        assert!(is_trusted_google_host(req.url().as_str()));
        assert_eq!(req.url().host_str(), Some("music.youtube.com"));
        let body = body_json(&req);
        assert_eq!(
            body["context"]["user"]["credentialTransferTokens"],
            serde_json::json!([{ "scope": "VIDEO", "token": "cast-token" }])
        );
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        assert_eq!(body["context"]["client"]["clientVersion"], "1.20261102.01.00");
        assert_eq!(body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"], 20391);
        assert_eq!(req.headers()["x-youtube-client-version"], "1.20261102.01.00");
        assert_eq!(req.headers()["x-goog-visitor-id"], "CgtWSVNJVE9SLUlE");
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
        assert!(body["playbackContext"].get("reloadPlaybackContext").is_none());
    }

    #[test]
    fn reload_player_request_carries_the_reload_token_with_ctt_and_session() {
        let session = crate::music_session::test_fixtures::session();
        let auth = crate::music_session::web_remix_auth(Some("cast-token"), Some(&session)).unwrap();

        let req = player_request(
            credential_client(), "dQw4w9WgXcQ", PlayerTarget::WebRemix(auth), None, None, Some("reload-tok"),
        )
        .unwrap();

        let body = body_json(&req);
        assert_eq!(
            body["playbackContext"]["reloadPlaybackContext"],
            serde_json::json!({ "reloadPlaybackParams": { "token": "reload-tok" } })
        );
        assert_eq!(body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"], 20391);
        assert_eq!(body["context"]["user"]["credentialTransferTokens"][0]["token"], "cast-token");
        assert_eq!(req.url().host_str(), Some("music.youtube.com"));
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
    }

    #[test]
    fn fallback_clients_send_no_stored_credentials() {
        for client in [&IOS_CLIENT, &ANDROID_CLIENT, &WEB_CLIENT] {
            let req = player_request(
                credential_client(), "dQw4w9WgXcQ", PlayerTarget::Anonymous(client), Some("visitor"), None, None,
            )
            .unwrap();
            assert_eq!(req.url().host_str(), Some("www.youtube.com"));
            assert!(body_json(&req)["context"].get("user").is_none(), "{}", client.client_name);
            assert!(req.headers().get(reqwest::header::COOKIE).is_none());
            assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
        }
    }

    // -- format selection and server control parts ------------------------------

    fn audio(itag: i32, mime: &str, bitrate: u64) -> AdaptiveFormat {
        AdaptiveFormat {
            itag,
            last_modified: 1,
            mime_type: mime.to_owned(),
            bitrate,
            ..Default::default()
        }
    }

    const OPUS: &str = "audio/webm; codecs=\"opus\"";
    const AAC: &str = "audio/mp4; codecs=\"mp4a.40.2\"";

    #[test]
    fn audio_selection_skips_premium_only_formats() {
        // A ctt /player response lists the cast account's Premium formats
        // (774, 141), but the SABR session does not serve them.
        let formats = [
            audio(774, OPUS, 272_644),
            audio(141, AAC, 262_000),
            audio(140, AAC, 134_111),
            audio(251, OPUS, 146_272),
            audio(250, OPUS, 73_493),
        ];
        assert_eq!(choose_audio_format(&formats).map(|f| f.itag).unwrap(), 251);
    }

    // -- audio quality and the premium chain --------------------------------------

    #[test]
    fn audio_quality_defaults_to_max() {
        assert_eq!(AudioQuality::parse(None), AudioQuality::Max);
        assert_eq!(AudioQuality::parse(Some("max")), AudioQuality::Max);
        assert_eq!(AudioQuality::parse(Some(" standard ")), AudioQuality::Standard);
        let logs = crate::storage::test_util::LogCapture::start();
        assert_eq!(AudioQuality::parse(Some("loud")), AudioQuality::Max);
        assert!(logs.text().contains("YTCAST_AUDIO_QUALITY"), "{}", logs.text());
    }

    fn premium_listing() -> Vec<AdaptiveFormat> {
        vec![
            audio(140, AAC, 134_111),
            audio(141, AAC, 262_000),
            audio(251, OPUS, 146_272),
            audio(774, OPUS, 272_644),
        ]
    }

    fn itags(formats: &[&AdaptiveFormat]) -> Vec<i32> {
        formats.iter().map(|f| f.itag).collect()
    }

    #[test]
    fn premium_candidates_are_774_then_141_with_ctt_and_max_quality() {
        let formats = premium_listing();
        assert_eq!(itags(&premium_candidates(&formats, AudioQuality::Max, true)), [774, 141]);
        assert!(premium_candidates(&formats, AudioQuality::Standard, true).is_empty());
        assert!(premium_candidates(&formats, AudioQuality::Max, false).is_empty());
        let regular = [audio(140, AAC, 134_111), audio(251, OPUS, 146_272)];
        assert!(premium_candidates(&regular, AudioQuality::Max, true).is_empty());
        let only_141 = [audio(141, AAC, 262_000), audio(251, OPUS, 146_272)];
        assert_eq!(itags(&premium_candidates(&only_141, AudioQuality::Max, true)), [141]);
    }

    #[test]
    fn premium_candidates_prefer_the_non_drc_variant() {
        let formats = [
            variant(774, Some("CgsKA2RyYxIEdHJ1ZQ"), true, None),
            variant(774, None, false, None),
        ];
        let chosen = premium_candidates(&formats, AudioQuality::Max, true);
        assert_eq!((chosen[0].is_drc, chosen[0].xtags.as_deref()), (false, None));
    }

    #[test]
    fn standard_selection_is_unchanged_by_premium_listings() {
        assert_eq!(choose_audio_format(&premium_listing()).unwrap().itag, 251);
    }

    fn listed(itag: i32, mime: &str, bitrate: u64, len: Option<u64>) -> AdaptiveFormat {
        AdaptiveFormat { content_length: len, ..audio(itag, mime, bitrate) }
    }

    /// Runs `pick_source` over 774, 141: itags in `ok` deliver; the others
    /// fail with no media, except 774 with a decipher failure when
    /// `decipher_fails`.
    async fn chain(ok: &[i32], decipher_fails: bool) -> (Option<i32>, Vec<i32>) {
        let formats = premium_listing();
        let candidates = premium_candidates(&formats, AudioQuality::Max, true);
        let mut tried = Vec::new();
        let source = pick_source(&candidates, |f| {
            tried.push(f.itag);
            let ok = ok.contains(&f.itag);
            let decipher = decipher_fails && f.itag == 774;
            async move {
                if ok {
                    Ok(())
                } else if decipher {
                    let err = super::super::url_transport::tests_support::decipher_error("jsc sig error: no solutions");
                    Err::<(), SabrError>(err)
                } else {
                    Err("no media via url (Get: HTTP 403, UmpPost: HTTP 403)".into())
                }
            }
        })
        .await;
        let chosen = match source {
            AudioSource::Premium(f, ()) => Some(f.itag),
            AudioSource::Sabr => None,
        };
        (chosen, tried)
    }

    #[tokio::test]
    async fn chain_uses_774_when_it_works() {
        assert_eq!(chain(&[774, 141], false).await, (Some(774), vec![774]));
    }

    #[tokio::test]
    async fn chain_falls_back_from_774_to_141() {
        let logs = crate::storage::test_util::LogCapture::start();
        assert_eq!(chain(&[141], false).await, (Some(141), vec![774, 141]));
        assert!(logs.text().contains("audio: itag 774 (premium) via url failed: no media via url"), "{}", logs.text());
        assert!(logs.text().contains("trying itag 141 via url"), "{}", logs.text());
    }

    #[tokio::test]
    async fn chain_ends_with_sabr_when_both_fail() {
        let logs = crate::storage::test_util::LogCapture::start();
        assert_eq!(chain(&[], false).await, (None, vec![774, 141]));
        assert!(logs.text().contains("itag 141 (premium) via url failed: no media via url"), "{}", logs.text());
        assert!(logs.text().contains("trying the regular format via sabr"), "{}", logs.text());
    }

    #[tokio::test]
    async fn a_decipher_failure_goes_straight_to_sabr() {
        assert_eq!(chain(&[141], true).await, (None, vec![774]));
    }

    #[test]
    fn premium_plan_follows_the_quality_setting() {
        let formats = [
            listed(774, OPUS, 272_644, Some(9_000_000)),
            listed(141, AAC, 262_000, Some(7_000_000)),
            listed(251, OPUS, 146_272, Some(4_000_000)),
        ];
        let env = |value: &'static str| move |name: &str| (name == "YTCAST_AUDIO_QUALITY").then(|| value.to_owned());
        assert_eq!(itags(&premium_plan(&formats, true, |_| None)), [774, 141]);
        assert_eq!(itags(&premium_plan(&formats, true, env("max"))), [774, 141]);
        assert!(premium_plan(&formats, true, env("standard")).is_empty());
        assert!(premium_plan(&formats, false, |_| None).is_empty());
    }

    #[test]
    fn premium_plan_skips_formats_without_a_known_length() {
        let formats = [listed(774, OPUS, 272_644, None), listed(141, AAC, 262_000, Some(7_000_000))];
        assert_eq!(itags(&premium_plan(&formats, true, |_| None)), [141]);
    }

    #[test]
    fn sabr_client_info_version_comes_from_the_session_for_web_remix() {
        let session = crate::music_session::test_fixtures::session();
        assert_eq!(sabr_client_version(&WEB_REMIX_CLIENT, Some(&session)).as_deref(), Some("1.20261102.01.00"));
        assert_eq!(sabr_client_version(&IOS_CLIENT, Some(&session)), None);
        assert_eq!(sabr_client_version(&WEB_REMIX_CLIENT, None), None);
    }

    // -- premium url source, refresh and pins -------------------------------------

    /// A fixed answer for `n` and the signature.
    struct FixedDecipher;

    impl super::super::url_transport::Decipher for FixedDecipher {
        fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
            Ok((n.map(|_| "solved-n".to_owned()), sig.map(|_| "solved-sig".to_owned())))
        }
    }

    fn url_source(video_id: &str) -> PremiumUrlSource {
        let session = Arc::new(crate::music_session::test_fixtures::session());
        let mut source = PremiumUrlSource::new(credential_client().clone(), video_id, "CTT-SECRET", session, Some("POT-SECRET".to_owned()));
        source.decipher = Arc::new(FixedDecipher);
        source
    }

    #[tokio::test]
    async fn resolve_uses_the_session_client_version_and_logs_no_values() {
        let logs = crate::storage::test_util::LogCapture::start();
        let format = AdaptiveFormat {
            url: Some("https://rr1---sn-abc.googlevideo.com/videoplayback?itag=774&n=N-SECRET".to_owned()),
            ..listed(774, OPUS, 272_644, Some(1000))
        };

        let url = url_source("vid00000001").resolve(&format).await.unwrap();

        let pairs: Vec<(String, String)> = reqwest::Url::parse(&url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("cver").as_deref(), Some("1.20261102.01.00"));
        assert_eq!(get("n").as_deref(), Some("solved-n"));
        assert_eq!(get("pot").as_deref(), Some("POT-SECRET"));
        let logs = logs.text();
        assert!(logs.contains("deciphered in"), "{logs}");
        for secret in ["N-SECRET", "solved-n", "POT-SECRET", "CTT-SECRET", &get("cpn").unwrap()] {
            assert!(!logs.contains(secret), "{secret} leaked: {logs}");
        }
    }

    #[test]
    fn refresh_accepts_only_the_same_file() {
        let refresher = PremiumUrlRefresher { source: url_source("vid00000002"), itag: 774, content_length: 1000, last_modified: 1, store: Arc::default() };
        let same = listed(774, OPUS, 272_644, Some(1000));
        let longer = listed(774, OPUS, 272_644, Some(2000));
        let mut newer = listed(774, OPUS, 272_644, Some(1000));
        newer.last_modified = 9;

        assert!(refresher.same_file(std::slice::from_ref(&same)).is_ok());
        assert!(refresher.same_file(&[longer.clone()]).is_err());
        assert!(refresher.same_file(&[newer.clone()]).is_err());
        assert!(refresher.same_file(&[listed(141, AAC, 1, Some(1000))]).is_err());
        assert_eq!(refresher.same_file(&[longer, newer, same]).unwrap().content_length, Some(1000));
    }

    /// Serves `content` by `range=a-b`; or answers every request with `status`.
    fn spawn_ranges(listener: tokio::net::TcpListener, content: Vec<u8>, status: u16) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let req = read_request(&mut sock).await;
                let line = req.lines().next().unwrap_or("").to_owned();
                record.lock().unwrap().push(line.clone());
                let range = line.split("range=").nth(1).unwrap_or("0-0").split(['&', ' ']).next().unwrap_or("0-0").to_owned();
                let (a, b) = range.split_once('-').unwrap();
                let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                let body = if status == 200 { content[a..=b.min(content.len() - 1)].to_vec() } else { Vec::new() };
                let head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        seen
    }

    const CAST: &str = "CTT-OF-THIS-CAST";

    fn pinned_format(video_id: &str, ctt: &str, url: String, content_length: u64) -> pins::PinnedFormat {
        let mut source = url_source(video_id);
        source.http = reqwest::Client::new();
        source.ctt = ctt.to_owned();
        pins::PinnedFormat {
            video_id: video_id.to_owned(),
            itag: 141,
            url,
            content_length,
            last_modified: 1,
            mime_type: AAC.to_owned(),
            title: "Song".to_owned(),
            duration_secs: 200,
            source,
        }
    }

    /// A fresh pin store with one pin for `video_id` (cast `CAST`) whose URL
    /// points at a local server.
    async fn pin_local(video_id: &str, status: u16, content: Vec<u8>) -> (Arc<pins::PinStore>, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let len = content.len() as u64;
        let seen = spawn_ranges(listener, content, status);
        let store = Arc::new(pins::PinStore::default());
        let url = format!("http://{addr}/videoplayback?itag=141&sig=PINNED-URL-SECRET");
        store.pin(pinned_format(video_id, CAST, url, len));
        (store, seen)
    }

    async fn drain(mut rx: mpsc::Receiver<Bytes>) -> Vec<u8> {
        let mut got = Vec::new();
        while let Some(chunk) = rx.recv().await {
            got.extend_from_slice(&chunk);
        }
        got
    }

    fn song_content() -> Vec<u8> {
        (0..3000u32).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn a_seek_continues_the_pinned_format_from_the_offset() {
        let logs = crate::storage::test_util::LogCapture::start();
        let (store, seen) = pin_local("seekvideo01", 200, song_content()).await;

        let (info, rx) = stream_audio_in(&store, "seekvideo01", Some(CAST), None, 1500).await.unwrap();

        assert_eq!((info.start_offset, info.content_length), (1500, Some(3000)));
        assert_eq!(info.mime_type, AAC);
        assert_eq!(drain(rx).await, song_content()[1500..]);
        assert!(seen.lock().unwrap().iter().all(|l| l.contains("itag=141")));
        let logs = logs.text();
        assert!(logs.contains("audio: itag 141 (premium) via url from byte 1500"), "{logs}");
        assert!(!logs.contains("PINNED-URL-SECRET") && !logs.contains(CAST), "{logs}");
    }

    #[tokio::test]
    async fn byte_zero_again_is_served_from_the_pin_without_a_player_call() {
        // No session and no reachable /player: only the pin can serve this.
        let (store, seen) = pin_local("seekvideo05", 200, song_content()).await;

        let (info, rx) = within_2s("byte-0 reopen", stream_audio_in(&store, "seekvideo05", Some(CAST), None, 0))
            .await
            .unwrap();

        assert_eq!((info.start_offset, info.mime_type.as_str()), (0, AAC));
        assert_eq!(drain(rx).await, song_content());
        assert!(seen.lock().unwrap()[0].contains("range=0-"));
        assert_eq!(store.video_ids(), ["seekvideo05"]);
    }

    #[tokio::test]
    async fn a_pin_of_another_cast_is_not_used_and_is_dropped() {
        let (store, seen) = pin_local("seekvideo06", 200, song_content()).await;

        let err = stream_audio_in(&store, "seekvideo06", Some("CTT-OF-ANOTHER-CAST"), None, 1500).await.unwrap_err();

        assert!(err.to_string().contains("no pinned format"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
        assert!(store.video_ids().is_empty());
    }

    #[tokio::test]
    async fn a_failing_pinned_seek_is_an_error_not_another_format() {
        let (store, seen) = pin_local("seekvideo02", 403, vec![0; 3000]).await;

        let err = stream_audio_in(&store, "seekvideo02", Some(CAST), None, 1500).await.unwrap_err();

        assert!(err.to_string().contains("HTTP 403"), "{err}");
        let lines = seen.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "{lines:?}"); // GET then the UMP POST form, same URL
        assert!(lines.iter().all(|l| l.contains("itag=141")), "{lines:?}");
        assert!(!err.to_string().contains("PINNED-URL-SECRET"));
    }

    #[tokio::test]
    async fn a_seek_past_the_end_is_not_satisfiable() {
        let (store, seen) = pin_local("seekvideo03", 200, vec![0; 3000]).await;

        let err = stream_audio_in(&store, "seekvideo03", Some(CAST), None, 3000).await.unwrap_err();

        assert_eq!(err.downcast_ref::<RangeNotSatisfiable>().map(|e| e.content_length), Some(3000));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_seek_without_a_pin_fails_without_a_player_call() {
        let store = Arc::new(pins::PinStore::default());
        let err = stream_audio_in(&store, "nopinvideo1", Some(CAST), None, 1500).await.unwrap_err();
        assert!(err.to_string().contains("no pinned format"), "{err}");
    }

    #[test]
    fn using_a_pin_refreshes_its_expiry() {
        let store = pins::PinStore::default();
        store.pin(pinned_format("ttlvideo001", CAST, String::new(), 10));
        store.age("ttlvideo001", std::time::Duration::from_secs(4 * 60));
        assert!(store.pinned("ttlvideo001", CAST).is_some());
        store.age("ttlvideo001", std::time::Duration::from_secs(4 * 60));
        assert!(store.pinned("ttlvideo001", CAST).is_some(), "use restarted the 5 minutes");
        store.age("ttlvideo001", std::time::Duration::from_secs(5 * 60 + 1));
        assert!(store.pinned("ttlvideo001", CAST).is_none());
    }

    #[test]
    fn the_download_keeps_its_pin_alive_at_every_range() {
        use super::super::url_transport::UrlRefresher;
        let store = Arc::new(pins::PinStore::default());
        let pin = pinned_format("ttlvideo002", CAST, String::new(), 10);
        store.pin(pin.clone());
        let refresher = PremiumUrlRefresher::for_pin(&pin, store.clone());

        store.age("ttlvideo002", std::time::Duration::from_secs(4 * 60));
        refresher.keep_alive();
        store.age("ttlvideo002", std::time::Duration::from_secs(4 * 60));

        assert_eq!(store.video_ids(), ["ttlvideo002"]);
    }

    #[test]
    fn at_most_four_pins_and_the_least_recently_used_goes() {
        let store = pins::PinStore::default();
        for id in ["lruvideo001", "lruvideo002", "lruvideo003", "lruvideo004"] {
            store.pin(pinned_format(id, CAST, String::new(), 10));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // The oldest pin was used last, so the second oldest goes.
        assert!(store.pinned("lruvideo001", CAST).is_some());
        store.pin(pinned_format("lruvideo005", CAST, String::new(), 10));

        let mut ids = store.video_ids();
        ids.sort();
        assert_eq!(ids, ["lruvideo001", "lruvideo003", "lruvideo004", "lruvideo005"]);
    }

    #[test]
    fn clear_forgets_every_pin() {
        let store = pins::PinStore::default();
        store.pin(pinned_format("clrvideo001", CAST, String::new(), 10));
        store.clear();
        assert!(store.video_ids().is_empty());
    }

    #[test]
    fn a_refreshed_url_goes_into_the_matching_pin_only() {
        let store = Arc::new(pins::PinStore::default());
        let pin = pinned_format("urlvideo001", CAST, "old-url".to_owned(), 10);
        store.pin(pin.clone());
        let refresher = PremiumUrlRefresher::for_pin(&pin, store.clone());

        refresher.record_url("new-url");
        assert_eq!(store.url_of("urlvideo001").as_deref(), Some("new-url"));

        store.update_url("urlvideo001", CAST, 141, 99, "other-file-url");
        store.update_url("urlvideo001", "CTT-OF-ANOTHER-CAST", 141, 1, "other-cast-url");
        store.update_url("urlvideo001", CAST, 774, 1, "other-itag-url");
        assert_eq!(store.url_of("urlvideo001").as_deref(), Some("new-url"));
    }

    #[test]
    fn a_pinned_refresher_checks_the_pinned_file() {
        let store = Arc::new(pins::PinStore::default());
        let mut pin = pinned_format("lenvideo001", CAST, String::new(), 123_456);
        pin.last_modified = 77;
        let refresher = PremiumUrlRefresher::for_pin(&pin, store);
        assert_eq!((refresher.itag, refresher.content_length, refresher.last_modified), (141, 123_456, 77));
    }

    fn song() -> SongMeta {
        SongMeta { title: "Song".to_owned(), duration_secs: 200 }
    }

    fn premium_listing_with_lengths() -> Vec<AdaptiveFormat> {
        vec![
            listed(774, OPUS, 272_644, Some(9_000_000)),
            listed(141, AAC, 262_000, Some(7_000_000)),
            listed(251, OPUS, 146_272, Some(4_000_000)),
        ]
    }

    #[tokio::test]
    async fn premium_start_pins_the_format_that_delivers() {
        let store = pins::PinStore::default();
        let source = url_source("startvideo1");
        let formats = premium_listing_with_lengths();

        let started = start_premium(&store, store.generation(), &source, &formats, true, |_| None, &song(), |f| {
            let itag = f.itag;
            async move {
                if itag == 774 {
                    return Err::<(mpsc::Receiver<Bytes>, String), SabrError>("no media via url".into());
                }
                let (_tx, rx) = mpsc::channel(1);
                Ok((rx, "resolved-141-url".to_owned()))
            }
        })
        .await;

        let (itag, info, _rx) = started.expect("141 delivers");
        assert_eq!((itag, info.content_length, info.mime_type.as_str()), (141, Some(7_000_000), AAC));
        let pin = store.pinned("startvideo1", "CTT-SECRET").unwrap();
        assert_eq!((pin.itag, pin.url.as_str(), pin.content_length), (141, "resolved-141-url", 7_000_000));
    }

    #[tokio::test]
    async fn premium_start_falling_back_to_sabr_leaves_no_pin() {
        let store = pins::PinStore::default();
        let source = url_source("startvideo2");
        let formats = premium_listing_with_lengths();
        let mut tried = Vec::new();

        let started = start_premium(&store, store.generation(), &source, &formats, true, |_| None, &song(), |f| {
            tried.push(f.itag);
            async { Err::<(mpsc::Receiver<Bytes>, String), SabrError>("no media via url".into()) }
        })
        .await;

        assert!(started.is_none());
        assert_eq!(tried, [774, 141]);
        assert!(store.video_ids().is_empty());
    }

    #[tokio::test]
    async fn a_cast_change_during_the_start_prevents_the_pin() {
        let store = pins::PinStore::default();
        let source = url_source("startvideo4");
        let formats = premium_listing_with_lengths();
        let generation = store.generation();
        store.clear(); // the cast ended while /player and the first range ran

        let started = start_premium(&store, generation, &source, &formats, true, |_| None, &song(), |_| async {
            let (_tx, rx) = mpsc::channel(1);
            Ok::<_, SabrError>((rx, "late-url".to_owned()))
        })
        .await;

        assert!(started.is_some(), "the song still plays");
        assert!(store.video_ids().is_empty(), "but nothing is pinned for the old cast");
    }

    #[test]
    fn pin_if_current_refuses_after_a_clear() {
        let store = pins::PinStore::default();
        let generation = store.generation();
        assert!(store.pin_if_current(generation, pinned_format("genvideo001", CAST, String::new(), 10)));
        store.clear();
        assert!(!store.pin_if_current(generation, pinned_format("genvideo002", CAST, String::new(), 10)));
        assert!(store.video_ids().is_empty());
    }

    #[test]
    fn a_new_pin_replaces_the_older_one_for_the_same_song() {
        let store = pins::PinStore::default();
        store.pin(pinned_format("repvideo001", CAST, "first".to_owned(), 10));
        store.pin(pinned_format("repvideo001", CAST, "second".to_owned(), 10));
        assert_eq!(store.video_ids(), ["repvideo001"]);
        assert_eq!(store.url_of("repvideo001").as_deref(), Some("second"));
    }

    #[test]
    fn clear_pins_empties_the_global_store() {
        let _lock = test_support::global_pins_lock();
        test_support::pin_global("globalpin01", CAST, "u", 10);
        assert!(test_support::global_has_pin("globalpin01"));
        clear_pins();
        assert!(!test_support::global_has_pin("globalpin01"));
    }

    #[tokio::test]
    async fn public_stream_audio_serves_seeks_from_the_global_pins() {
        let _lock = test_support::global_pins_lock();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = spawn_ranges(listener, song_content(), 200);
        test_support::pin_global("globalpin02", CAST, &format!("http://{addr}/videoplayback?itag=141"), 3000);

        let (info, rx) = stream_audio("globalpin02", Some(CAST), None, 1500).await.unwrap();

        assert_eq!(info.start_offset, 1500);
        assert_eq!(drain(rx).await, song_content()[1500..]);
        assert_eq!(seen.lock().unwrap().len(), 1);
        clear_pins();
    }

    #[tokio::test]
    async fn a_long_pinned_download_keeps_its_own_pin_alive() {
        let total = 2 * URL_CHUNK_SIZE as usize + 1000;
        let content: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let (store, seen) = pin_local("longvideo01", 200, content.clone()).await;

        let (_info, rx) = stream_audio_in(&store, "longvideo01", Some(CAST), None, 1500).await.unwrap();
        // Four minutes pass before the next ranges are fetched...
        store.age("longvideo01", std::time::Duration::from_secs(4 * 60));
        assert_eq!(drain(rx).await, content[1500..]);
        // ...and four more after: only the download's own touches keep it.
        store.age("longvideo01", std::time::Duration::from_secs(4 * 60));

        // Two ranges from byte 1500: one boundary, one keep-alive.
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(store.pinned("longvideo01", CAST).is_some());
    }

    #[tokio::test]
    async fn standard_quality_never_tries_a_premium_format() {
        let store = pins::PinStore::default();
        let source = url_source("startvideo3");
        let formats = premium_listing_with_lengths();
        let standard = |name: &str| (name == "YTCAST_AUDIO_QUALITY").then(|| "standard".to_owned());
        let mut tried = 0;

        let started = start_premium(&store, store.generation(), &source, &formats, true, standard, &song(), |_| {
            tried += 1;
            async { Err::<(mpsc::Receiver<Bytes>, String), SabrError>("unexpected".into()) }
        })
        .await;

        assert!(started.is_none());
        assert_eq!(tried, 0);
    }

    #[test]
    fn sabr_state_takes_the_session_client_version() {
        let state = test_state(GOOD_URL).with_client_version(Some("1.20261104.00.00".to_owned()));
        assert_eq!(state.client_version, "1.20261104.00.00");
        let state = test_state(GOOD_URL).with_client_version(None);
        assert_eq!(state.client_version, WEB_REMIX_CLIENT.client_version);
    }

    /// Fails or answers per call; flags overlapping solver runs on shared
    /// flags; signals when a run has started.
    struct ProbingDecipher {
        fail_first: std::sync::atomic::AtomicBool,
        probe: &'static SolverProbe,
    }

    /// Overlap detection shared by fake solvers (Premium decipher and nsig).
    struct SolverProbe {
        busy: std::sync::atomic::AtomicBool,
        overlapped: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
    }

    impl SolverProbe {
        const fn new() -> Self {
            Self {
                busy: std::sync::atomic::AtomicBool::new(false),
                overlapped: std::sync::atomic::AtomicBool::new(false),
                started: tokio::sync::Notify::const_new(),
            }
        }

        fn run(&self) {
            use std::sync::atomic::Ordering::SeqCst;
            if self.busy.swap(true, SeqCst) {
                self.overlapped.store(true, SeqCst);
            }
            self.started.notify_one();
            std::thread::sleep(std::time::Duration::from_millis(60));
            self.busy.store(false, SeqCst);
        }

        fn overlapped(&self) -> bool {
            self.overlapped.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl super::super::url_transport::Decipher for ProbingDecipher {
        fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
            self.probe.run();
            if self.fail_first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err("jsc n error: no solutions".to_owned());
            }
            Ok((n.map(|_| "n".to_owned()), sig.map(|_| "s".to_owned())))
        }
    }

    fn probing_source(probe: &'static SolverProbe, fail_first: bool) -> PremiumUrlSource {
        let mut source = url_source("permvideo01");
        source.decipher = Arc::new(ProbingDecipher { fail_first: fail_first.into(), probe });
        source
    }

    fn trusted_format() -> AdaptiveFormat {
        AdaptiveFormat {
            url: Some("https://rr1---sn-abc.googlevideo.com/videoplayback?itag=774&n=abc".to_owned()),
            ..listed(774, OPUS, 272_644, Some(1000))
        }
    }

    #[tokio::test]
    async fn a_failed_solver_run_releases_its_permit() {
        static PROBE: SolverProbe = SolverProbe::new();
        let source = probing_source(&PROBE, true);

        assert!(source.resolve(&trusted_format()).await.is_err());
        let second = tokio::time::timeout(std::time::Duration::from_secs(2), source.resolve(&trusted_format())).await;

        assert!(second.expect("permit leaked").is_ok());
    }

    #[tokio::test]
    async fn solver_runs_never_overlap() {
        static PROBE: SolverProbe = SolverProbe::new();
        let source = probing_source(&PROBE, false);
        let format = trusted_format();

        let (a, b, c) = tokio::join!(source.resolve(&format), source.resolve(&format), source.resolve(&format));

        assert!(a.is_ok() && b.is_ok() && c.is_ok());
        assert!(!PROBE.overlapped(), "two solver runs overlapped");
    }

    #[tokio::test]
    async fn a_cancelled_request_keeps_the_permit_until_the_solver_finishes() {
        static PROBE: SolverProbe = SolverProbe::new();
        let source = probing_source(&PROBE, false);

        let first = {
            let source = source.clone();
            tokio::spawn(async move { source.resolve(&trusted_format()).await })
        };
        PROBE.started.notified().await; // its solver is running now
        first.abort();
        source.resolve(&trusted_format()).await.unwrap();

        assert!(!PROBE.overlapped(), "permit released on cancel while the solver ran");
    }

    fn probing_nsig(_js: &str, n: &str, _hash: &str) -> Result<String, SabrError> {
        NSIG_PROBE.run();
        Ok(n.chars().rev().collect())
    }

    static NSIG_PROBE: SolverProbe = SolverProbe::new();

    fn nsig_url() -> String {
        "https://rr1---sn-abc.googlevideo.com/videoplayback?a=1&n=abcd&b=2".to_owned()
    }

    #[tokio::test]
    async fn sabr_nsig_runs_under_the_same_solver_permit() {
        let _lock = nsig_probe_lock();
        let session = crate::music_session::test_fixtures::session();
        let source = probing_source(&NSIG_PROBE, false);
        let format = trusted_format();

        let (nsig, premium) = tokio::join!(
            solve_nsig_with(nsig_url(), Some(&session), probing_nsig),
            source.resolve(&format),
        );

        assert!(nsig.unwrap().contains("n=dcba"));
        assert!(premium.is_ok());
        assert!(!NSIG_PROBE.overlapped(), "SABR nsig and a Premium decipher overlapped");
    }

    #[tokio::test]
    async fn a_cancelled_sabr_nsig_keeps_the_permit() {
        let _lock = nsig_probe_lock();
        let source = probing_source(&NSIG_PROBE, false);

        let nsig = tokio::spawn(async {
            let session = crate::music_session::test_fixtures::session();
            solve_nsig_with(nsig_url(), Some(&session), probing_nsig).await
        });
        NSIG_PROBE.started.notified().await;
        nsig.abort();
        source.resolve(&trusted_format()).await.unwrap();

        assert!(!NSIG_PROBE.overlapped(), "permit released on a cancelled nsig solve");
    }

    /// The two nsig tests share `NSIG_PROBE` (a plain fn solver cannot carry
    /// its own); they must not run at the same time.
    fn nsig_probe_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- format variants and client info ---------------------------------------------

    fn variant(itag: i32, xtags: Option<&str>, is_drc: bool, track: Option<&str>) -> AdaptiveFormat {
        AdaptiveFormat {
            xtags: xtags.map(str::to_owned),
            is_drc,
            audio_track_id: track.map(str::to_owned),
            ..audio(itag, OPUS, 272_644)
        }
    }

    #[test]
    fn reload_keeps_the_same_variant_of_the_itag() {
        let mut state = test_state(GOOD_URL);
        switch_audio(&mut state, &variant(140, None, false, None));
        let mut drc = variant(140, Some("drc"), true, None);
        drc.last_modified = 7;
        let mut plain = variant(140, None, false, None);
        plain.last_modified = 8;
        let player = ReloadedPlayer { server_url: RELOADED_URL.to_owned(), ustreamer_config: vec![], formats: vec![drc, plain] };

        apply_reload(&mut state, &player).unwrap();

        assert_eq!(state.audio_format_id.last_modified, Some(8));
        assert_eq!(state.audio_format_id.xtags, None);
    }

    #[test]
    fn itag_variants_are_described_with_xtags_drc_and_track() {
        let formats = [
            variant(774, Some("CgsKA2RyYw"), true, Some("en.4")),
            variant(774, None, false, None),
            audio(251, OPUS, 146_272),
        ];
        assert_eq!(
            describe_variants(&formats, 774),
            "774 variants: [xtags=CgsKA2RyYw isDrc=true audioTrack=en.4 lm=1, xtags=- isDrc=false audioTrack=- lm=1]"
        );
    }

    #[test]
    fn web_remix_client_info_uses_the_session_version_and_omits_empty_device_fields() {
        let mut state = test_state(GOOD_URL);
        state.client_version = "1.20261104.00.00".to_owned();

        let info = state.build_request().streamer_context.unwrap().client_info.unwrap();

        assert_eq!(info.client_version.as_deref(), Some("1.20261104.00.00"));
        assert_eq!(info.client_name, Some(67));
        assert_eq!((info.device_make, info.device_model, info.os_name, info.os_version), (None, None, None, None));
    }

    #[test]
    fn audio_selection_with_only_premium_formats_says_so() {
        let formats = [audio(774, OPUS, 272_644), audio(141, AAC, 262_000)];
        let err = choose_audio_format(&formats).unwrap_err();
        assert!(err.to_string().contains("Premium"), "{err}");
    }

    #[test]
    fn audio_selection_without_audio_formats_is_a_plain_error() {
        let formats = [audio(160, "video/mp4", 100_000)];
        let err = choose_audio_format(&formats).unwrap_err();
        assert_eq!(err.to_string(), "no suitable audio format found");
    }

    fn part(part_type: u32, data: Vec<u8>) -> ump::UmpPart {
        ump::UmpPart { part_type, data }
    }

    fn reload_part(token: Option<&str>) -> ump::UmpPart {
        let ctx = vs::ReloadPlaybackContext {
            reload_playback_params: Some(vs::ReloadPlaybackParams { token: token.map(str::to_owned) }),
        };
        part(ump::RELOAD_PLAYER_RESPONSE, ctx.encode_to_vec())
    }

    #[test]
    fn reload_part_is_decoded_and_ends_the_response_without_retry() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);

        // The rest of the response (47, 35) is still applied after a 46.
        assert!(handle_part(&mut state, &reload_part(Some("secret-reload-token"))).unwrap().is_none());
        let err = finish_response(&mut state, &[46, 47, 35]).unwrap_err();

        assert!(!is_retryable(&err));
        let reload = err.downcast_ref::<ReloadRequested>().expect("a reload request");
        assert_eq!(reload.token.as_deref(), Some("secret-reload-token"));
        assert!(err.to_string().contains("reload"), "{err}");
        assert!(state.pending_reload.is_none());
        let logs = logs.text();
        assert!(logs.contains("RELOAD_PLAYER_RESPONSE (token len=19)"), "{logs}");
        assert!(logs.contains("parts: 46,47,35"), "{logs}");
        for text in [err.to_string(), format!("{err:?}"), logs] {
            assert!(!text.contains("secret-reload-token"), "{text}");
        }
    }

    #[test]
    fn response_without_reload_finishes_cleanly() {
        let mut state = test_state(GOOD_URL);
        finish_response(&mut state, &[20, 21, 22, 35]).unwrap();
    }

    #[test]
    fn playback_start_policy_is_decoded_for_the_log() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);
        let policy = vs::PlaybackStartPolicy {
            start_min_readahead_policy: Some(vs::playback_start_policy::MinReadaheadPolicy {
                min_readahead_ms: Some(1500),
                min_bandwidth_bytes_per_sec: None,
            }),
            start_timeout_ms: Some(5000),
            ..Default::default()
        };

        let out = handle_part(&mut state, &part(ump::PLAYBACK_START_POLICY, policy.encode_to_vec())).unwrap();

        assert!(out.is_none());
        let logs = logs.text();
        assert!(logs.contains("playback start policy: start_readahead_ms=1500, start_timeout_ms=5000"), "{logs}");
    }

    #[test]
    fn sabr_error_part_logs_type_and_code() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);
        let data = vs::SabrError { r#type: Some("sabr.config".to_owned()), code: Some(3) }.encode_to_vec();

        let err = handle_part(&mut state, &part(ump::SABR_ERROR, data)).unwrap_err();

        assert!(err.to_string().contains("type=sabr.config, code=3"), "{err}");
        assert!(logs.text().contains("SABR_ERROR type=sabr.config code=3"), "{}", logs.text());
    }

    #[test]
    fn media_header_itags_audio_bytes_and_protection_statuses_are_tallied() {
        let mut state = test_state(GOOD_URL);
        let header = |id: u32, itag: i32| {
            part(
                ump::MEDIA_HEADER,
                vs::MediaHeader { header_id: Some(id), itag: Some(itag), ..Default::default() }.encode_to_vec(),
            )
        };
        handle_part(&mut state, &header(1, 251)).unwrap();
        handle_part(&mut state, &header(2, 160)).unwrap();
        handle_part(&mut state, &header(3, 251)).unwrap();
        handle_part(&mut state, &part(ump::MEDIA, vec![1, 0xaa, 0xbb, 0xcc])).unwrap();
        handle_part(&mut state, &part(ump::MEDIA, vec![2, 0xdd])).unwrap();
        handle_part(&mut state, &part(ump::STREAM_PROTECTION_STATUS, protection(1))).unwrap();
        handle_part(&mut state, &part(ump::STREAM_PROTECTION_STATUS, protection(1))).unwrap();
        handle_part(&mut state, &part(ump::STREAM_PROTECTION_STATUS, protection(2))).unwrap();

        assert_eq!(state.stats.media_headers_by_itag, std::collections::BTreeMap::from([(160, 1), (251, 2)]));
        assert_eq!(state.stats.audio_bytes_by_itag, std::collections::BTreeMap::from([(251, 3)]));
        assert_eq!(state.stats.protection_statuses, vec![1, 2]);
    }

    #[test]
    fn transport_errors_stay_retryable() {
        let err: SabrError = "connection reset".into();
        assert!(is_retryable(&err));
    }

    #[test]
    fn first_sabr_audio_bytes_log_the_audio_line_once() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);
        let header = vs::MediaHeader { header_id: Some(1), itag: Some(251), ..Default::default() };
        handle_part(&mut state, &part(ump::MEDIA_HEADER, header.encode_to_vec())).unwrap();

        handle_part(&mut state, &part(ump::MEDIA, vec![1, 0xaa])).unwrap();
        handle_part(&mut state, &part(ump::MEDIA, vec![1, 0xbb])).unwrap();

        let logs = logs.text();
        assert_eq!(logs.matches("audio: itag 251 (standard) via sabr, first bytes after").count(), 1, "{logs}");
    }

    #[test]
    fn media_part_for_audio_is_returned_for_sending() {
        let mut state = test_state(GOOD_URL);
        state.header_map.insert(
            3,
            HeaderInfo {
                itag: 251,
                is_init_seg: false,
                sequence_number: 1,
                start_ms: 0,
                duration_ms: 10_000,
                is_audio: true,
            },
        );

        let out = handle_part(&mut state, &part(ump::MEDIA, vec![3, 0xaa, 0xbb])).unwrap();

        assert_eq!(out.as_deref(), Some(&[0xaa, 0xbb][..]));
    }

    fn protection(status: i32) -> Vec<u8> {
        vs::StreamProtectionStatus { status: Some(status), ..Default::default() }.encode_to_vec()
    }

    #[test]
    fn protection_status_log_says_whether_a_potoken_was_sent() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);

        for status in 0..=3 {
            state.po_token = None;
            handle_part(&mut state, &part(ump::STREAM_PROTECTION_STATUS, protection(status))).unwrap();
            state.po_token = Some(b"pot".to_vec());
            handle_part(&mut state, &part(ump::STREAM_PROTECTION_STATUS, protection(status))).unwrap();
        }

        let logs = logs.text();
        assert!(logs.contains("stream_protection_status=1 (OK, no PoToken sent)"), "{logs}");
        assert!(logs.contains("stream_protection_status=1 (OK, PoToken sent)"), "{logs}");
        for status in 0..=3 {
            let line = format!("stream_protection_status={status} (");
            assert_eq!(logs.matches(&line).count(), 2, "status {status}: {logs}");
        }
        assert_eq!(logs.matches("no PoToken sent").count(), 4, "{logs}");
        assert!(!logs.contains("Premium"), "{logs}");
    }

    // -- reload loop -------------------------------------------------------------

    const RELOADED_URL: &str = "https://rr9---sn-new.googlevideo.com/videoplayback?r=1";

    /// Answers every round trip with a RELOAD_PLAYER_RESPONSE while `reload_while`
    /// holds, and ends the stream cleanly otherwise.
    struct ReloadingCycle {
        requests: u32,
        reload_while: fn(&SabrState) -> bool,
    }

    impl SabrCycle for ReloadingCycle {
        fn run<'a>(&'a mut self, state: &'a mut SabrState) -> CycleFuture<'a> {
            self.requests += 1;
            Box::pin(async move {
                if !(self.reload_while)(state) {
                    return Ok(false);
                }
                handle_part(state, &reload_part(Some("tok")))?;
                finish_response(state, &[ump::RELOAD_PLAYER_RESPONSE])?;
                Ok(true)
            })
        }
    }

    /// Records the reload tokens it was asked with and serves a fixed player.
    struct FakeReloader {
        tokens: Vec<Option<String>>,
        formats: Vec<AdaptiveFormat>,
    }

    impl FakeReloader {
        fn new(formats: Vec<AdaptiveFormat>) -> Self {
            Self { tokens: Vec::new(), formats }
        }
    }

    impl PlayerReloader for FakeReloader {
        fn fetch<'a>(&'a mut self, token: Option<&'a str>) -> ReloadFuture<'a> {
            self.tokens.push(token.map(str::to_owned));
            let player = ReloadedPlayer {
                server_url: RELOADED_URL.to_owned(),
                ustreamer_config: vec![9, 9],
                formats: self.formats.clone(),
            };
            Box::pin(async move { Ok(player) })
        }
    }

    fn always(_: &SabrState) -> bool {
        true
    }

    /// A SABR state streaming 251 whose format id has last-modified 1.
    fn sabr_251_state() -> SabrState {
        let mut state = test_state(GOOD_URL);
        switch_audio(&mut state, &audio(251, OPUS, 146_272));
        state
    }

    fn cookie_for(itag: i32) -> Vec<u8> {
        vs::PlaybackCookie {
            audio_fmt: Some(misc::FormatId { itag: Some(itag), last_modified: Some(5), xtags: None }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn reload_formats() -> Vec<AdaptiveFormat> {
        vec![
            audio(774, OPUS, 272_644),
            audio(251, OPUS, 146_272),
            audio(140, AAC, 134_111),
            audio(160, "video/mp4", 100_000),
        ]
    }

    #[tokio::test]
    async fn reload_swaps_in_the_new_streaming_data_and_continues() {
        let mut state = test_state(GOOD_URL);
        state.sabr_contexts.insert(2, vec![1]);
        state.sabr_contexts_send.insert(2);
        state.playback_cookie = Some(cookie_for(251));
        state.po_token = Some(SECRET_POTOKEN.to_vec());
        let (tx, _rx) = mpsc::channel(1);
        let mut cycle = ReloadingCycle { requests: 0, reload_while: |s| s.server_url == GOOD_URL };
        let mut reloader = FakeReloader::new(reload_formats());

        drive_loop(&mut state, &tx, &mut cycle, &mut reloader).await.unwrap();

        assert_eq!(reloader.tokens, vec![Some("tok".to_owned())]);
        assert_eq!(cycle.requests, 2);
        assert_eq!(state.server_url, RELOADED_URL);
        assert_eq!(state.ustreamer_config, vec![9, 9]);
        assert!(state.sabr_contexts.is_empty());
        assert!(state.sabr_contexts_send.is_empty());
        // Reference behaviour: cookie and PoToken carry over.
        assert_eq!(state.playback_cookie, Some(cookie_for(251)));
        assert_eq!(state.po_token.as_deref(), Some(SECRET_POTOKEN));
        assert_eq!(state.stats.reloads, 1);
        assert_eq!(state.audio_itag, 251);
    }

    /// Records the request each round trip would send, then plays `inner`.
    struct RecordingCycle<C> {
        sent: Vec<vs::VideoPlaybackAbrRequest>,
        inner: C,
    }

    impl<C: SabrCycle + Send> SabrCycle for RecordingCycle<C> {
        fn run<'a>(&'a mut self, state: &'a mut SabrState) -> CycleFuture<'a> {
            self.sent.push(state.build_request());
            self.inner.run(state)
        }
    }

    #[tokio::test]
    async fn first_request_after_reload_selects_the_reloaded_format_id() {
        let mut state = sabr_251_state();
        state.playback_cookie = Some(cookie_for(251));
        let (tx, _rx) = mpsc::channel(1);
        let mut cycle = RecordingCycle {
            sent: Vec::new(),
            inner: ReloadingCycle { requests: 0, reload_while: |s| s.server_url == GOOD_URL },
        };
        let mut reloaded_251 = audio(251, OPUS, 146_272);
        reloaded_251.last_modified = 2;
        reloaded_251.xtags = Some("CgcKAnhoEgEx".to_owned());
        let mut reloader = FakeReloader::new(vec![reloaded_251, audio(140, AAC, 134_111)]);

        drive_loop(&mut state, &tx, &mut cycle, &mut reloader).await.unwrap();

        assert_eq!(cycle.sent.len(), 2);
        let after = &cycle.sent[1];
        let expected = misc::FormatId {
            itag: Some(251),
            last_modified: Some(2),
            xtags: Some("CgcKAnhoEgEx".to_owned()),
        };
        // Field 16: googlevideo's selected_audio_format_ids.
        assert_eq!(after.preferred_audio_format_ids, vec![expected]);
        // A cookie for the streaming format carries over.
        assert_eq!(after.streamer_context.as_ref().unwrap().playback_cookie, Some(cookie_for(251)));
        assert_eq!(cycle.sent[0].preferred_audio_format_ids[0].last_modified, Some(1));
    }

    #[test]
    fn reload_drops_only_a_cookie_for_another_audio_format() {
        let player = ReloadedPlayer { server_url: RELOADED_URL.to_owned(), ustreamer_config: vec![], formats: vec![] };
        let mut state = sabr_251_state();
        state.playback_cookie = Some(cookie_for(774));
        apply_reload(&mut state, &player).unwrap();
        assert_eq!(state.playback_cookie, None);

        let mut state = sabr_251_state();
        state.playback_cookie = Some(vec![7]); // undecodable: kept as is
        apply_reload(&mut state, &player).unwrap();
        assert_eq!(state.playback_cookie, Some(vec![7]));
    }

    /// Answers with a SABR_ERROR while `error_while(state, request count)`
    /// holds, else ends cleanly.
    struct SabrErrorCycle {
        requests: u32,
        error_while: fn(&SabrState, u32) -> bool,
    }

    impl SabrCycle for SabrErrorCycle {
        fn run<'a>(&'a mut self, state: &'a mut SabrState) -> CycleFuture<'a> {
            self.requests += 1;
            let requests = self.requests;
            Box::pin(async move {
                if !(self.error_while)(state, requests) {
                    return Ok(false);
                }
                let err = vs::SabrError { r#type: Some("sabr.no_audio_selected".to_owned()), code: Some(2) };
                handle_part(state, &part(ump::SABR_ERROR, err.encode_to_vec()))?;
                Ok(true)
            })
        }
    }

    #[tokio::test]
    async fn sabr_error_without_premium_is_retried_as_before() {
        let mut state = test_state(GOOD_URL);
        let (tx, _rx) = mpsc::channel(1);
        let mut cycle = SabrErrorCycle { requests: 0, error_while: |_, requests| requests == 1 };
        let mut reloader = FakeReloader::new(reload_formats());

        drive_loop(&mut state, &tx, &mut cycle, &mut reloader).await.unwrap();

        assert!(reloader.tokens.is_empty());
        assert_eq!(cycle.requests, 2);
        assert_eq!(state.audio_itag, 251);
    }

    #[test]
    fn request_format_summary_shows_format_ids_and_cookie_formats_only() {
        let mut state = sabr_251_state();
        switch_audio(&mut state, &audio(140, AAC, 134_111));
        state.po_token = Some(SECRET_POTOKEN.to_vec());
        let cookie = vs::PlaybackCookie {
            audio_fmt: Some(misc::FormatId { itag: Some(251), last_modified: Some(5), xtags: None }),
            ..Default::default()
        };
        state.playback_cookie = Some(cookie.encode_to_vec());

        let line = describe_request_formats(&state.build_request());

        assert!(line.contains("selected audio [140 lm=1 xtags=-]"), "{line}");
        assert!(line.contains("selected video [251 lm=1 xtags=-]"), "{line}");
        assert!(line.contains("init [251 lm=1 xtags=-]"), "{line}");
        assert!(line.contains("cookie audio=251 video=-"), "{line}");
        assert!(!line.contains("secret"), "{line}");
    }

    #[tokio::test]
    async fn reloads_are_capped_at_two_per_song() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut state = test_state(GOOD_URL);
        let (tx, _rx) = mpsc::channel(1);
        let mut cycle = ReloadingCycle { requests: 0, reload_while: always };
        let mut reloader = FakeReloader::new(reload_formats());

        let err = drive_loop(&mut state, &tx, &mut cycle, &mut reloader).await.unwrap_err();

        assert!(err.is::<ReloadRequested>(), "{err}");
        assert_eq!(reloader.tokens.len(), 2);
        assert_eq!(cycle.requests, 3);
        assert_eq!(state.stats.reloads, 2);
        assert!(logs.text().contains("reloading player response (1/2)"), "{}", logs.text());
        assert!(!logs.text().contains("secret"), "{}", logs.text());
    }

    #[tokio::test]
    async fn reload_fetch_failure_ends_the_stream() {
        struct FailingReloader;
        impl PlayerReloader for FailingReloader {
            fn fetch<'a>(&'a mut self, _token: Option<&'a str>) -> ReloadFuture<'a> {
                Box::pin(async { Err("reload needs WEB_REMIX".into()) })
            }
        }
        let mut state = test_state(GOOD_URL);
        let (tx, _rx) = mpsc::channel(1);
        let mut cycle = ReloadingCycle { requests: 0, reload_while: always };

        let err = drive_loop(&mut state, &tx, &mut cycle, &mut FailingReloader).await.unwrap_err();

        assert!(err.to_string().contains("WEB_REMIX"), "{err}");
        assert_eq!(cycle.requests, 1);
    }

}
