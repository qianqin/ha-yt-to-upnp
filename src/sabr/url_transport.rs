//! Premium audio transport: fetch one audio format through its own
//! googlevideo URL in byte ranges, as the music.youtube.com web player does
//! for Premium itag 141, instead of through a SABR session.
//!
//! The format URL comes from the cast's /player response (`url`, or
//! `signatureCipher` = url + `s` + `sp`). The signature and the `n` parameter
//! are solved with the same base.js solver as SABR's nsig. URLs carry
//! signatures and tokens, so logs only ever show the host and param names.

use std::time::Instant;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::stream::{host_for_log, is_trusted_google_host, SabrError};
use super::ump::{self, UmpParser};

/// Solves base.js challenges for a format URL.
pub(crate) trait Decipher: Send + Sync {
    fn n(&self, value: &str) -> Result<String, String>;
    fn sig(&self, value: &str) -> Result<String, String>;
}

/// The real solver, on the anonymous page session's base.js.
pub(crate) struct PlayerJsDecipher {
    pub session: std::sync::Arc<crate::music_session::YtMusicSession>,
}

impl PlayerJsDecipher {
    fn player(&self) -> Result<(&str, String), String> {
        let js = self.session.player_js.as_deref().ok_or("no base.js in the YouTube Music session")?;
        Ok((js, self.session.player_hash.clone().unwrap_or_default()))
    }
}

impl Decipher for PlayerJsDecipher {
    fn n(&self, value: &str) -> Result<String, String> {
        let (js, hash) = self.player()?;
        crate::nsig::decode_nsig(js, value, &hash)
    }

    fn sig(&self, value: &str) -> Result<String, String> {
        let (js, hash) = self.player()?;
        crate::nsig::decode_sig(js, value, &hash)
    }
}

/// Where a format's media URL comes from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FormatSource {
    url: String,
    /// `(sp, s)` from a `signatureCipher`: the deciphered `s` goes into `sp`.
    signature: Option<(String, String)>,
}

pub(crate) fn format_source(
    url: Option<&str>,
    signature_cipher: Option<&str>,
) -> Result<FormatSource, String> {
    if let Some(url) = url {
        return Ok(FormatSource { url: url.to_owned(), signature: None });
    }
    let cipher = signature_cipher.ok_or("format has neither url nor signatureCipher")?;
    let fields: Vec<(String, String)> =
        serde_urlencoded::from_str(cipher).map_err(|_| "unparseable signatureCipher")?;
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let url = field("url").ok_or("signatureCipher without url")?;
    let s = field("s").ok_or("signatureCipher without s")?;
    let sp = field("sp").unwrap_or_else(|| "signature".to_owned());
    Ok(FormatSource { url, signature: Some((sp, s)) })
}

/// Query param names of a URL (never values), for logs.
pub(crate) fn param_names(url: &str) -> Vec<String> {
    reqwest::Url::parse(url)
        .map(|u| u.query_pairs().map(|(k, _)| k.into_owned()).collect())
        .unwrap_or_default()
}

/// Which of `url` / `signatureCipher` a format has, with field and URL
/// param NAMES only, e.g. `signatureCipher (fields s, sp, url; url params expire, itag, n)`.
pub(crate) fn describe_source(url: Option<&str>, signature_cipher: Option<&str>) -> String {
    if let Some(url) = url {
        return format!("url (host {}; params {})", host_for_log(url), param_names(url).join(", "));
    }
    let Some(cipher) = signature_cipher else {
        return "neither url nor signatureCipher".to_owned();
    };
    let fields: Vec<(String, String)> = serde_urlencoded::from_str(cipher).unwrap_or_default();
    let names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    let url_params = fields
        .iter()
        .find(|(k, _)| k == "url")
        .map(|(_, u)| param_names(u).join(", "))
        .unwrap_or_default();
    format!("signatureCipher (fields {}; url params {})", names.join(", "), url_params)
}

