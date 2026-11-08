pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub title: String,
    pub duration: u64,
    pub stream_url: String,
}

/// Resolve a video ID to a playable audio stream URL.
///
/// Makes a lightweight InnerTube /player call to get the real title and
/// duration (needed for YouTube seekbar sync), then returns a localhost URL
/// that the DIAL server will handle via SABR when MPD connects.
///
/// Requests go through `credential_client()`: the body carries the cast's
/// ctt, so redirects must not re-send it off Google hosts.
pub async fn resolve_stream(
    video_id: &str,
    ctt: Option<&str>,
    _playlist_id: Option<&str>,
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<Option<StreamInfo>> {
    let client = crate::sabr::stream::credential_client();
    let stream_url = format!("http://localhost:8008/stream/{}", video_id);

    let (title, duration) = match player_request(client, video_id, ctt, yt_session) {
        Ok(PlayerRequest::WebRemix(req)) => match player_call(client, video_id, req).await {
            Ok(td) => td,
            Err(e) => {
                tracing::warn!("[innertube] WEB_REMIX failed: {}, trying IOS", e);
                ios_or_fallback(client, video_id).await
            }
        },
        Ok(PlayerRequest::Ios(req)) => match player_call(client, video_id, req).await {
            Ok(td) => td,
            Err(e) => {
                tracing::warn!("[innertube] IOS failed: {}, using fallback", e);
                (video_id.to_owned(), 0)
            }
        },
        Err(e) => {
            tracing::warn!("[innertube] could not build /player request: {}, using fallback", e);
            (video_id.to_owned(), 0)
        }
    };

    tracing::info!("[stream] {video_id} → \"{}\" [{}s] via SABR", title, duration);
    Ok(Some(StreamInfo {
        title,
        duration,
        stream_url,
    }))
}

/// The first /player request to try, by client.
enum PlayerRequest {
    WebRemix(reqwest::Request),
    Ios(reqwest::Request),
}

/// WEB_REMIX when the cast sent a ctt and the page session exists (the rule
/// in `music_session::web_remix_auth`), IOS otherwise.
fn player_request(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<PlayerRequest> {
    match crate::music_session::web_remix_auth(ctt, yt_session) {
        Ok(auth) => Ok(PlayerRequest::WebRemix(web_remix_request(client, video_id, auth)?)),
        Err(reason) => {
            tracing::info!("[innertube] {} — using IOS", reason);
            Ok(PlayerRequest::Ios(ios_request(client, video_id)?))
        }
    }
}

async fn ios_or_fallback(client: &reqwest::Client, video_id: &str) -> (String, u64) {
    let result = match ios_request(client, video_id) {
        Ok(req) => player_call(client, video_id, req).await,
        Err(e) => Err(e),
    };
    result.unwrap_or_else(|e| {
        tracing::warn!("[innertube] IOS failed: {}, using fallback", e);
        (video_id.to_owned(), 0)
    })
}

async fn player_call(
    client: &reqwest::Client,
    video_id: &str,
    req: reqwest::Request,
) -> Result<(String, u64)> {
    let resp = client.execute(req).await.map_err(|e| e.without_url())?;

    if !resp.status().is_success() {
        return Err(format!("/player HTTP {}", resp.status()).into());
    }

    parse_player_response(video_id, resp).await
}

/// WEB_REMIX /player request. The only authentication is the cast's ctt in
/// `context.user`; the session supplies the anonymous page context.
fn web_remix_request(
    client: &reqwest::Client,
    video_id: &str,
    auth: crate::music_session::WebRemixAuth<'_>,
) -> Result<reqwest::Request> {
    let session = auth.session;
    let mut playback_ctx = serde_json::json!({
        "contentPlaybackContext": {
            "html5Preference": "HTML5_PREF_WANTS",
        }
    });
    if let Some(sts) = session.signature_timestamp {
        playback_ctx["contentPlaybackContext"]["signatureTimestamp"] = serde_json::json!(sts);
    }
    let body = serde_json::json!({
        "videoId": video_id,
        "context": session.player_context(Some(auth.ctt)),
        "contentCheckOk": true,
        "racyCheckOk": true,
        "playbackContext": playback_ctx,
    });

    let mut req = client
        .post("https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.youtube.com")
        .header("Referer", "https://music.youtube.com/")
        .header("X-YouTube-Client-Name", "67");
    if let Some(version) = session.client_version() {
        req = req.header("X-YouTube-Client-Version", version);
    }
    if let Some(vd) = session.visitor_data() {
        req = req.header("X-Goog-Visitor-Id", vd);
    }

    Ok(req.json(&body).build().map_err(|e| e.without_url())?)
}

fn ios_request(client: &reqwest::Client, video_id: &str) -> Result<reqwest::Request> {
    let body = serde_json::json!({
        "videoId": video_id,
        "context": {
            "client": {
                "clientName": "IOS",
                "clientVersion": "19.45.4",
                "deviceMake": "Apple",
                "deviceModel": "iPhone16,2",
                "osName": "iPhone",
                "osVersion": "18.1.0.22B83",
                "hl": "en",
                "gl": "US",
            }
        },
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    Ok(client
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("User-Agent", "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)")
        .header("Content-Type", "application/json")
        .json(&body)
        .build()?)
}

async fn parse_player_response(
    video_id: &str,
    resp: reqwest::Response,
) -> Result<(String, u64)> {
    let json: serde_json::Value = resp.json().await?;
    let status = json["playabilityStatus"]["status"]
        .as_str()
        .unwrap_or("UNKNOWN");
    if status != "OK" {
        let reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("unknown");
        tracing::warn!("[innertube] {} not playable: {} ({})", video_id, status, reason);
        return Err(format!("{}: {} ({})", video_id, status, reason).into());
    }
    let title = json["videoDetails"]["title"]
        .as_str()
        .unwrap_or(video_id)
        .to_owned();
    let duration = json["videoDetails"]["lengthSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Ok((title, duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_json(req: &reqwest::Request) -> serde_json::Value {
        serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    fn assert_no_stored_credentials(req: &reqwest::Request) {
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn web_remix_request_carries_ctt_and_no_stored_credentials() {
        let session = crate::music_session::test_fixtures::session();
        let client = reqwest::Client::new();

        let Ok(PlayerRequest::WebRemix(req)) =
            player_request(&client, "dQw4w9WgXcQ", Some("cast-token"), Some(&session))
        else {
            panic!("expected WEB_REMIX with ctt and session");
        };

        assert_eq!(req.url().host_str(), Some("music.youtube.com"));
        let body = body_json(&req);
        assert_eq!(
            body["context"]["user"]["credentialTransferTokens"],
            serde_json::json!([{ "scope": "VIDEO", "token": "cast-token" }])
        );
        assert_eq!(body["context"]["client"]["visitorData"], "CgtWSVNJVE9SLUlE");
        assert_eq!(body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"], 20391);
        assert_eq!(req.headers()["x-youtube-client-version"], "1.20261102.01.00");
        assert_eq!(req.headers()["x-goog-visitor-id"], "CgtWSVNJVE9SLUlE");
        assert_no_stored_credentials(&req);
    }

    #[test]
    fn ios_is_used_without_ctt_or_session() {
        let session = crate::music_session::test_fixtures::session();
        let client = reqwest::Client::new();
        for (ctt, session) in [(Some("cast-token"), None), (None, Some(&session)), (None, None)] {
            let Ok(PlayerRequest::Ios(req)) = player_request(&client, "dQw4w9WgXcQ", ctt, session) else {
                panic!("expected IOS for ctt={:?} session={}", ctt, session.is_some());
            };
            assert_eq!(req.url().host_str(), Some("www.youtube.com"));
            let body = body_json(&req);
            assert_eq!(body["context"]["client"]["clientName"], "IOS");
            assert!(body["context"].get("user").is_none(), "ctt must not go to IOS");
            assert_no_stored_credentials(&req);
        }
    }
}
