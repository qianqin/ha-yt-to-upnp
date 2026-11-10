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
    /// The URL's `n` and signature `s` (either may be absent), solved in one
    /// solver run; answers in the same positions.
    fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String>;
}

/// The solver entry point: [`crate::nsig::solve_cached_at`].
pub(crate) type SolverFn = fn(
    &str,
    &[(crate::nsig::Challenge, &str)],
    &str,
    Option<std::path::PathBuf>,
) -> Result<Vec<String>, String>;

/// The real solver, on the anonymous page session's base.js, with the
/// per-player-version cache (keyed by the player hash).
pub(crate) struct PlayerJsDecipher {
    pub session: std::sync::Arc<crate::music_session::YtMusicSession>,
    pub disk_cache: Option<std::path::PathBuf>,
    pub solver: SolverFn,
}

impl PlayerJsDecipher {
    pub(crate) fn new(session: std::sync::Arc<crate::music_session::YtMusicSession>) -> Self {
        let disk_cache = crate::nsig::cache_path(session.player_hash.as_deref().unwrap_or_default());
        Self { session, disk_cache, solver: crate::nsig::solve_cached_at }
    }
}

impl Decipher for PlayerJsDecipher {
    fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
        use crate::nsig::Challenge;
        let js = self.session.player_js.as_deref().ok_or("no base.js in the YouTube Music session")?;
        let hash = self.session.player_hash.as_deref().unwrap_or_default();
        let requests: Vec<(Challenge, &str)> = [(Challenge::N, n), (Challenge::Sig, sig)]
            .into_iter()
            .filter_map(|(c, v)| v.map(|v| (c, v)))
            .collect();
        if requests.is_empty() {
            return Ok((None, None));
        }
        let mut answers = (self.solver)(js, &requests, hash, self.disk_cache.clone())?.into_iter();
        let n = n.map(|_| answers.next().unwrap_or_default());
        let sig = sig.map(|_| answers.next().unwrap_or_default());
        Ok((n, sig))
    }
}

/// The format URL could not be deciphered. Another format would use the same
/// base.js, so the caller stops trying formats.
#[derive(Debug)]
pub(crate) struct DecipherError(String);

impl std::fmt::Display for DecipherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "decipher failed: {}", self.0)
    }
}

impl std::error::Error for DecipherError {}

