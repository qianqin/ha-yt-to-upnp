//! InnerTube /next: the next queue item and its own ctt.
//!
//! The phone hands over a ctt (credential transfer token, scope VIDEO) only
//! with setPlaylist/setVideo, and only for that one video. A receiver that
//! advances on its own gets the next video's ctt from /next, as
//! yt-cast-receiver does (DefaultPlaylistRequestHandler#getPreviousNextVideos):
//! a TVHTML5 request with the current video's ctt and the MDX context of the
//! connected senders returns the autoplay set, whose watch endpoint carries
//! the next video's ctt in `watchEndpointSupportedAuthorizationTokenConfig`.
//!
//! Tokens stay in memory, are never logged (lengths at most) and go only to
//! www.youtube.com through `credential_client()`.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const NEXT_URL: &str = "https://www.youtube.com/youtubei/v1/next?prettyPrint=false";

/// The TV client yt-cast-receiver sends /next as (TV_CLIENT_INFO).
const TV_CLIENT_NAME: &str = "TVHTML5";
const TV_CLIENT_NAME_ID: &str = "7";
const TV_CLIENT_VERSION: &str = "7.20230405.08.01";
const TV_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) \
                             Chrome/75.0.3770.142 Safari/537.36; SMART-TV; Tizen 4.0,gzip(gfe)";

/// Set to log the key structure (never values) of every /next response.
pub const DEBUG_ENV: &str = "YTCAST_DEBUG_NEXT";

/// The video now playing, as /next is asked about it.
pub struct WatchTarget<'a> {
    pub video_id: &'a str,
    /// The cast's list id; empty when it sent none.
    pub playlist_id: &'a str,
    pub params: Option<&'a str>,
    pub index: usize,
    /// This video's own ctt.
    pub ctt: &'a str,
    /// Lounge device ids of the connected senders (mdxConnectedDevices).
    pub device_ids: &'a [String],
}

/// The queue item /next names as the next one.
#[derive(Clone, PartialEq)]
pub struct NextVideo {
    pub video_id: String,
    pub ctt: Option<String>,
    pub params: Option<String>,
    pub playlist_id: Option<String>,
    pub index: Option<u64>,
}

/// Tokens are reported by length only.
impl std::fmt::Debug for NextVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NextVideo")
            .field("video_id", &self.video_id)
            .field("ctt_len", &self.ctt.as_deref().map(str::len))
            .field("params_len", &self.params.as_deref().map(str::len))
            .field("playlist_id_len", &self.playlist_id.as_deref().map(str::len))
            .field("index", &self.index)
            .finish()
    }
}

/// Where in the autoplay set the next item was found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NextSource {
    /// `nextVideoRenderer.autoplayEndpointRenderer.endpoint`: the queue's next.
    NextVideoRenderer,
    /// `autoplayVideoRenderer.mdxAutoplayVideoRenderer.navigationEndpoint`:
    /// an autoplay pick, used when the set has no next video.
    MdxAutoplay,
}

impl NextSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NextSource::NextVideoRenderer => "nextVideoRenderer",
            NextSource::MdxAutoplay => "mdxAutoplayVideoRenderer",
        }
    }
}

