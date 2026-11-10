use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct DialState {
    /// Whether the "YouTube" DIAL app is currently running.
    pub running: AtomicBool,
    /// Channel to deliver pairing codes for the "cl" (YouTube) session.
    pub pairing_tx: mpsc::Sender<String>,
    /// Channel to deliver pairing codes for the "m" (YouTube Music) session.
    pub pairing_tx_m: mpsc::Sender<String>,
    /// Asks the player loop to start the silence pre-roll. Capacity 1: a
    /// request still waiting covers any further launch.
    pub preroll_tx: mpsc::Sender<()>,
    /// Human-readable name shown during discovery (e.g. "Living Room Pi").
    pub device_name: String,
    /// UPnP UUID for this device (without the "uuid:" prefix).
    pub uuid: String,
    /// Local IP for building absolute Application-URL.
    pub local_ip: std::net::Ipv4Addr,
    /// Port this DIAL server listens on.
    pub port: u16,
    /// Credential transfer token from the casting session: the receiver's
    /// only authentication, sent per video with WEB_REMIX /player.
    pub ctt: std::sync::RwLock<Option<String>>,
    /// Anonymous YouTube Music session (full INNERTUBE_CONTEXT from page).
    pub yt_session: Option<std::sync::Arc<crate::music_session::YtMusicSession>>,
    /// Screen ID for the "cl" (YouTube) Lounge session, exposed in DIAL additionalData.
    pub screen_id: String,
    /// Last `/stream` request that failed: (video_id, reason). Lets the
    /// player tell a failed stream from a song that ended, since MPD just
    /// stops in both cases.
    pub stream_failure: std::sync::Mutex<Option<(String, String)>>,
    /// Caps the endless `/silence` streams open at once.
    pub silence_slots: SilenceSlots,
}

impl DialState {
    /// Records a failed `/stream` for `video_id`, or forgets any earlier
    /// failure on success: MPD plays one stream at a time, so it is stale.
    fn note_stream_result(&self, video_id: &str, result: std::result::Result<(), String>) {
        *self.stream_failure.lock().unwrap() =
            result.err().map(|reason| (video_id.to_owned(), reason));
    }