#[cfg(test)]
pub(crate) mod tests_support {
    pub(crate) fn decipher_error(reason: &str) -> super::SabrError {
        Box::new(super::DecipherError(reason.to_owned()))
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
    let n = pairs.iter().find(|(k, _)| k == "n").map(|(_, v)| v.clone());
    let s = source.signature.as_ref().map(|(_, s)| s.as_str());
    // The solver's own errors are generic; nothing here quotes a value.
    let (n_solved, sig) = decipher.solve(n.as_deref(), s).map_err(DecipherError)?;
    if let Some(n_solved) = n_solved {
        for (_, value) in pairs.iter_mut().filter(|(k, _)| k == "n") {
            *value = n_solved.clone();
        }
    }
    if let (Some((sp, _)), Some(sig)) = (&source.signature, sig) {
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

/// Extra attempts per range after the first bytes have arrived; also the
/// number of times one range may break off and be resumed.
const RANGE_RETRIES: u32 = 2;

/// How long a range request may wait for its response, or for the next
/// body chunk. Waiting for the consumer (backpressure) is not limited.
pub(crate) const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

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
    /// Called at every new range while the song downloads.
    fn keep_alive(&self) {}
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
    pub read_timeout: std::time::Duration,
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
    async fn next_media(&mut self, read_timeout: std::time::Duration) -> Option<Result<Bytes, RangeError>> {
        loop {
            let chunk = match tokio::time::timeout(read_timeout, self.stream.next()).await {
                Err(_) => return Some(Err(RangeError::Other("read timed out".to_owned()))),
                Ok(None) => return None,
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(e))) => return Some(Err(RangeError::Other(e.without_url().to_string()))),
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
    read_timeout: std::time::Duration,
) -> Result<RangeBody, RangeError> {
    let target = range_url(url, start, end, rn, mode);
    let req = match mode {
        RangeMode::Get => http.get(&target),
        RangeMode::UmpPost => http
            .post(&target)
            .header("Content-Type", "application/x-protobuf")
            .body(UMP_POST_BODY.to_vec()),
    };
    let resp = tokio::time::timeout(read_timeout, req.send())
        .await
        .map_err(|_| RangeError::Other("no response (timed out)".to_owned()))?
        .map_err(|e| RangeError::Other(e.without_url().to_string()))?;
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
        let end = start.saturating_add(self.chunk_size - 1);
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
            let first = match open_range(&self.http, &self.url, self.start_offset, end, 1, mode, self.read_timeout).await {
                Ok(mut body) => match body.next_media(self.read_timeout).await {
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
                    // A consumer that goes away stops the download at once,
                    // even while the server is slow to send.
                    None => tokio::select! {
                        biased;
                        _ = tx.closed() => return (Ending::Cancelled, offset - self.start_offset),
                        next = body.next_media(self.read_timeout) => next,
                    },
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
            if failure.is_none() {
                broken_ranges = 0;
            }
            if let Some(e) = failure {
                broken_ranges += 1;
                if broken_ranges > RANGE_RETRIES {
                    return (Ending::Failed(format!("range {range_start}-{range_end} broke off at {offset}: {e}")), sent);
                }
                tracing::warn!("[url] range {}-{} broke off at {}: {}, resuming", range_start, range_end, offset, e);
            }

            // Open the next range (or the rest of a broken one).
            self.refresher.keep_alive();
            range_start = offset;
            range_end = self.range_end(offset);
            let mut attempts = 0;
            body = loop {
                if tx.is_closed() {
                    return (Ending::Cancelled, sent);
                }
                rn += 1;
                match open_range(&self.http, &self.url, range_start, range_end, rn, mode, self.read_timeout).await {
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

    // Recognisable secret values: none may appear in any log or error.
    const S_SECRET: &str = "S-SECRET-SIGNATURE";
    const N_SECRET: &str = "N-SECRET-NVALUE";
    const POT_SECRET: &str = "POT-SECRET-TOKEN";
    const CPN_SECRET: &str = "CPN-SECRET";
    const SECRETS: [&str; 6] = [S_SECRET, N_SECRET, POT_SECRET, CPN_SECRET, "SECRET-SIG", "SECRET-NVALUE"];

    fn assert_no_secrets(text: &str) {
        for secret in SECRETS {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(!text.to_uppercase().contains("EULAVN-TERCES"), "reversed n leaked: {text}");
    }

    /// Reverses `n` and upper-cases the signature, like a (fake) base.js.
    struct FakeDecipher;

    impl Decipher for FakeDecipher {
        fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
            Ok((n.map(|n| n.chars().rev().collect()), sig.map(str::to_lowercase)))
        }
    }

    fn format_url() -> String {
        format!("https://rr1---sn-abc.googlevideo.com/videoplayback?expire=1&itag=141&n={N_SECRET}&sparams=expire")
    }

    fn cipher() -> String {
        serde_urlencoded::to_string([("s", S_SECRET), ("sp", "sig"), ("url", &format_url())]).unwrap()
    }

    fn query(url: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    }

    fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn signature_cipher_is_split_into_url_s_and_sp() {
        let source = format_source(None, Some(&cipher())).unwrap();

        assert_eq!(source.url, format_url());
        assert_eq!(source.signature, Some(("sig".to_owned(), S_SECRET.to_owned())));
        assert_eq!(format_source(Some(&format_url()), None).unwrap().signature, None);
        assert!(format_source(None, None).is_err());
    }

    #[test]
    fn source_description_has_names_but_no_values() {
        let text = describe_source(None, Some(&cipher()));
        assert_eq!(text, "signatureCipher (fields s, sp, url; url params expire, itag, n, sparams)");
        let plain = describe_source(Some(&format_url()), None);
        assert_eq!(plain, "url (host rr1---sn-abc.googlevideo.com; params expire, itag, n, sparams)");
        assert_eq!(describe_source(None, None), "neither url nor signatureCipher");
    }

    #[test]
    fn resolved_url_has_signature_in_sp_solved_n_pot_cpn_and_cver() {
        let logs = crate::storage::test_util::LogCapture::start();
        let source = format_source(None, Some(&cipher())).unwrap();

        let url = resolve_url(&source, &FakeDecipher, Some(POT_SECRET), "1.2026", CPN_SECRET).unwrap();

        let pairs = query(&url);
        assert_eq!(param(&pairs, "sig"), Some(S_SECRET.to_lowercase().as_str()));
        assert_eq!(param(&pairs, "n"), Some(N_SECRET.chars().rev().collect::<String>().as_str()));
        assert_eq!(param(&pairs, "pot"), Some(POT_SECRET));
        assert_eq!(param(&pairs, "cpn"), Some(CPN_SECRET));
        assert_eq!(param(&pairs, "cver"), Some("1.2026"));
        assert_eq!(param(&pairs, "itag"), Some("141"));
        assert!(!url.contains(S_SECRET), "the raw s must not be sent");
        assert_no_secrets(&logs.text());
    }

    #[test]
    fn plain_url_without_pot_gets_n_solved_only() {
        let source = format_source(Some(&format_url()), None).unwrap();
        let url = resolve_url(&source, &FakeDecipher, None, "v", "c").unwrap();
        let pairs = query(&url);
        assert_eq!(param(&pairs, "n"), Some(N_SECRET.chars().rev().collect::<String>().as_str()));
        assert_eq!(param(&pairs, "pot"), None);
        assert_eq!(param(&pairs, "sig"), None);
    }

    #[test]
    fn untrusted_format_host_is_refused() {
        let source = format_source(Some(&format!("https://evil.example/videoplayback?sig={S_SECRET}")), None).unwrap();
        let err = resolve_url(&source, &FakeDecipher, Some(POT_SECRET), "v", "c").unwrap_err();
        assert!(err.to_string().contains("evil.example"), "{err}");
        assert_no_secrets(&err.to_string());
    }

    #[test]
    fn decipher_failure_is_a_typed_error_without_values() {
        struct Broken;
        impl Decipher for Broken {
            fn solve(&self, _: Option<&str>, _: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
                Err("jsc n error: no solutions".to_owned())
            }
        }
        let source = format_source(None, Some(&cipher())).unwrap();

        let err = resolve_url(&source, &Broken, Some(POT_SECRET), "v", CPN_SECRET).unwrap_err();

        assert!(err.is::<DecipherError>(), "{err}");
        assert_eq!(err.to_string(), "decipher failed: jsc n error: no solutions");
        assert_no_secrets(&format!("{err} {err:?}"));
    }

    #[test]
    fn player_js_decipher_solves_n_and_sig_with_the_session_player() {
        let tmp = std::env::temp_dir().join(format!("ytcast-decipher-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut session = crate::music_session::test_fixtures::session();
        session.player_js = Some(crate::nsig::SYNTHETIC_PLAYER.to_owned());
        session.player_hash = Some("test-decipher-hash".to_owned());
        let decipher = PlayerJsDecipher {
            session: Arc::new(session),
            disk_cache: Some(tmp.join("pp.js")),
            solver: crate::nsig::solve_cached_at,
        };

        let (n, sig) = decipher.solve(Some("abc"), Some("sig")).unwrap();

        assert_eq!((n.as_deref(), sig.as_deref()), (Some("cba"), Some("SIG")));
        assert!(tmp.join("pp.js").exists());
        // Keyed by this session's player hash: another hash with no base.js fails.
        let mut other = crate::music_session::test_fixtures::session();
        other.player_js = Some(String::new());
        other.player_hash = Some("test-decipher-other-hash".to_owned());
        let other = PlayerJsDecipher { session: Arc::new(other), disk_cache: None, solver: crate::nsig::solve_cached_at };
        assert!(other.solve(Some("abc"), None).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Counts `solve` calls and records what each was asked.
    #[derive(Default)]
    struct CountingDecipher {
        calls: Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl Decipher for CountingDecipher {
        fn solve(&self, n: Option<&str>, sig: Option<&str>) -> Result<(Option<String>, Option<String>), String> {
            self.calls.lock().unwrap().push((n.map(str::to_owned), sig.map(str::to_owned)));
            FakeDecipher.solve(n, sig)
        }
    }

    #[test]
    fn resolve_solves_n_and_sig_in_exactly_one_call() {
        let decipher = CountingDecipher::default();
        let source = format_source(None, Some(&cipher())).unwrap();

        resolve_url(&source, &decipher, None, "v", "c").unwrap();

        let calls = decipher.calls.lock().unwrap().clone();
        assert_eq!(calls, [(Some(N_SECRET.to_owned()), Some(S_SECRET.to_owned()))]);
    }

    static SOLVER_CALLS: Mutex<Vec<(Vec<(crate::nsig::Challenge, String)>, String, Option<std::path::PathBuf>)>> =
        Mutex::new(Vec::new());

    fn recording_solver(
        _js: &str,
        requests: &[(crate::nsig::Challenge, &str)],
        hash: &str,
        disk_cache: Option<std::path::PathBuf>,
    ) -> Result<Vec<String>, String> {
        SOLVER_CALLS.lock().unwrap().push((
            requests.iter().map(|(c, v)| (*c, (*v).to_owned())).collect(),
            hash.to_owned(),
            disk_cache,
        ));
        Ok(requests.iter().map(|(_, v)| format!("solved-{v}")).collect())
    }

    #[test]
    fn player_js_decipher_makes_one_solver_run_keyed_by_the_player_hash() {
        use crate::nsig::Challenge;
        let mut session = crate::music_session::test_fixtures::session();
        session.player_hash = Some("recording-hash-1".to_owned());
        let mut decipher = PlayerJsDecipher::new(Arc::new(session));
        assert_eq!(decipher.disk_cache, crate::nsig::cache_path("recording-hash-1"));
        assert_ne!(decipher.disk_cache, crate::nsig::cache_path(""));
        decipher.solver = recording_solver;

        let answers = decipher.solve(Some("nn"), Some("ss")).unwrap();

        assert_eq!(answers, (Some("solved-nn".to_owned()), Some("solved-ss".to_owned())));
        let calls: Vec<_> = SOLVER_CALLS
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, hash, _)| hash == "recording-hash-1")
            .cloned()
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, [(Challenge::N, "nn".to_owned()), (Challenge::Sig, "ss".to_owned())]);
        assert_eq!(calls[0].2, crate::nsig::cache_path("recording-hash-1"));
    }

    // -- fake googlevideo ---------------------------------------------------------

    #[derive(Clone, Copy, Default)]
    struct Behaviour {
        /// Status for GET / POST (0 = 200).
        get_status: u16,
        post_status: u16,
        /// From this request on (0-based), URLs without `fresh=1` get 403.
        expire_from: Option<usize>,
        /// (request index, bytes): announce the whole range, send only these
        /// bytes, then close the connection mid-body.
        truncate: Option<(usize, usize)>,
        /// (request index, bytes): a well-formed but short response.
        short: Option<(usize, usize)>,
        /// Send this many body bytes, then stall without closing.
        stall_after: Option<usize>,
    }

    fn status_or_ok(status: u16) -> u16 {
        if status == 0 { 200 } else { status }
    }

    /// Serves `content` by `range=a-b`: GET as raw bytes, POST (`ump=1`) as
    /// UMP MEDIA parts. Records each request line.
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
                let content = content.clone();
                let record = record.clone();
                // Each connection on its own task: a stalled one must not
                // block the next request.
                tokio::spawn(async move {
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
                    let index = {
                        let mut seen = record.lock().unwrap();
                        seen.push(line.clone());
                        seen.len() - 1
                    };
                    let is_post = line.starts_with("POST");
                    let expired = behaviour.expire_from.is_some_and(|from| index >= from) && !line.contains("fresh=1");
                    let status = if expired {
                        403
                    } else if is_post {
                        status_or_ok(behaviour.post_status)
                    } else {
                        status_or_ok(behaviour.get_status)
                    };
                    let path = line.split(' ').nth(1).unwrap_or("");
                    let pairs: Vec<(String, String)> =
                        serde_urlencoded::from_str(path.split_once('?').map_or("", |(_, q)| q)).unwrap_or_default();
                    let range = pairs.iter().find(|(k, _)| k == "range").map(|(_, v)| v.clone()).unwrap_or_default();
                    let (a, b) = range.split_once('-').unwrap_or(("0", "0"));
                    let (a, b): (usize, usize) = (a.parse().unwrap_or(0), b.parse().unwrap_or(0));
                    let mut slice = content.get(a..=b.min(content.len().saturating_sub(1))).unwrap_or(&[]).to_vec();
                    if let Some((at, keep)) = behaviour.short {
                        if at == index {
                            slice.truncate(keep);
                        }
                    }
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
                    let send = match (behaviour.truncate, behaviour.stall_after) {
                        (Some((at, keep)), _) if at == index => keep.min(body.len()),
                        (_, Some(keep)) => keep.min(body.len()),
                        _ => body.len(),
                    };
                    let _ = sock.write_all(&body[..send]).await;
                    if behaviour.stall_after.is_some() && send < body.len() {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                });
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

    /// Refreshes to the same URL, so a refusal persists.
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

    async fn fetcher(behaviour: Behaviour, data: &[u8], clen: Option<u64>) -> (RangeFetch, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = spawn_videoplayback(listener, data.to_vec(), behaviour);
        let url = format!("http://{addr}/videoplayback?itag=141&sig=SECRET-SIG&pot={POT_SECRET}&cpn={CPN_SECRET}");
        let fetch = RangeFetch {
            http: reqwest::Client::new(),
            url: url.clone(),
            content_length: clen,
            chunk_size: 1000,
            itag: 141,
            start_offset: 0,
            refresher: Box::new(FreshUrl { url, calls: Arc::default() }),
            read_timeout: std::time::Duration::from_millis(500),
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

    const OK: Behaviour = Behaviour {
        get_status: 0,
        post_status: 0,
        expire_from: None,
        truncate: None,
        short: None,
        stall_after: None,
    };

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
        assert!(logs.contains("complete: 2500 bytes"), "{logs}");
        assert_no_secrets(&logs);
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
    async fn with_content_length_a_short_range_is_resumed_at_its_end() {
        let data = content(2500);
        let (fetch, seen) = fetcher(Behaviour { short: Some((1, 300)), ..OK }, &data, Some(2500)).await;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999", "GET 1300-2299", "GET 2300-2499"]);
    }

    #[tokio::test]
    async fn a_range_broken_off_mid_body_resumes_at_the_exact_byte() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(2500);
        let (fetch, seen) = fetcher(Behaviour { truncate: Some((1, 400)), ..OK }, &data, Some(2500)).await;

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999", "GET 1400-2399", "GET 2400-2499"]);
        assert_no_secrets(&logs.text());
    }

    #[tokio::test]
    async fn the_break_budget_is_per_range_not_per_song() {
        // Three separate ranges each break once; each is resumed.
        let data = content(4000);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Requests 1, 3 and 5 break off after 100 bytes.
        let seen = spawn_multi_truncate(listener, data.clone(), vec![1, 3, 5]);
        let url = format!("http://{addr}/videoplayback?itag=141");
        let fetch = RangeFetch {
            http: reqwest::Client::new(),
            url: url.clone(),
            content_length: Some(4000),
            chunk_size: 1000,
            itag: 141,
            start_offset: 0,
            refresher: Box::new(SameUrl { url, calls: Arc::default() }),
            read_timeout: std::time::Duration::from_millis(500),
        };

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(seen.lock().unwrap().len(), 7);
    }

    /// Like `spawn_videoplayback` with OK behaviour, but the listed requests
    /// break off after 100 body bytes.
    fn spawn_multi_truncate(listener: tokio::net::TcpListener, content: Vec<u8>, broken: Vec<usize>) -> Arc<Mutex<Vec<String>>> {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = text.lines().next().unwrap_or("").to_owned();
                let index = {
                    let mut seen = record.lock().unwrap();
                    seen.push(line.clone());
                    seen.len() - 1
                };
                let range = line.split("range=").nth(1).unwrap_or("0-0").split('&').next().unwrap_or("0-0").to_owned();
                let (a, b) = range.split_once('-').unwrap();
                let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                let body = content[a..=b.min(content.len() - 1)].to_vec();
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let send = if broken.contains(&index) { 100 } else { body.len() };
                let _ = sock.write_all(&body[..send]).await;
            }
        });
        seen
    }

    #[tokio::test]
    async fn refused_get_falls_back_to_the_ump_post_form() {
        let data = content(1800);
        let (fetch, seen) = fetcher(Behaviour { get_status: 403, ..OK }, &data, Some(1800)).await;

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
        let (fetch, seen) = fetcher(Behaviour { get_status: 403, post_status: 403, ..OK }, &data, Some(1000)).await;

        let err = fetch.start().await.unwrap_err();

        assert!(err.to_string().contains("HTTP 403"), "{err}");
        assert_eq!(ranges(&seen), ["GET 0-999", "POST 0-999"]);
        assert_no_secrets(&format!("{err} {}", logs.text()));
    }

    #[tokio::test]
    async fn expired_url_mid_song_is_refreshed_once_and_resumes_at_the_offset() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(3500);
        let (mut fetch, seen) = fetcher(Behaviour { expire_from: Some(2), ..OK }, &data, Some(3500)).await;
        let calls: Arc<Mutex<u32>> = Arc::default();
        fetch.refresher = Box::new(FreshUrl { url: fetch.url.clone(), calls: calls.clone() });

        let rx = fetch.start().await.unwrap();

        assert_eq!(collect(rx).await, data);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 1000-1999", "GET 2000-2999", "GET 2000-2999", "GET 3000-3499"]);
        let lines = seen.lock().unwrap().clone();
        assert!(lines[3].contains("fresh=1") && lines[4].contains("fresh=1"), "{lines:?}");
        assert_no_secrets(&logs.text());
    }

    #[tokio::test]
    async fn the_url_is_refreshed_only_once_per_song() {
        let data = content(5000);
        let (mut fetch, _seen) = fetcher(Behaviour { expire_from: Some(1), ..OK }, &data, Some(5000)).await;
        let calls: Arc<Mutex<u32>> = Arc::default();
        fetch.refresher = Box::new(SameUrl { url: fetch.url.clone(), calls: calls.clone() });

        let rx = fetch.start().await.unwrap();

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
    async fn dropping_the_receiver_during_a_slow_range_stops_at_once() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(100_000);
        // One big range; the server sends 2000 bytes and then stalls.
        let (mut fetch, _seen) = fetcher(Behaviour { stall_after: Some(2000), ..OK }, &data, Some(100_000)).await;
        fetch.chunk_size = 100_000;
        fetch.read_timeout = std::time::Duration::from_secs(10);

        let mut rx = fetch.start().await.unwrap();
        rx.recv().await.unwrap();
        while rx.try_recv().is_ok() {}
        drop(rx);

        let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if logs.text().contains("stopped by the player") {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(stopped.is_ok(), "download kept waiting on the stalled server: {}", logs.text());
    }

    #[tokio::test]
    async fn a_stalled_server_times_out() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(3000);
        let (fetch, seen) = fetcher(Behaviour { stall_after: Some(200), ..OK }, &data, Some(3000)).await;

        let started = std::time::Instant::now();
        let rx = fetch.start().await.unwrap();
        let got = collect(rx).await;

        // Each range stalls after 200 bytes: 1 + RANGE_RETRIES resumes, then give up.
        assert!(got.len() < 3000);
        assert_eq!(got, data[..got.len()]);
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
        let logs = logs.text();
        assert!(logs.contains("read timed out"), "{logs}");
        assert!(logs.contains("failed after"), "{logs}");
        // The first request, then RANGE_RETRIES resumes of the same range.
        assert_eq!(ranges(&seen), ["GET 0-999", "GET 200-1199", "GET 400-1399"]);
    }

    /// Drops the consumer when the next range is about to be opened.
    struct DropAtBoundary {
        rx: Arc<Mutex<Option<mpsc::Receiver<Bytes>>>>,
    }

    impl UrlRefresher for DropAtBoundary {
        fn refresh(&mut self) -> RefreshFuture<'_> {
            Box::pin(async { Err("not expected".into()) })
        }
        fn keep_alive(&self) {
            self.rx.lock().unwrap().take();
        }
    }

    #[tokio::test]
    async fn no_new_range_is_opened_after_a_drop_at_a_range_boundary() {
        let logs = crate::storage::test_util::LogCapture::start();
        let data = content(3000);
        let (mut fetch, seen) = fetcher(OK, &data, Some(3000)).await;
        let slot: Arc<Mutex<Option<mpsc::Receiver<Bytes>>>> = Arc::default();
        fetch.refresher = Box::new(DropAtBoundary { rx: slot.clone() });

        let rx = fetch.start().await.unwrap();
        *slot.lock().unwrap() = Some(rx);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !logs.text().contains("stopped by the player") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("download did not stop");

        assert_eq!(ranges(&seen), ["GET 0-999"]);
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

    #[test]
    fn range_end_does_not_overflow() {
        let fetch = RangeFetch {
            http: reqwest::Client::new(),
            url: String::new(),
            content_length: None,
            chunk_size: 1000,
            itag: 141,
            start_offset: 0,
            refresher: Box::new(SameUrl { url: String::new(), calls: Arc::default() }),
            read_timeout: READ_TIMEOUT,
        };
        assert_eq!(fetch.range_end(u64::MAX - 5), u64::MAX);
    }

    #[tokio::test]
    async fn transport_errors_do_not_carry_the_url() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let url = format!("http://{addr}/videoplayback?sig=SECRET-SIG&pot={POT_SECRET}");

        let err = match open_range(&reqwest::Client::new(), &url, 0, 999, 1, RangeMode::Get, READ_TIMEOUT).await {
            Ok(_) => panic!("closed port answered"),
            Err(e) => e,
        };

        assert_no_secrets(&format!("{err} {err:?}"));
    }
}