/// Build the TVHTML5 /next request for `target`. The page session gives the
/// visitor id, language and region; the only authentication is the ctt.
pub fn next_request(
    client: &reqwest::Client,
    target: &WatchTarget<'_>,
    session: &crate::music_session::YtMusicSession,
) -> Result<reqwest::Request> {
    let page_client = &session.innertube_context["client"];
    let mut tv_client = serde_json::json!({
        "clientName": TV_CLIENT_NAME,
        "clientVersion": TV_CLIENT_VERSION,
    });
    for key in ["hl", "gl", "visitorData"] {
        if let Some(value) = page_client.get(key).filter(|v| v.is_string()) {
            tv_client[key] = value.clone();
        }
    }
    let devices: Vec<serde_json::Value> = target
        .device_ids
        .iter()
        .map(|id| serde_json::json!({ "deviceId": id }))
        .collect();
    let mut body = serde_json::json!({
        "videoId": target.video_id,
        "playlistIndex": target.index,
        "enableMdxAutoplay": true,
        "isMdxPlayback": true,
        "mdxContext": { "mdxReceiverContext": { "mdxConnectedDevices": devices } },
        "context": {
            "client": tv_client,
            "user": {
                "enableSafetyMode": false,
                "lockedSafetyMode": false,
                "credentialTransferTokens": [{ "scope": "VIDEO", "token": target.ctt }],
            },
            "request": { "useSsl": true },
        },
    });
    if !target.playlist_id.is_empty() {
        body["playlistId"] = serde_json::json!(target.playlist_id);
    }
    if let Some(params) = target.params {
        body["params"] = serde_json::json!(params);
    }

    debug_assert!(crate::sabr::stream::is_trusted_google_host(NEXT_URL));
    let mut req = client
        .post(NEXT_URL)
        .header("User-Agent", TV_USER_AGENT)
        .header("Content-Type", "application/json")
        .header("X-YouTube-Client-Name", TV_CLIENT_NAME_ID)
        .header("X-YouTube-Client-Version", TV_CLIENT_VERSION);
    if let Some(vd) = session.visitor_data() {
        req = req.header("X-Goog-Visitor-Id", vd);
    }
    Ok(req.json(&body).timeout(NEXT_TIMEOUT).build().map_err(|e| e.without_url())?)
}