    /// Takes the recorded failure; returns its reason if it was for `video_id`.
    pub fn take_stream_failure(&self, video_id: &str) -> Option<String> {
        self.stream_failure
            .lock()
            .unwrap()
            .take()
            .filter(|(vid, _)| vid == video_id)
            .map(|(_, reason)| reason)
    }
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Start the DIAL HTTP server on the given port.
///
/// This serves the UPnP device description and the `/apps/YouTube` DIAL
/// endpoints that the YouTube Music app talks to after SSDP discovery.
pub async fn run_dial_server(
    port: u16,
    state: Arc<DialState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let app = build_router(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("DIAL server listening on 0.0.0.0:{port}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        // Wait until shutdown is signalled
        loop {
            if *shutdown.borrow() {
                return;
            }
            if shutdown.changed().await.is_err() {
                return;
            }
        }
    })
    .await?;

    Ok(())
}

fn build_router(state: Arc<DialState>) -> Router {
    Router::new()
        .route("/ssdp/device-desc.xml", get(device_description))
        .route("/apps/YouTube", get(app_status))
        .route("/apps/YouTube", post(app_launch))
        .route("/apps/YouTube/run", delete(app_stop))
        .route("/stream/{video_id}", get(stream_audio))
        .route(SILENCE_PATH, get(silence_audio))
        .with_state(state)
}

/// `/stream` is only meant for the local MPD; the server binds 0.0.0.0 for
/// DIAL, so the peer address is the only thing keeping LAN clients out.
fn is_loopback_peer(peer: SocketAddr) -> bool {
    // to_canonical() folds IPv4-mapped IPv6 (::ffff:127.x) back to IPv4.
    peer.ip().to_canonical().is_loopback()
}

#[cfg(test)]
impl DialState {
    /// A state with documentation-range addresses and no session.
    pub(crate) fn for_tests(preroll_tx: mpsc::Sender<()>) -> Arc<Self> {
        let (pairing_tx, _) = mpsc::channel(1);
        let (pairing_tx_m, _) = mpsc::channel(1);
        Arc::new(DialState {
            preroll_tx,
            running: AtomicBool::new(false),
            pairing_tx,
            pairing_tx_m,
            device_name: "Test Receiver".to_string(),
            uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            local_ip: std::net::Ipv4Addr::new(192, 0, 2, 1),
            port: 8008,
            ctt: std::sync::RwLock::new(None),
            yt_session: None,
            screen_id: "test-screen".to_string(),
            stream_failure: Default::default(),
            silence_slots: Default::default(),
        })
    }
}

// ---------------------------------------------------------------------------
// GET /ssdp/device-desc.xml
// ---------------------------------------------------------------------------

async fn device_description(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let base_url = format!("http://{}:{}", state.local_ip, state.port);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion>
    <major>1</major>
    <minor>0</minor>
  </specVersion>
  <URLBase>{base_url}</URLBase>
  <device>
    <deviceType>urn:dial-multiscreen-org:device:dial:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>ytcast-lite</manufacturer>
    <modelName>ytcast-lite 0.1</modelName>
    <UDN>uuid:{uuid}</UDN>
    <iconList>
      <icon>
        <mimetype>image/png</mimetype>
        <width>48</width>
        <height>48</height>
        <depth>24</depth>
        <url>/icon.png</url>
      </icon>
    </iconList>
    <serviceList>
      <service>
        <serviceType>urn:dial-multiscreen-org:service:dial:1</serviceType>
        <serviceId>urn:dial-multiscreen-org:serviceId:dial</serviceId>
        <controlURL>/ssdp/notfound</controlURL>
        <eventSubURL>/ssdp/notfound</eventSubURL>
        <SCPDURL>/ssdp/notfound</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        base_url = base_url,
        name = state.device_name,
        uuid = state.uuid,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    // The Application-URL header MUST be an absolute URL per DIAL spec Section 5.4.
    // No trailing slash -- matches peer-dial Node.js reference implementation.
    let app_url = format!("http://{}:{}/apps", state.local_ip, state.port);
    headers.insert(
        HeaderName::from_static("application-url"),
        app_url.parse().unwrap(),
    );

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// GET /apps/YouTube -- app status
// ---------------------------------------------------------------------------

async fn app_status(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);

    let running = state.running.load(Ordering::Relaxed);
    let state_str = if running { "running" } else { "stopped" };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="1.7">
  <name>YouTube</name>
  <options allowStop="true"/>
  <state>{state_str}</state>
  <link rel="run" href="run"/>
  <additionalData>
    <screenId>{screen_id}</screenId>
  </additionalData>
</service>"#,
        screen_id = state.screen_id,
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());

    (StatusCode::OK, headers, xml).into_response()
}

// ---------------------------------------------------------------------------
// POST /apps/YouTube -- launch
// ---------------------------------------------------------------------------

async fn app_launch(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
    body: String,
) -> Response {
    // The body carries the pairing code, so only its size is logged.
    tracing::debug!("DIAL {} {} from {} ({} byte body)", method, uri, addr, body.len());

    // Body is form-urlencoded: pairingCode=XXX&theme=m&v=VIDEO_ID&t=0
    let params: Vec<(String, String)> =
        serde_urlencoded::from_str(&body).unwrap_or_default();

    let theme = params.iter().find(|(k, _)| k == "theme").map(|(_, v)| v.as_str()).unwrap_or("cl");

    if let Some((_, code)) = params.iter().find(|(k, _)| k == "pairingCode") {
        if !code.is_empty() {
            let tx = if theme == "m" { &state.pairing_tx_m } else { &state.pairing_tx };
            tracing::info!("DIAL launch: theme={}, pairing code routing to {} session", theme, theme);
            if let Err(e) = tx.send(code.clone()).await {
                tracing::error!("failed to send pairing code: {e}");
            }
            // Only a launch that pairs a phone may wake the speaker: any LAN
            // peer can POST here.
            request_preroll(&state.preroll_tx);
        }
    }

    state.running.store(true, Ordering::Relaxed);

    // DIAL spec: 201 Created with LOCATION pointing to the running instance.
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, "/apps/YouTube/run".parse().unwrap());

    (StatusCode::CREATED, headers).into_response()
}

