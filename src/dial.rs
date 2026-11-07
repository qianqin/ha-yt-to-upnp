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
    /// Human-readable name shown during discovery (e.g. "Living Room Pi").
    pub device_name: String,
    /// UPnP UUID for this device (without the "uuid:" prefix).
    pub uuid: String,
    /// Local IP for building absolute Application-URL.
    pub local_ip: std::net::Ipv4Addr,
    /// Port this DIAL server listens on.
    pub port: u16,
    /// Shared HTTP client for outbound requests (e.g. SABR streaming).
    pub http_client: reqwest::Client,
    /// Credential transfer token from the casting session (YTM Premium auth).
    pub ctt: std::sync::RwLock<Option<String>>,
    /// YouTube cookies for Premium auth (only sent with WEB_REMIX /player calls).
    pub yt_cookies: Option<std::sync::Arc<crate::cookies::YtCookies>>,
    /// Live YouTube Music session (full INNERTUBE_CONTEXT from page).
    pub yt_session: Option<std::sync::Arc<crate::cookies::YtMusicSession>>,
    /// OAuth state for Premium Bearer token auth.
    pub oauth_state: Option<std::sync::Arc<crate::oauth::OAuthState>>,
    /// Screen ID for the "cl" (YouTube) Lounge session, exposed in DIAL additionalData.
    pub screen_id: String,
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
        .with_state(state)
}

/// `/stream` is only meant for the local MPD; the server binds 0.0.0.0 for
/// DIAL, so the peer address is the only thing keeping LAN clients out.
fn is_loopback_peer(peer: SocketAddr) -> bool {
    // to_canonical() folds IPv4-mapped IPv6 (::ffff:127.x) back to IPv4.
    peer.ip().to_canonical().is_loopback()
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
        }
    }

    state.running.store(true, Ordering::Relaxed);

    // DIAL spec: 201 Created with LOCATION pointing to the running instance.
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, "/apps/YouTube/run".parse().unwrap());

    (StatusCode::CREATED, headers).into_response()
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
// GET /stream/:video_id -- SABR audio stream
// ---------------------------------------------------------------------------

async fn stream_audio(
    axum::extract::Path(video_id): axum::extract::Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<DialState>>,
) -> Response {
    if !is_loopback_peer(peer) {
        tracing::warn!("[stream] rejected non-loopback peer {}", peer);
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

    let ctt = state.ctt.read().unwrap().clone();
    let yt_cookies = state.yt_cookies.as_ref().map(|c| c.as_ref());
    let yt_session = state.yt_session.as_ref().map(|s| s.as_ref());
    let oauth = state.oauth_state.clone();
    match crate::sabr::stream::stream_audio(&state.http_client, &video_id, ctt.as_deref(), yt_cookies, yt_session, oauth.as_ref()).await {
        Ok((info, rx)) => {
            // Build streaming response from the mpsc receiver.
            // When YTCAST_DUMP_STREAM=1, also write every byte to /tmp/sabr-dump-{id}.webm
            // so we can inspect with ffprobe if MPD crashes.
            let dump_file = if std::env::var("YTCAST_DUMP_STREAM").is_ok() {
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

            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                info.mime_type
                    .parse()
                    .unwrap_or_else(|_| "audio/webm".parse().unwrap()),
            );
            headers.insert(
                header::TRANSFER_ENCODING,
                "chunked".parse().unwrap(),
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

            (StatusCode::OK, headers, body).into_response()
        }
        Err(e) => {
            tracing::error!("[stream] {} failed: {}", video_id, e);
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
        let (pairing_tx, _) = mpsc::channel(1);
        let (pairing_tx_m, _) = mpsc::channel(1);
        Arc::new(DialState {
            running: AtomicBool::new(false),
            pairing_tx,
            pairing_tx_m,
            device_name: "Test Receiver".to_string(),
            uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            local_ip: Ipv4Addr::new(192, 0, 2, 1),
            port: 8008,
            http_client: reqwest::Client::new(),
            ctt: std::sync::RwLock::new(None),
            yt_cookies: None,
            yt_session: None,
            oauth_state: None,
            screen_id: "test-screen".to_string(),
        })
    }

    /// Serve the real router on an ephemeral loopback port while pretending the
    /// TCP peer is `peer_addr`, then issue a GET for `path`.
    async fn get_as(peer_addr: SocketAddr, path: &str) -> StatusCode {
        let app = build_router(test_state()).layer(MockConnectInfo(peer_addr));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service()).await.unwrap();
        });
        let status = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap()
            .status();
        server.abort();
        StatusCode::from_u16(status.as_u16()).unwrap()
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

    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn dial_launch_does_not_log_pairing_code() {
        const CODE: &str = "SECRETPAIRCODE123";
        let buf = LogBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        // Current-thread test runtime: the handler runs on this thread, so a
        // thread-local default subscriber sees all of its events.
        let _guard = tracing::subscriber::set_default(subscriber);

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
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("DIAL"), "log capture is not working: {logs:?}");
        assert!(!logs.contains(CODE), "pairing code leaked into logs: {logs}");
    }

    #[tokio::test]
    async fn dial_stays_reachable_from_lan() {
        let lan = peer(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)));
        assert_eq!(get_as(lan, "/ssdp/device-desc.xml").await, StatusCode::OK);
        assert_eq!(get_as(lan, "/apps/YouTube").await, StatusCode::OK);
    }
}