/// As for bgutil-pot: a hanging /next must not hold the chain back forever.
const NEXT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A /next answer is some 100-500 KB; anything past this is not one.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Read a response body, failing once it grows past `max` bytes.
async fn read_capped<S, E>(mut body: S, max: usize) -> Result<Vec<u8>>
where
    S: futures_util::Stream<Item = std::result::Result<bytes::Bytes, E>> + Unpin,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Into::into)?;
        if out.len() + chunk.len() > max {
            return Err(format!("/next answer larger than {max} bytes").into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The next item of a /next response: the queue's next video, else the
/// autoplay pick (as yt-cast-receiver reads it). `None` without either.
pub fn parse_next(response: &serde_json::Value) -> Option<(NextSource, NextVideo)> {
    let set = &response["contents"]["singleColumnWatchNextResults"]["autoplay"]["autoplay"]["sets"][0];
    let next = &set["nextVideoRenderer"]["autoplayEndpointRenderer"]["endpoint"]["watchEndpoint"];
    let autoplay = &set["autoplayVideoRenderer"]["mdxAutoplayVideoRenderer"]["navigationEndpoint"]["watchEndpoint"];
    watch_endpoint(next)
        .map(|v| (NextSource::NextVideoRenderer, v))
        .or_else(|| watch_endpoint(autoplay).map(|v| (NextSource::MdxAutoplay, v)))
}

fn watch_endpoint(endpoint: &serde_json::Value) -> Option<NextVideo> {
    let string = |v: &serde_json::Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let video_id = string(&endpoint["videoId"])?;
    let token = &endpoint["watchEndpointSupportedAuthorizationTokenConfig"]["videoAuthorizationToken"]
        ["credentialTransferTokens"][0]["token"];
    Some(NextVideo {
        video_id,
        ctt: string(token),
        params: string(&endpoint["params"]),
        playlist_id: string(&endpoint["playlistId"]),
        index: endpoint["index"].as_u64(),
    })
}

/// The key structure of `value` down to `depth` levels, without any value:
/// what to compare when YouTube changes the response shape.
pub fn json_shape(value: &serde_json::Value, depth: usize) -> String {
    match value {
        serde_json::Value::Object(map) if depth > 0 => {
            let inner: Vec<String> =
                map.iter().map(|(k, v)| format!("{k}{}", shape_suffix(v, depth - 1))).collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Object(_) => "{..}".to_owned(),
        serde_json::Value::Array(items) => match items.first() {
            Some(first) if depth > 0 => format!("[{}; {}]", json_shape(first, depth - 1), items.len()),
            _ => format!("[{}]", items.len()),
        },
        _ => "_".to_owned(),
    }
}

fn shape_suffix(value: &serde_json::Value, depth: usize) -> String {
    match value {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => format!(":{}", json_shape(value, depth)),
        _ => String::new(),
    }
}

/// Ask /next which item follows `target`, with its ctt if YouTube gave one.
pub async fn fetch_next(
    target: &WatchTarget<'_>,
    session: &crate::music_session::YtMusicSession,
) -> Result<Option<(NextSource, NextVideo)>> {
    let client = crate::sabr::stream::credential_client();
    let req = next_request(client, target, session)?;
    let resp = client.execute(req).await.map_err(|e| e.without_url())?;
    if !resp.status().is_success() {
        return Err(format!("/next HTTP {}", resp.status()).into());
    }
    use futures_util::TryStreamExt;
    let body = read_capped(resp.bytes_stream().map_err(|e| e.without_url()), MAX_RESPONSE_BYTES).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    if std::env::var_os(DEBUG_ENV).is_some() {
        let set = &json["contents"]["singleColumnWatchNextResults"]["autoplay"];
        tracing::info!("[next] response keys: {}", json_shape(&json, 2));
        tracing::info!("[next] autoplay keys: {}", json_shape(set, 7));
    }
    Ok(parse_next(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "sender-device-1";

    fn target<'a>(device_ids: &'a [String]) -> WatchTarget<'a> {
        WatchTarget {
            video_id: "aaaaaaaaaaa",
            playlist_id: "RDAMVMaaaaaaaaaaa",
            params: Some("cast-params"),
            index: 2,
            ctt: "ctt-current",
            device_ids,
        }
    }

    fn body(req: &reqwest::Request) -> serde_json::Value {
        serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[test]
    fn next_request_is_a_tv_mdx_request_with_the_current_videos_ctt() {
        let session = crate::music_session::test_fixtures::session();
        let devices = vec![DEVICE.to_owned()];
        let req = next_request(&reqwest::Client::new(), &target(&devices), &session).unwrap();

        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().host_str(), Some("www.youtube.com"));
        assert_eq!(req.url().path(), "/youtubei/v1/next");
        assert_eq!(req.headers()["x-youtube-client-name"], "7");
        assert_eq!(req.headers()["x-goog-visitor-id"], "CgtWSVNJVE9SLUlE");
        assert!(req.headers().get(reqwest::header::COOKIE).is_none());
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());

        let b = body(&req);
        assert_eq!(b["videoId"], "aaaaaaaaaaa");
        assert_eq!(b["playlistId"], "RDAMVMaaaaaaaaaaa");
        assert_eq!(b["params"], "cast-params");
        assert_eq!(b["playlistIndex"], 2);
        assert_eq!(b["enableMdxAutoplay"], true);
        assert_eq!(b["isMdxPlayback"], true);
        assert_eq!(
            b["mdxContext"],
            serde_json::json!({ "mdxReceiverContext": { "mdxConnectedDevices": [{ "deviceId": DEVICE }] } })
        );
        assert_eq!(
            b["context"]["user"],
            serde_json::json!({
                "enableSafetyMode": false,
                "lockedSafetyMode": false,
                "credentialTransferTokens": [{ "scope": "VIDEO", "token": "ctt-current" }],
            })
        );
        let client = &b["context"]["client"];
        assert_eq!(client["clientName"], "TVHTML5");
        assert_eq!(client["clientVersion"], TV_CLIENT_VERSION);
        assert_eq!(client["visitorData"], "CgtWSVNJVE9SLUlE");
        assert_eq!(client["hl"], "de");
        assert_eq!(client["gl"], "DE");
        // Nothing of the WEB_REMIX page client beyond those.
        assert!(client.get("platform").is_none() && client.get("configInfo").is_none(), "{client}");
    }

    #[test]
    fn next_request_times_out_after_ten_seconds() {
        let session = crate::music_session::test_fixtures::session();
        let req = next_request(&reqwest::Client::new(), &target(&[]), &session).unwrap();
        assert_eq!(req.timeout(), Some(&std::time::Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn the_answer_is_read_only_up_to_the_cap() {
        let chunks = |n: usize| {
            futures_util::stream::iter(
                (0..n).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"0123456789"))),
            )
        };
        assert_eq!(read_capped(chunks(3), 30).await.unwrap().len(), 30);
        let err = read_capped(chunks(4), 30).await.unwrap_err();
        assert!(err.to_string().contains("larger than 30 bytes"), "{err}");
    }

    #[test]
    fn next_request_leaves_out_what_the_cast_did_not_send() {
        let session = crate::music_session::test_fixtures::session();
        let t = WatchTarget { playlist_id: "", params: None, device_ids: &[], ..target(&[]) };
        let b = body(&next_request(&reqwest::Client::new(), &t, &session).unwrap());
        assert!(b.get("playlistId").is_none());
        assert!(b.get("params").is_none());
        // As the reference: an empty device list when no sender id is known.
        assert_eq!(b["mdxContext"]["mdxReceiverContext"]["mdxConnectedDevices"], serde_json::json!([]));
    }

    fn endpoint(video_id: &str, token: Option<&str>) -> serde_json::Value {
        let mut ep = serde_json::json!({
            "videoId": video_id,
            "playlistId": "RDAMVMaaaaaaaaaaa",
            "index": 3,
            "params": "next-params",
        });
        if let Some(token) = token {
            ep["watchEndpointSupportedAuthorizationTokenConfig"] = serde_json::json!({
                "videoAuthorizationToken": { "credentialTransferTokens": [{ "scope": "VIDEO", "token": token }] }
            });
        }
        ep
    }

    fn response(set: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "contents": { "singleColumnWatchNextResults": { "autoplay": { "autoplay": { "sets": [set] } } } }
        })
    }

    fn next_set(ep: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "nextVideoRenderer": { "autoplayEndpointRenderer": { "endpoint": { "watchEndpoint": ep } } } })
    }

    fn autoplay_set(ep: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "autoplayVideoRenderer": { "mdxAutoplayVideoRenderer": { "navigationEndpoint": { "watchEndpoint": ep } } }
        })
    }

    #[test]
    fn parses_the_next_video_with_its_ctt() {
        let (source, next) = parse_next(&response(next_set(endpoint("bbbbbbbbbbb", Some("ctt-next"))))).unwrap();
        assert_eq!(source, NextSource::NextVideoRenderer);
        assert_eq!(
            next,
            NextVideo {
                video_id: "bbbbbbbbbbb".into(),
                ctt: Some("ctt-next".into()),
                params: Some("next-params".into()),
                playlist_id: Some("RDAMVMaaaaaaaaaaa".into()),
                index: Some(3),
            }
        );
    }

    #[test]
    fn falls_back_to_the_mdx_autoplay_video() {
        let (source, next) =
            parse_next(&response(autoplay_set(endpoint("ccccccccccc", Some("ctt-auto"))))).unwrap();
        assert_eq!(source, NextSource::MdxAutoplay);
        assert_eq!(next.video_id, "ccccccccccc");
        assert_eq!(next.ctt.as_deref(), Some("ctt-auto"));
    }

    #[test]
    fn the_next_video_wins_over_the_autoplay_pick() {
        let mut set = next_set(endpoint("bbbbbbbbbbb", Some("ctt-next")));
        set.as_object_mut().unwrap().extend(autoplay_set(endpoint("ccccccccccc", None)).as_object().unwrap().clone());
        let (source, next) = parse_next(&response(set)).unwrap();
        assert_eq!((source, next.video_id.as_str()), (NextSource::NextVideoRenderer, "bbbbbbbbbbb"));
    }

    #[test]
    fn a_next_video_without_a_token_has_no_ctt() {
        let (_, next) = parse_next(&response(next_set(endpoint("bbbbbbbbbbb", None)))).unwrap();
        assert_eq!(next.ctt, None);
    }

    #[test]
    fn no_autoplay_set_means_no_next_video() {
        assert_eq!(parse_next(&serde_json::json!({ "contents": {} })), None);
        assert_eq!(parse_next(&response(serde_json::json!({}))), None);
        assert_eq!(parse_next(&response(next_set(serde_json::json!({ "videoId": "" })))), None);
    }

    #[test]
    fn debug_output_and_shape_never_show_token_values() {
        let json = response(next_set(endpoint("bbbbbbbbbbb", Some("ctt-secret-next"))));
        let (_, next) = parse_next(&json).unwrap();
        let shown = format!("{next:?} {}", json_shape(&json, 20));
        assert!(!shown.contains("ctt-secret-next"), "{shown}");
        assert!(!shown.contains("next-params"), "{shown}");
        assert!(shown.contains("credentialTransferTokens"), "{shown}");
        assert!(shown.contains("nextVideoRenderer"), "{shown}");
    }
}