/// The fetchable URL: signature deciphered into `sp`, `n` solved, plus the
/// GVS PoToken, a client playback nonce and the client version (as the web
/// player sends them). Fails closed on a non-Google host.
pub(crate) fn resolve_url(
    source: &FormatSource,
    decipher: &dyn Decipher,
    po_token: Option<&str>,
    cver: &str,
    cpn: &str,
) -> Result<String, SabrError> {
    if !is_trusted_google_host(&source.url) {
        return Err(format!("untrusted format host {}", host_for_log(&source.url)).into());
    }
    let mut url = reqwest::Url::parse(&source.url).map_err(|_| "unparseable format url")?;
    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    for (key, value) in pairs.iter_mut() {
        if key == "n" {
            *value = decipher.n(value).map_err(|e| format!("n decipher: {e}"))?;
        }
    }
    if let Some((sp, s)) = &source.signature {
        let sig = decipher.sig(s).map_err(|e| format!("sig decipher: {e}"))?;
        pairs.push((sp.clone(), sig));
    }
    if let Some(pot) = po_token {
        pairs.push(("pot".to_owned(), pot.to_owned()));
    }
    pairs.push(("cpn".to_owned(), cpn.to_owned()));
    pairs.push(("cver".to_owned(), cver.to_owned()));
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url.into())
}

/// A random 16-character client playback nonce, like the web player's.
pub(crate) fn generate_cpn() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = rand::thread_rng();
    (0..16).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

/// How ranges are requested: classic GET returning raw bytes, or the web
/// player's POST with `ump=1` and a UMP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeMode {
    Get,
    UmpPost,
}

/// The web player's 2-byte request body (protobuf field 15 = 0).
const UMP_POST_BODY: [u8; 2] = [0x78, 0x00];

/// Extra attempts per range after the first bytes have arrived.
const RANGE_RETRIES: u32 = 2;

/// Channel slots between the download and the consumer. Each slot holds one
/// body chunk as it arrives (typically 8-64 KiB), so memory stays bounded and
/// a slow consumer slows the download (backpressure) instead of buffering it.
const CHANNEL_CHUNKS: usize = 8;

pub(crate) type RefreshFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, SabrError>> + Send + 'a>>;

/// Provides a fresh, deciphered URL when the current one is refused
/// mid-song (e.g. expired).
pub(crate) trait UrlRefresher: Send {
    fn refresh(&mut self) -> RefreshFuture<'_>;
}

/// One format, fetched as sequential byte ranges from `start_offset`.
pub(crate) struct RangeFetch {
    pub http: reqwest::Client,
    pub url: String,
    pub content_length: Option<u64>,
    pub chunk_size: u64,
    pub itag: i32,
    pub start_offset: u64,
    pub refresher: Box<dyn UrlRefresher>,
}

fn range_url(url: &str, start: u64, end: u64, rn: u32, mode: RangeMode) -> String {
    let mut out = format!("{url}&range={start}-{end}&rn={rn}");
    if mode == RangeMode::UmpPost {
        out.push_str("&ump=1&srfvp=1");
    }
    out
}

/// Why a range request failed. `Refused` (HTTP 401/403/410) may mean the URL
/// expired; it never carries the URL.
#[derive(Debug)]
enum RangeError {
    Refused(u16),
    Other(String),
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeError::Refused(status) => write!(f, "HTTP {status}"),
            RangeError::Other(e) => f.write_str(e),
        }
    }
}

/// An open range response, read chunk by chunk as it arrives.
struct RangeBody {
    stream: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
    /// `Some` for a UMP response: media is taken from its MEDIA parts.
    ump: Option<UmpParser>,
}