/// Start waking the speaker while the phone is still loading the song.
/// Never waits: the DIAL answer must not be delayed by MPD.
fn request_preroll(tx: &mpsc::Sender<()>) {
    match tx.try_send(()) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(())) => {
            tracing::debug!("DIAL launch: pre-roll request already pending");
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            tracing::debug!("DIAL launch: player loop gone, no pre-roll");
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /apps/YouTube/run -- stop
// ---------------------------------------------------------------------------

async fn app_stop(
    method: Method,
    uri: Uri,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> StatusCode {
    tracing::debug!("DIAL {} {} from {}", method, uri, addr);
    state.running.store(false, Ordering::Relaxed);
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// GET /silence -- endless silence for the pre-roll
// ---------------------------------------------------------------------------

/// Endless silence MPD plays as the pre-roll while a cast is still loading.
pub const SILENCE_PATH: &str = "/silence";
/// Raw PCM, decoded by MPD's built-in `pcm` decoder plugin, so it needs no
/// optional decoder (ffmpeg, sndfile) and no header with a length.
/// All-zero samples are silence in either byte order.
const SILENCE_CONTENT_TYPE: &str = "audio/L16;rate=44100;channels=2";
/// About 93 ms of 16-bit stereo at 44.1 kHz per chunk.
static SILENCE_CHUNK: [u8; 16 * 1024] = [0; 16 * 1024];
/// MPD needs one; the second covers a stream that is still closing while
/// the next pre-roll opens its own.
const SILENCE_MAX_STREAMS: usize = 2;

pub struct SilenceSlots(Arc<tokio::sync::Semaphore>);

impl Default for SilenceSlots {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(SILENCE_MAX_STREAMS)))
    }
}

