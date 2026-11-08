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
pub async fn resolve_stream(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    _playlist_id: Option<&str>,
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<Option<StreamInfo>> {
    let stream_url = format!("http://localhost:8008/stream/{}", video_id);

    // WEB_REMIX is the only client that accepts the cast's credential
    // transfer token; without one there is nothing to authenticate with.
    let (title, duration) = if ctt.is_some() {
        match web_remix_player(client, video_id, ctt, yt_session).await {
            Ok(td) => td,
            Err(e) => {
                tracing::warn!("[innertube] WEB_REMIX failed: {}, trying IOS", e);
                ios_player(client, video_id).await.unwrap_or_else(|e2| {
                    tracing::warn!("[innertube] IOS failed: {}, using fallback", e2);
                    (video_id.to_owned(), 0)
                })
            }
        }
    } else {
        ios_player(client, video_id).await.unwrap_or_else(|e| {
            tracing::warn!("[innertube] IOS failed: {}, using fallback", e);
            (video_id.to_owned(), 0)
        })
    };

    tracing::info!("[stream] {video_id} → \"{}\" [{}s] via SABR", title, duration);
    Ok(Some(StreamInfo {
        title,
        duration,
        stream_url,
    }))
}

const WEB_REMIX_FALLBACK_VERSION: &str = "1.20260114.03.00";

async fn web_remix_player(
    client: &reqwest::Client,
    video_id: &str,
    ctt: Option<&str>,
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<(String, u64)> {
    let req = web_remix_request(client, video_id, ctt, yt_session)?;
    let resp = client.execute(req).await?;

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
    ctt: Option<&str>,
    yt_session: Option<&crate::music_session::YtMusicSession>,
) -> Result<reqwest::Request> {
    let body = if let Some(session) = yt_session {
        let mut playback_ctx = serde_json::json!({
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
            }
        });
        if let Some(sts) = session.signature_timestamp {
            playback_ctx["contentPlaybackContext"]["signatureTimestamp"] = serde_json::json!(sts);
        }
        serde_json::json!({
            "videoId": video_id,
            "context": session.player_context(ctt),
            "contentCheckOk": true,
            "racyCheckOk": true,
            "playbackContext": playback_ctx,
        })
    } else {
        // Minimal context (may return UNPLAYABLE without full ytcfg)
        let mut context = serde_json::json!({
            "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": WEB_REMIX_FALLBACK_VERSION,
                "hl": "en",
                "gl": "US",
            }
        });
        if let Some(token) = ctt {
            context["user"] = serde_json::json!({
                "credentialTransferTokens": [{
                    "scope": "VIDEO",
                    "token": token,
                }]
            });
        }
        serde_json::json!({
            "videoId": video_id,
            "context": context,
            "contentCheckOk": true,
            "racyCheckOk": true,
        })
    };

    let mut req = client
        .post("https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.youtube.com")
        .header("Referer", "https://music.youtube.com/")
        .header("X-YouTube-Client-Name", "67")
        .header("X-YouTube-Client-Version",
            yt_session
                .and_then(|s| s.client_version())
                .unwrap_or(WEB_REMIX_FALLBACK_VERSION)
        );

    if let Some(vd) = yt_session.and_then(|s| s.visitor_data()) {
        req = req.header("X-Goog-Visitor-Id", vd);
    }

    Ok(req.json(&body).build()?)
}

async fn ios_player(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<(String, u64)> {
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

    let resp = client
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("User-Agent", "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("/player HTTP {}", resp.status()).into());
    }

    parse_player_response(video_id, resp).await
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

    #[test]
    fn web_remix_request_carries_ctt_and_no_stored_credentials() {
        let session = crate::music_session::test_fixtures::session();
        let client = reqwest::Client::new();

        let req = web_remix_request(&client, "dQw4w9WgXcQ", Some("cast-token"), Some(&session)).unwrap();

        let body = body_json(&req);
        assert_eq!(
            body["context"]["user"]["credentialTransferTokens"],
            serde_json::json!([{ "scope": "VIDEO", "token": "cast-token" }])
        );
        assert_eq!(body["context"]["client"]["visitorData"], "CgtWSVNJVE9SLUlE");
        assert_eq!(body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"], 20391);
        assert_eq!(req.headers()["x-youtube-client-version"], "1.20261102.01.00");
        assert_eq!(req.headers()["x-goog-visitor-id"], "CgtWSVNJVE9SLUlE");
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn web_remix_request_without_session_still_carries_ctt() {
        let client = reqwest::Client::new();
        let req = web_remix_request(&client, "dQw4w9WgXcQ", Some("cast-token"), None).unwrap();
        assert_eq!(body_json(&req)["context"]["user"]["credentialTransferTokens"][0]["token"], "cast-token");
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }
}