impl RangeBody {
    /// The next non-empty piece of media, `None` at the end of the range.
    async fn next_media(&mut self) -> Option<Result<Bytes, RangeError>> {
        loop {
            let chunk = match self.stream.next().await? {
                Ok(chunk) => chunk,
                Err(e) => return Some(Err(RangeError::Other(e.without_url().to_string()))),
            };
            let media = match &mut self.ump {
                None => chunk,
                Some(parser) => {
                    parser.push(&chunk);
                    let mut media = Vec::new();
                    while let Some(part) = parser.next_part() {
                        // MEDIA: one-byte header id, then payload.
                        if part.part_type == ump::MEDIA && part.data.len() > 1 {
                            media.extend_from_slice(&part.data[1..]);
                        }
                    }
                    Bytes::from(media)
                }
            };
            if !media.is_empty() {
                return Some(Ok(media));
            }
        }
    }
}

async fn open_range(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    rn: u32,
    mode: RangeMode,
) -> Result<RangeBody, RangeError> {
    let target = range_url(url, start, end, rn, mode);
    let req = match mode {
        RangeMode::Get => http.get(&target),
        RangeMode::UmpPost => http
            .post(&target)
            .header("Content-Type", "application/x-protobuf")
            .body(UMP_POST_BODY.to_vec()),
    };
    let resp = req.send().await.map_err(|e| RangeError::Other(e.without_url().to_string()))?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403 | 410) {
        return Err(RangeError::Refused(status.as_u16()));
    }
    if !status.is_success() {
        return Err(RangeError::Other(format!("HTTP {}", status.as_u16())));
    }
    let is_ump = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/vnd.yt-ump"));
    Ok(RangeBody {
        stream: resp.bytes_stream().boxed(),
        ump: is_ump.then(UmpParser::new),
    })
}

/// How the download ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// Content length reached, or a short range without one.
    Complete,
    /// The consumer went away (song skipped or stopped).
    Cancelled,
    Failed(String),
}

impl RangeFetch {
    fn range_end(&self, start: u64) -> u64 {
        let end = start + self.chunk_size - 1;
        match self.content_length {
            Some(len) => end.min(len.saturating_sub(1)),
            None => end,
        }
    }

    fn complete_at(&self, offset: u64) -> bool {
        self.content_length.is_some_and(|len| offset >= len)
    }

    /// Open the first range (GET, then the UMP POST form) and read its first
    /// media bytes; the rest streams from a background task. `Err` means no
    /// byte arrived, so the caller can still try another format.
    pub(crate) async fn start(self) -> Result<mpsc::Receiver<Bytes>, SabrError> {
        if self.complete_at(self.start_offset) {
            return Err(format!("start offset {} is past the end", self.start_offset).into());
        }
        let started = Instant::now();
        let end = self.range_end(self.start_offset);
        let mut failures = Vec::new();
        let mut opened = None;
        for mode in [RangeMode::Get, RangeMode::UmpPost] {
            let first = match open_range(&self.http, &self.url, self.start_offset, end, 1, mode).await {
                Ok(mut body) => match body.next_media().await {
                    Some(Ok(bytes)) => Ok((body, bytes)),
                    Some(Err(e)) => Err(e.to_string()),
                    None => Err("no media".to_owned()),
                },
                Err(e) => Err(e.to_string()),
            };
            match first {
                Ok((body, bytes)) => {
                    opened = Some((mode, body, bytes));
                    break;
                }
                Err(e) => {
                    tracing::info!("[url] itag {} {:?} range {}-{}: {}", self.itag, mode, self.start_offset, end, e);
                    failures.push(format!("{mode:?}: {e}"));
                }
            }
        }
        let Some((mode, body, first)) = opened else {
            return Err(format!("no media via url ({})", failures.join(", ")).into());
        };
        tracing::info!(
            "[url] itag {} first bytes via {:?} from {} after {} ms (offset {}, clen {:?})",
            self.itag, mode, host_for_log(&self.url), started.elapsed().as_millis(),
            self.start_offset, self.content_length,
        );

        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        tokio::spawn(async move {
            let mut fetch = self;
            let (ending, sent) = fetch.pump(mode, body, first, &tx).await;
            match ending {
                Ending::Complete => tracing::info!("[url] itag {} complete: {} bytes", fetch.itag, sent),
                Ending::Cancelled => tracing::info!("[url] itag {} stopped by the player after {} bytes", fetch.itag, sent),
                Ending::Failed(e) => tracing::warn!("[url] itag {} failed after {} bytes: {}", fetch.itag, sent, e),
            }
        });
        Ok(rx)
    }