/// Not paced: MPD reads only as far ahead as its input buffer, and TCP
/// back-pressure suspends the stream until it reads again.
async fn silence_audio(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    if !is_loopback_peer(peer) {
        tracing::debug!("[silence] rejected non-loopback peer {}", peer);
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(slot) = state.silence_slots.0.clone().try_acquire_owned() else {
        tracing::debug!("[silence] all {SILENCE_MAX_STREAMS} streams busy");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // The body owns the slot, so it is freed when the stream is dropped.
    let chunks = futures_util::stream::repeat_with(move || {
        let _slot = &slot;
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(&SILENCE_CHUNK))
    });
    (
        [(header::CONTENT_TYPE, SILENCE_CONTENT_TYPE)],
        axum::body::Body::from_stream(chunks),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /stream/:video_id -- SABR audio stream
// ---------------------------------------------------------------------------

/// Start offset of a `Range: bytes=N-` (or `bytes=N-M`) request, as MPD
/// sends when it seeks in a stream of known length; 0 for anything else.
fn range_start(headers: &HeaderMap) -> u64 {
    headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("bytes="))
        .filter(|spec| !spec.contains(','))
        .and_then(|spec| spec.split_once('-'))
        .and_then(|(start, _)| start.trim().parse().ok())
        .unwrap_or(0)
}

/// Status and framing headers. A stream of known length (url transport) gets
/// Content-Length and Accept-Ranges, so MPD can seek with a range request (206
/// from `start`); SABR streams stay chunked and unseekable, as before.
fn stream_framing(content_length: Option<u64>, start: u64) -> (StatusCode, HeaderMap) {
    let mut headers = HeaderMap::new();
    let Some(len) = content_length else {
        headers.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        return (StatusCode::OK, headers);
    };
    headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, (len - start.min(len)).to_string().parse().unwrap());
    if start == 0 {
        return (StatusCode::OK, headers);
    }
    headers.insert(
        header::CONTENT_RANGE,
        format!("bytes {}-{}/{}", start, len.saturating_sub(1), len).parse().unwrap(),
    );
    (StatusCode::PARTIAL_CONTENT, headers)
}

async fn stream_audio(
    axum::extract::Path(video_id): axum::extract::Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
    request_headers: HeaderMap,
) -> Response {
    if !is_loopback_peer(peer) {
        tracing::debug!("[stream] rejected non-loopback peer {}", peer);
        return StatusCode::FORBIDDEN.into_response();
    }

    // Validate video ID (11 chars, alphanumeric + - + _)
    if !video_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        || video_id.len() != 11
    {
        return (StatusCode::BAD_REQUEST, "invalid video id").into_response();
    }

    serve_stream(&state, &video_id, &request_headers, |video_id, ctt, yt_session, start| async move {
        crate::sabr::stream::stream_audio(&video_id, ctt.as_deref(), yt_session, start).await
    })
    .await
}

type StreamResult = std::result::Result<
    (crate::sabr::stream::SabrStreamInfo, mpsc::Receiver<bytes::Bytes>),
    crate::sabr::stream::SabrError,
>;

/// The `/stream` response for a validated request: the Range offset goes to
/// `open` (the real one is `sabr::stream::stream_audio`), and its result is
/// framed (200, 206, or 416 past the end).
async fn serve_stream<F, Fut>(
    state: &DialState,
    video_id: &str,
    request_headers: &HeaderMap,
    open: F,
) -> Response
where
    F: FnOnce(String, Option<String>, Option<Arc<crate::music_session::YtMusicSession>>, u64) -> Fut,
    Fut: std::future::Future<Output = StreamResult>,
{
    let ctt = state.ctt.read().unwrap().clone();
    let yt_session = state.yt_session.clone();
    let start = range_start(request_headers);
    match open(video_id.to_owned(), ctt, yt_session, start).await {
        Ok((info, rx)) => {
            if start == 0 {
                state.note_stream_result(video_id, Ok(()));
            }
            // Build streaming response from the mpsc receiver.
            // When YTCAST_DUMP_STREAM=1, also write every byte of a stream
            // from its start to /tmp/sabr-dump-{id}.webm, so it can be
            // inspected with ffprobe if MPD crashes (a range request would
            // overwrite it with a tail).
            let dump_file = if start == 0 && std::env::var("YTCAST_DUMP_STREAM").is_ok() {
                let path = format!("/tmp/sabr-dump-{}.webm", video_id);
                tracing::info!("[stream] dumping raw stream to {}", path);
                std::fs::File::create(&path).ok()
            } else {
                None
            };
            let dump_file = std::sync::Arc::new(std::sync::Mutex::new(dump_file));

            let stream = ReceiverStream::new(rx).map(move |chunk| {
                if let Ok(mut guard) = dump_file.lock() {
                    if let Some(ref mut f) = *guard {
                        use std::io::Write;
                        let _ = f.write_all(&chunk);
                    }
                }
                Ok::<_, std::io::Error>(chunk)
            });
            let body = axum::body::Body::from_stream(stream);

            let (status, mut headers) = stream_framing(info.content_length, info.start_offset);
            headers.insert(
                header::CONTENT_TYPE,
                info.mime_type
                    .parse()
                    .unwrap_or_else(|_| "audio/webm".parse().unwrap()),
            );
            // Replace non-ASCII characters with '?' for a header-safe title
            let safe_title: String = info
                .title
                .chars()
                .map(|c| if c.is_ascii() { c } else { '?' })
                .collect();
            headers.insert(
                HeaderName::from_static("x-title"),
                safe_title
                    .parse()
                    .unwrap_or_else(|_| "unknown".parse().unwrap()),
            );
            headers.insert(
                HeaderName::from_static("x-duration"),
                info.duration_secs.to_string().parse().unwrap(),
            );

            (status, headers, body).into_response()
        }
        Err(e) => {
            if let Some(past_end) = e.downcast_ref::<crate::sabr::stream::RangeNotSatisfiable>() {
                tracing::info!("[stream] {} range from byte {}: past the end", video_id, start);
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{}", past_end.content_length).parse().unwrap(),
                );
                return (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response();
            }
            tracing::error!("[stream] {} failed (from byte {}): {}", video_id, start, e);
            // A song that never started is recorded before responding, so it
            // is visible once MPD stops; a failed seek is not a song failure.
            if start == 0 {
                state.note_stream_result(video_id, Err(e.to_string()));
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("error: {}", e),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::connect_info::MockConnectInfo;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn peer(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 40000)
    }

    fn test_state() -> Arc<DialState> {
        let (preroll_tx, _) = mpsc::channel(1);
        test_state_with(preroll_tx)
    }

    fn test_state_with(preroll_tx: mpsc::Sender<()>) -> Arc<DialState> {
        DialState::for_tests(preroll_tx)
    }

    /// Serve the real router on an ephemeral loopback port while pretending the
    /// TCP peer is `peer_addr`, then issue a GET for `path`.
    async fn get_as(peer_addr: SocketAddr, path: &str) -> StatusCode {
        let (response, server) = fetch_as(peer_addr, path).await;
        let status = response.status();
        server.abort();
        StatusCode::from_u16(status.as_u16()).unwrap()
    }

    /// Like `get_as`, but hands back the response (its body may be endless)
    /// and the server task, which the caller aborts when done.
    async fn fetch_as(
        peer_addr: SocketAddr,
        path: &str,
    ) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        let app = build_router(test_state()).layer(MockConnectInfo(peer_addr));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service()).await.unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        (response, server)
    }

    fn lan_peers() -> Vec<SocketAddr> {
        vec![
            peer(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))),
            peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))),
            peer(IpAddr::V6("fd00::5".parse().unwrap())),
            peer(IpAddr::V6("2001:db8::5".parse().unwrap())),
            peer(IpAddr::V6("::ffff:192.0.2.10".parse().unwrap())),
        ]
    }

    #[test]
    fn loopback_peers_are_recognised() {
        assert!(is_loopback_peer(peer(IpAddr::V4(Ipv4Addr::LOCALHOST))));
        assert!(is_loopback_peer(peer(IpAddr::V4(Ipv4Addr::new(127, 1, 2, 3)))));
        assert!(is_loopback_peer(peer(IpAddr::V6(Ipv6Addr::LOCALHOST))));
    }

    #[test]
    fn ipv4_mapped_loopback_is_loopback() {
        assert!(is_loopback_peer(peer(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()))));
    }

    #[test]
    fn lan_peers_are_not_loopback() {
        for p in lan_peers() {
            assert!(!is_loopback_peer(p), "{p} must not be treated as loopback");
        }
    }

    #[tokio::test]
    async fn debug_endpoints_are_gone() {
        let local = peer(IpAddr::V4(Ipv4Addr::LOCALHOST));
        for path in [
            "/debug/play/abc",
            "/debug/pause",
            "/debug/resume",
            "/debug/seek/10",
            "/debug/stop",
        ] {
            assert_eq!(get_as(local, path).await, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn stream_from_lan_is_forbidden() {
        // A well-formed video id: without the peer check this would start a
        // YouTube fetch, so 403 proves the request was stopped before that.
        for p in lan_peers() {
            assert_eq!(
                get_as(p, "/stream/dQw4w9WgXcQ").await,
                StatusCode::FORBIDDEN,
                "{p}"
            );
        }
    }

    #[tokio::test]
    async fn stream_from_loopback_is_not_rejected() {
        // Invalid id so the handler answers 400 without any network fetch,
        // which proves the request got past the peer check.
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()),
        ] {
            assert_eq!(
                get_as(peer(ip), "/stream/bad").await,
                StatusCode::BAD_REQUEST,
                "{ip}"
            );
        }
    }

    #[tokio::test]
    async fn silence_from_lan_is_forbidden() {
        for p in lan_peers() {
            assert_eq!(get_as(p, SILENCE_PATH).await, StatusCode::FORBIDDEN, "{p}");
        }
    }

    #[tokio::test]
    async fn silence_from_loopback_is_an_endless_pcm_stream_of_zeros() {
        let (mut response, server) = fetch_as(peer(IpAddr::V4(Ipv4Addr::LOCALHOST)), SILENCE_PATH).await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "audio/L16;rate=44100;channels=2"
        );
        // Several seconds' worth of audio: more than any single chunk, so the
        // body keeps coming instead of ending after a fixed buffer.
        let mut read = 0;
        while read < 4 * 44_100 * 4 {
            let chunk = response.chunk().await.unwrap().expect("silence must not end");
            assert!(chunk.iter().all(|&b| b == 0), "silence must be all zero samples");
            read += chunk.len();
        }
        server.abort();
    }

    #[tokio::test]
    async fn dial_launch_does_not_log_pairing_code() {
        const CODE: &str = "SECRETPAIRCODE123";
        // Current-thread test runtime: the handler runs on this thread, so
        // the capture sees all of its events.
        let capture = crate::storage::test_util::LogCapture::start();

        let status = app_launch(
            Method::POST,
            "/apps/YouTube".parse().unwrap(),
            ConnectInfo(peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))),
            State(test_state()),
            format!("pairingCode={CODE}&theme=m&v=dQw4w9WgXcQ&t=0"),
        )
        .await
        .status();

        assert_eq!(status, StatusCode::CREATED);
        let logs = capture.text();
        assert!(logs.contains("DIAL"), "log capture is not working: {logs:?}");
        assert!(!logs.contains(CODE), "pairing code leaked into logs: {logs}");
    }

    /// A DIAL launch as the phone sends it; fails the test if the handler
    /// does not answer at once.
    async fn launch(state: Arc<DialState>, theme: &str) -> StatusCode {
        let handler = app_launch(
            Method::POST,
            "/apps/YouTube".parse().unwrap(),
            ConnectInfo(peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))),
            State(state),
            format!("pairingCode=CODE&theme={theme}&v=dQw4w9WgXcQ&t=0"),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), handler)
            .await
            .expect("DIAL launch must not wait for the pre-roll")
            .status()
    }

    #[tokio::test]
    async fn dial_launch_requests_a_preroll_for_both_themes() {
        for theme in ["m", "cl"] {
            let (tx, mut rx) = mpsc::channel(1);
            assert_eq!(launch(test_state_with(tx), theme).await, StatusCode::CREATED);
            assert_eq!(rx.try_recv(), Ok(()), "theme={theme}");
        }
    }

    #[tokio::test]
    async fn launch_without_pairing_code_requests_no_preroll() {
        // Any LAN peer can POST a launch; only a phone that pairs may wake
        // the speaker and take it from its other source.
        for body in ["", "theme=m&v=dQw4w9WgXcQ", "pairingCode=&theme=m"] {
            let (tx, mut rx) = mpsc::channel(1);
            let status = app_launch(
                Method::POST,
                "/apps/YouTube".parse().unwrap(),
                ConnectInfo(peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)))),
                State(test_state_with(tx)),
                body.to_string(),
            )
            .await
            .status();
            assert_eq!(status, StatusCode::CREATED, "{body:?}");
            assert!(rx.try_recv().is_err(), "pre-roll requested for {body:?}");
        }
    }

    fn range(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, value.parse().unwrap());
        headers
    }

    #[test]
    fn range_start_reads_mpd_seek_requests() {
        assert_eq!(range_start(&HeaderMap::new()), 0);
        assert_eq!(range_start(&range("bytes=0-")), 0);
        assert_eq!(range_start(&range("bytes=123456-")), 123_456);
        assert_eq!(range_start(&range("bytes=10-20")), 10);
        // Forms MPD does not send are served whole.
        assert_eq!(range_start(&range("bytes=-500")), 0);
        assert_eq!(range_start(&range("items=5-")), 0);
        assert_eq!(range_start(&range("bytes=5-,9-")), 0);
    }

    fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
        headers.get(name).map(|v| v.to_str().unwrap())
    }

    #[test]
    fn known_length_streams_are_served_with_byte_ranges() {
        let (status, headers) = stream_framing(Some(1000), 0);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_text(&headers, header::CONTENT_LENGTH), Some("1000"));
        assert_eq!(header_text(&headers, header::ACCEPT_RANGES), Some("bytes"));
        assert!(headers.get(header::TRANSFER_ENCODING).is_none());

        let (status, headers) = stream_framing(Some(1000), 400);
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(header_text(&headers, header::CONTENT_LENGTH), Some("600"));
        assert_eq!(header_text(&headers, header::CONTENT_RANGE), Some("bytes 400-999/1000"));
    }

    #[test]
    fn sabr_streams_stay_chunked() {
        let (status, headers) = stream_framing(None, 0);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header_text(&headers, header::TRANSFER_ENCODING), Some("chunked"));
        assert!(headers.get(header::CONTENT_LENGTH).is_none());
        assert!(headers.get(header::ACCEPT_RANGES).is_none());
    }

    fn fake_info(content_length: Option<u64>, start_offset: u64) -> crate::sabr::stream::SabrStreamInfo {
        crate::sabr::stream::SabrStreamInfo {
            title: "Song".to_owned(),
            duration_secs: 200,
            mime_type: "audio/mp4".to_owned(),
            content_length,
            start_offset,
        }
    }

    #[tokio::test]
    async fn range_request_reaches_the_stream_and_is_answered_206() {
        let state = test_state();
        let seen = std::sync::Mutex::new(None);
        let resp = serve_stream(&state, "dQw4w9WgXcQ", &range("bytes=400-"), |video_id, _, _, start| {
            *seen.lock().unwrap() = Some((video_id, start));
            async move {
                let (tx, rx) = mpsc::channel(1);
                tx.send(bytes::Bytes::from(vec![7u8; 600])).await.unwrap();
                Ok((fake_info(Some(1000), start), rx))
            }
        })
        .await;

        assert_eq!(seen.lock().unwrap().clone(), Some(("dQw4w9WgXcQ".to_owned(), 400)));
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(header_text(resp.headers(), header::CONTENT_RANGE), Some("bytes 400-999/1000"));
        assert_eq!(header_text(resp.headers(), header::CONTENT_LENGTH), Some("600"));
        assert_eq!(header_text(resp.headers(), header::CONTENT_TYPE), Some("audio/mp4"));
        let body = axum::body::to_bytes(resp.into_body(), 10_000).await.unwrap();
        assert_eq!(body.len(), 600);
    }

    #[tokio::test]
    async fn range_past_the_end_is_416_and_no_song_failure() {
        let state = test_state();
        let resp = serve_stream(&state, "dQw4w9WgXcQ", &range("bytes=1000-"), |_, _, _, _| async {
            let err: crate::sabr::stream::SabrError =
                Box::new(crate::sabr::stream::RangeNotSatisfiable { content_length: 1000 });
            Err(err)
        })
        .await;

        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header_text(resp.headers(), header::CONTENT_RANGE), Some("bytes */1000"));
        assert!(state.stream_failure.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_seek_is_500_but_no_song_failure() {
        let state = test_state();
        let resp = serve_stream(&state, "dQw4w9WgXcQ", &range("bytes=400-"), |_, _, _, _| async {
            Err::<(crate::sabr::stream::SabrStreamInfo, mpsc::Receiver<bytes::Bytes>), _>("no media via url".into())
        })
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.stream_failure.lock().unwrap().is_none());

        // The same failure at byte 0 is the song failing.
        let resp = serve_stream(&state, "dQw4w9WgXcQ", &HeaderMap::new(), |_, _, _, _| async {
            Err::<(crate::sabr::stream::SabrStreamInfo, mpsc::Receiver<bytes::Bytes>), _>("no media via url".into())
        })
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.stream_failure.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn whole_song_request_starts_at_zero_and_is_200() {
        let state = test_state();
        let resp = serve_stream(&state, "dQw4w9WgXcQ", &HeaderMap::new(), |_, _, _, start| async move {
            assert_eq!(start, 0);
            let (_tx, rx) = mpsc::channel(1);
            Ok((fake_info(Some(1000), 0), rx))
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header_text(resp.headers(), header::CONTENT_LENGTH), Some("1000"));
    }

    #[tokio::test]
    async fn rejected_lan_peers_are_not_logged_as_warnings() {
        // A LAN peer probing /silence or /stream must not flood the log.
        let lan = peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)));
        let capture = crate::storage::test_util::LogCapture::start();
        let silence = silence_audio(ConnectInfo(lan), State(test_state())).await;
        let stream = stream_audio(
            axum::extract::Path("dQw4w9WgXcQ".to_string()),
            ConnectInfo(lan),
            State(test_state()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(silence.status(), StatusCode::FORBIDDEN);
        assert_eq!(stream.status(), StatusCode::FORBIDDEN);
        let logs = capture.text();
        assert!(logs.contains("rejected non-loopback peer"), "log capture is not working: {logs:?}");
        assert!(!logs.contains("WARN"), "rejections logged as warnings: {logs}");
    }

    #[tokio::test]
    async fn silence_serves_at_most_two_streams_at_once() {
        let state = test_state();
        let local = || ConnectInfo(peer(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let first = silence_audio(local(), State(state.clone())).await;
        let second = silence_audio(local(), State(state.clone())).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            silence_audio(local(), State(state.clone())).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // A finished stream frees its slot.
        drop(first);
        assert_eq!(silence_audio(local(), State(state)).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn repeated_launches_queue_at_most_one_preroll_request() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = test_state_with(tx);
        for _ in 0..3 {
            assert_eq!(launch(state.clone(), "m").await, StatusCode::CREATED);
        }
        assert_eq!(rx.try_recv(), Ok(()));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn launch_succeeds_when_nobody_takes_preroll_requests() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        assert_eq!(launch(test_state_with(tx), "m").await, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn dial_stays_reachable_from_lan() {
        let lan = peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)));
        assert_eq!(get_as(lan, "/ssdp/device-desc.xml").await, StatusCode::OK);
        assert_eq!(get_as(lan, "/apps/YouTube").await, StatusCode::OK);
    }

    #[test]
    fn stream_failure_is_taken_once_for_its_video() {
        let state = test_state();
        state.note_stream_result("aaaaaaaaaaa", Err("nsig decode failed".into()));
        assert_eq!(state.take_stream_failure("aaaaaaaaaaa").as_deref(), Some("nsig decode failed"));
        assert_eq!(state.take_stream_failure("aaaaaaaaaaa"), None);
    }

    #[test]
    fn stream_failure_of_another_video_is_dropped() {
        let state = test_state();
        state.note_stream_result("aaaaaaaaaaa", Err("boom".into()));
        assert_eq!(state.take_stream_failure("bbbbbbbbbbb"), None);
        // A stale failure must not mark a later stop of its video as failed.
        assert_eq!(state.take_stream_failure("aaaaaaaaaaa"), None);
    }

    #[test]
    fn successful_stream_clears_earlier_failure() {
        let state = test_state();
        state.note_stream_result("aaaaaaaaaaa", Err("boom".into()));
        state.note_stream_result("aaaaaaaaaaa", Ok(()));
        assert_eq!(state.take_stream_failure("aaaaaaaaaaa"), None);
    }
}