    /// Forward `first` and the rest of `body`, then the following ranges,
    /// until the content length (or, without one, a short range). A refused
    /// range gets one fresh URL per song; other failures resume from the
    /// current offset up to `RANGE_RETRIES` times. Returns how it ended and
    /// the bytes forwarded.
    async fn pump(
        &mut self,
        mode: RangeMode,
        mut body: RangeBody,
        first: Bytes,
        tx: &mpsc::Sender<Bytes>,
    ) -> (Ending, u64) {
        let mut offset = self.start_offset;
        let mut range_start = offset;
        let mut range_end = self.range_end(offset);
        let mut rn = 1u32;
        let mut refreshed = false;
        let mut broken_ranges = 0u32;
        let mut pending = Some(first);
        loop {
            // Drain the open range.
            let mut failure = None;
            loop {
                let next = match pending.take() {
                    Some(bytes) => Some(Ok(bytes)),
                    None => body.next_media().await,
                };
                match next {
                    Some(Ok(bytes)) => {
                        offset += bytes.len() as u64;
                        if tx.send(bytes).await.is_err() {
                            return (Ending::Cancelled, offset - self.start_offset);
                        }
                    }
                    Some(Err(e)) => {
                        failure = Some(e);
                        break;
                    }
                    None => break,
                }
            }
            let sent = offset - self.start_offset;
            if self.complete_at(offset) {
                return (Ending::Complete, sent);
            }
            if failure.is_none() && offset <= range_end {
                // A short range: the end of a format without a known length.
                if self.content_length.is_none() && offset > range_start {
                    return (Ending::Complete, sent);
                }
                failure = Some(RangeError::Other(format!("range ended at {offset}")));
            }
            if let Some(e) = failure {
                broken_ranges += 1;
                if broken_ranges > RANGE_RETRIES {
                    return (Ending::Failed(format!("range {range_start}-{range_end} broke off at {offset}: {e}")), sent);
                }
                tracing::warn!("[url] range {}-{} broke off at {}: {}, resuming", range_start, range_end, offset, e);
            }

            // Open the next range (or the rest of a broken one).
            range_start = offset;
            range_end = self.range_end(offset);
            let mut attempts = 0;
            body = loop {
                if tx.is_closed() {
                    return (Ending::Cancelled, sent);
                }
                rn += 1;
                match open_range(&self.http, &self.url, range_start, range_end, rn, mode).await {
                    Ok(body) => break body,
                    Err(RangeError::Refused(status)) if !refreshed => {
                        refreshed = true;
                        tracing::warn!("[url] range {}-{}: HTTP {}, refreshing the URL", range_start, range_end, status);
                        match self.refresher.refresh().await {
                            Ok(url) => self.url = url,
                            Err(e) => return (Ending::Failed(format!("URL refresh failed: {e}")), sent),
                        }
                    }
                    Err(e) if attempts < RANGE_RETRIES => {
                        attempts += 1;
                        tracing::warn!("[url] range {}-{}: {}, retry {}/{}", range_start, range_end, e, attempts, RANGE_RETRIES);
                        tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempts))).await;
                    }
                    Err(e) => return (Ending::Failed(format!("range {range_start}-{range_end}: {e}")), sent),
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Reverses `n` and upper-cases the signature, like a (fake) base.js.
    struct FakeDecipher;

    impl Decipher for FakeDecipher {
        fn n(&self, value: &str) -> Result<String, String> {
            Ok(value.chars().rev().collect())
        }
        fn sig(&self, value: &str) -> Result<String, String> {
            Ok(value.to_uppercase())
        }
    }

    const FORMAT_URL: &str = "https://rr1---sn-abc.googlevideo.com/videoplayback?expire=1&itag=141&n=abcd&sparams=expire";

    fn query(url: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    }

    fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn signature_cipher_is_split_into_url_s_and_sp() {
        let cipher = serde_urlencoded::to_string([("s", "secret-s"), ("sp", "sig"), ("url", FORMAT_URL)]).unwrap();

        let source = format_source(None, Some(&cipher)).unwrap();

        assert_eq!(source.url, FORMAT_URL);
        assert_eq!(source.signature, Some(("sig".to_owned(), "secret-s".to_owned())));
        assert_eq!(format_source(Some(FORMAT_URL), None).unwrap().signature, None);
        assert!(format_source(None, None).is_err());
    }

    #[test]
    fn source_description_has_names_but_no_values() {
        let cipher = serde_urlencoded::to_string([("s", "secret-s"), ("sp", "sig"), ("url", FORMAT_URL)]).unwrap();

        let text = describe_source(None, Some(&cipher));

        assert_eq!(text, "signatureCipher (fields s, sp, url; url params expire, itag, n, sparams)");
        let plain = describe_source(Some(FORMAT_URL), None);
        assert_eq!(plain, "url (host rr1---sn-abc.googlevideo.com; params expire, itag, n, sparams)");
        assert_eq!(describe_source(None, None), "neither url nor signatureCipher");
    }

    #[test]
    fn resolved_url_has_signature_in_sp_solved_n_pot_cpn_and_cver() {
        let cipher = serde_urlencoded::to_string([("s", "secret-s"), ("sp", "sig"), ("url", FORMAT_URL)]).unwrap();
        let source = format_source(None, Some(&cipher)).unwrap();

        let url = resolve_url(&source, &FakeDecipher, Some("pot-token"), "1.2026", "CPN").unwrap();

        let pairs = query(&url);
        assert_eq!(param(&pairs, "sig"), Some("SECRET-S"));
        assert_eq!(param(&pairs, "n"), Some("dcba"));
        assert_eq!(param(&pairs, "pot"), Some("pot-token"));
        assert_eq!(param(&pairs, "cpn"), Some("CPN"));
        assert_eq!(param(&pairs, "cver"), Some("1.2026"));
        assert_eq!(param(&pairs, "itag"), Some("141"));
        assert!(!url.contains("secret-s"), "{url}");
    }

    #[test]
    fn plain_url_without_pot_gets_n_solved_only() {
        let source = format_source(Some(FORMAT_URL), None).unwrap();
        let url = resolve_url(&source, &FakeDecipher, None, "v", "c").unwrap();
        let pairs = query(&url);
        assert_eq!(param(&pairs, "n"), Some("dcba"));
        assert_eq!(param(&pairs, "pot"), None);
        assert_eq!(param(&pairs, "sig"), None);
    }

    #[test]
    fn untrusted_format_host_is_refused() {
        let source = format_source(Some("https://evil.example/videoplayback?sig=secret-q"), None).unwrap();
        let err = resolve_url(&source, &FakeDecipher, Some("pot"), "v", "c").unwrap_err();
        assert!(err.to_string().contains("evil.example"), "{err}");
        assert!(!err.to_string().contains("secret-q"), "{err}");
    }

    #[test]
    fn decipher_failure_is_an_error() {
        struct Broken;
        impl Decipher for Broken {
            fn n(&self, _: &str) -> Result<String, String> {
                Err("no n function".to_owned())
            }
            fn sig(&self, _: &str) -> Result<String, String> {
                Err("no sig function".to_owned())
            }
        }
        let source = format_source(Some(FORMAT_URL), None).unwrap();
        let err = resolve_url(&source, &Broken, None, "v", "c").unwrap_err();
        assert!(err.to_string().contains("n decipher"), "{err}");
    }

    // -- fake googlevideo ---------------------------------------------------------

    #[derive(Clone, Copy)]
    struct Behaviour {
        get_status: u16,
        post_status: u16,
        /// From this request on (0-based), URLs without `fresh=1` get 403:
        /// the original URL "expires".
        expire_from: Option<usize>,
    }

    /// Serves `content` by `range=a-b` query param: GET as raw bytes, POST
    /// (`ump=1`) as UMP MEDIA parts. Records each request line.
    fn spawn_videoplayback(
        listener: tokio::net::TcpListener,
        content: Vec<u8>,
        behaviour: Behaviour,
    ) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let mut req = Vec::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..pos]).to_ascii_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if req.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let line = text.lines().next().unwrap_or("").to_owned();
                record.lock().unwrap().push(line.clone());
                let is_post = line.starts_with("POST");
                let index = record.lock().unwrap().len() - 1;
                let expired = behaviour.expire_from.is_some_and(|from| index >= from) && !line.contains("fresh=1");
                let status = if expired {
                    403
                } else if is_post {
                    behaviour.post_status
                } else {
                    behaviour.get_status
                };
                let path = line.split(' ').nth(1).unwrap_or("");
                let pairs: Vec<(String, String)> =
                    serde_urlencoded::from_str(path.split_once('?').map_or("", |(_, q)| q)).unwrap_or_default();
                let range = pairs.iter().find(|(k, _)| k == "range").map(|(_, v)| v.clone()).unwrap_or_default();
                let (a, b) = range.split_once('-').unwrap_or(("0", "0"));
                let (a, b): (usize, usize) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0));
                let slice = content.get(a..=b.min(content.len().saturating_sub(1))).unwrap_or(&[]).to_vec();
                let (ctype, body) = if is_post {
                    let mut out = Vec::new();
                    ump::write_varint(&mut out, ump::MEDIA_HEADER);
                    ump::write_varint(&mut out, 0);
                    ump::write_varint(&mut out, ump::MEDIA);
                    ump::write_varint(&mut out, slice.len() as u32 + 1);
                    out.push(0);
                    out.extend_from_slice(&slice);
                    ("application/vnd.yt-ump", out)
                } else {
                    ("audio/mp4", slice)
                };
                let body = if status == 200 { body } else { Vec::new() };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        seen
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Hands out the same URL with `fresh=1`, counting calls.
    struct FreshUrl {
        url: String,
        calls: Arc<Mutex<u32>>,
    }

    impl UrlRefresher for FreshUrl {
        fn refresh(&mut self) -> RefreshFuture<'_> {
            *self.calls.lock().unwrap() += 1;
            let url = format!("{}&fresh=1", self.url);
            Box::pin(async move { Ok(url) })
        }
    }

    async fn fetcher(behaviour: Behaviour, data: &[u8], clen: Option<u64>) -> (RangeFetch, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = spawn_videoplayback(listener, data.to_vec(), behaviour);
        let url = format!("http://{addr}/videoplayback?itag=141&sig=SECRET-SIG");
        let fetch = RangeFetch {
            http: reqwest::Client::new(),
            url: url.clone(),
            content_length: clen,
            chunk_size: 1000,
            itag: 141,
            start_offset: 0,
            refresher: Box::new(FreshUrl { url, calls: Arc::default() }),
        };
        (fetch, seen)
    }

    async fn collect(mut rx: mpsc::Receiver<Bytes>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.unwrap() {
            out.extend_from_slice(&chunk);
        }
        out
    }

    fn ranges(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|line| {
                let method = line.split(' ').next().unwrap().to_owned();
                let range = line.split("range=").nth(1).unwrap().split('&').next().unwrap().to_owned();
                format!("{method} {range}")
            })
            .collect()
    }

    const OK: Behaviour = Behaviour { get_status: 200, post_status: 200, expire_from: None };

    #[tokio::test]
    async fn get_ranges_stream_the_whole_format_in_order() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(2500);
        let (fetch, seen) = fetcher(OK, &data, Some(2500)).await;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999", "GET 2000-2499"]);
        let logs = logs.text();
        assert!(logs.contains("first bytes via Get"), "{logs}");
        assert!(!logs.contains("SECRET-SIG"), "{logs}");
    }

    #[tokio::test]
    async fn expired_url_mid_song_is_refreshed_once_and_resumes_at_the_offset() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(3500);
        let (mut fetch, seen) =
            fetcher(Behaviour { expire_from: Some(2), ..OK }, &data, Some(3500)).await;
        let calls: Arc<Mutex<u32>> = Arc::default();
        fetch.refresher = Box::new(FreshUrl { url: fetch.url.clone(), calls: calls.clone() });

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999", "GET 2000-2999", "GET 2000-2999", "GET 3000-3499"]);
        let lines = seen.lock().unwrap().clone();
        assert!(lines[3].contains("fresh=1") && lines[4].contains("fresh=1"), "{lines:?}");
        let logs = logs.text();
        assert!(!logs.contains("SECRET-SIG"), "{logs}");
    }

    /// Refreshes to the same URL, so the refusal persists.
    struct SameUrl {
        url: String,
        calls: Arc<Mutex<u32>>,
    }

    impl UrlRefresher for SameUrl {
        fn refresh(&mut self) -> RefreshFuture<'_> {
            *self.calls.lock().unwrap() += 1;
            let url = self.url.clone();
            Box::pin(async move { Ok(url) })
        }
    }

    #[tokio::test]
    async fn the_url_is_refreshed_only_once_per_song() {
        let data = content(5000);
        let (mut fetch, _seen) = fetcher(Behaviour { expire_from: Some(1), ..OK }, &data, Some(5000)).await;
        let calls: Arc<Mutex<u32>> = Arc::default();
        fetch.refresher = Box::new(SameUrl { url: fetch.url.clone(), calls: calls.clone() });

        let rx = fetch.start().await.unwrap();

        // The first range arrived; everything after it stays refused.
        assert_eq!(collect(rx).await, data[..1000]);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn dropping_the_receiver_stops_the_download() {
        let data = content(200_000);
        let (fetch, seen) = fetcher(OK, &data, Some(200_000)).await;

        let mut rx = fetch.start().await.unwrap();
        rx.recv().await.unwrap();
        drop(rx);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let requests = seen.lock().unwrap().len();
        assert!(requests < 20, "kept downloading after the player went away: {requests} of 200 ranges");
    }

    #[tokio::test]
    async fn a_start_offset_begins_ranges_there() {
        let data = content(3000);
        let (mut fetch, seen) = fetcher(OK, &data, Some(3000)).await;
        fetch.start_offset = 1500;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data[1500..]);
        assert_eq!(ranges(&seen), ["GET 1500-2499", "GET 2500-2999"]);
    }

    #[tokio::test]
    async fn a_start_offset_past_the_end_is_refused() {
        let data = content(1000);
        let (mut fetch, seen) = fetcher(OK, &data, Some(1000)).await;
        fetch.start_offset = 1000;

        assert!(fetch.start().await.is_err());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_content_length_a_short_range_ends_the_stream() {
        let data = content(1500);
        let (fetch, seen) = fetcher(OK, &data, None).await;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999"]);
    }

    #[tokio::test]
    async fn refused_get_falls_back_to_the_ump_post_form() {
        let data = content(1800);
        let (fetch, seen) = fetcher(Behaviour { get_status: 403, post_status: 200, expire_from: None }, &data, Some(1800)).await;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(ranges(&seen), ["GET 0-999", "POST 0-999", "POST 1000-1799"]);
        let lines = seen.lock().unwrap().clone();
        assert!(lines[1].contains("ump=1"), "{lines:?}");
    }

    #[tokio::test]
    async fn no_bytes_from_either_form_is_an_error_for_the_caller() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(1000);
        let (fetch, seen) = fetcher(Behaviour { get_status: 403, post_status: 403, expire_from: None }, &data, Some(1000)).await;

        let err = fetch.start().await.unwrap_err();

        assert!(err.to_string().contains("HTTP 403"), "{err}");
        assert_eq!(ranges(&seen), ["GET 0-999", "POST 0-999"]);
        for text in [err.to_string(), logs.text()] {
            assert!(!text.contains("SECRET-SIG"), "{text}");
        }
    }
}
