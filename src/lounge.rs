use std::collections::HashMap;
use std::error::Error as StdError;
use std::time::Duration;

use rand::Rng;
use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::messages::{
    self, encode_outgoing, parse_chunks, zx, IncomingMessage, OutgoingMessage,
};

const BIND_URL: &str = "https://www.youtube.com/api/lounge/bc/bind";
const PAIRING_URL: &str = "https://www.youtube.com/api/lounge/pairing";

/// Base backoff delay for reconnection attempts.
const BACKOFF_BASE_MS: u64 = 500;
/// Maximum number of retries before giving up on a single connection cycle.
const MAX_RETRIES: u32 = 25;

pub type Result<T> = std::result::Result<T, LoungeError>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LoungeError {
    Http(reqwest::Error),
    Message(messages::MessageError),
    /// Server returned 400 with "Unknown SID" — needs reconnect
    UnknownSid,
    /// Server returned 410 — token expired, need full re-bind
    Gone,
    /// Protocol error (unexpected response, missing fields, etc.)
    Protocol(String),
    /// Shutdown signal received
    Shutdown,
}

impl std::fmt::Display for LoungeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoungeError::Http(e) => write!(f, "lounge http: {e}"),
            LoungeError::Message(e) => write!(f, "lounge message: {e}"),
            LoungeError::UnknownSid => write!(f, "lounge: unknown SID"),
            LoungeError::Gone => write!(f, "lounge: session gone (410)"),
            LoungeError::Protocol(msg) => write!(f, "lounge protocol: {msg}"),
            LoungeError::Shutdown => write!(f, "lounge: shutdown"),
        }
    }
}

impl std::error::Error for LoungeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoungeError::Http(e) => Some(e),
            LoungeError::Message(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for LoungeError {
    /// Lounge URLs carry SID, gsessionid and loungeIdToken in the query, and
    /// reqwest errors print their URL; strip it so errors can be logged.
    fn from(e: reqwest::Error) -> Self {
        LoungeError::Http(e.without_url())
    }
}

impl From<messages::MessageError> for LoungeError {
    fn from(e: messages::MessageError) -> Self {
        LoungeError::Message(e)
    }
}

// ---------------------------------------------------------------------------
// Player commands (sent from lounge to the player task)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    SetPlaylist {
        video_id: String,
        video_ids: Vec<String>,
        index: usize,
        current_time: f64,
        list_id: String,
        ctt: Option<String>,
        params: Option<String>,
        /// Replayed in the receiver's own bind or rebind response: no proof
        /// that a sender is connected now.
        from_bind: bool,
    },
    SetVideo {
        video_id: String,
        ctt: Option<String>,
        current_time: f64,
        /// See `SetPlaylist::from_bind`.
        from_bind: bool,
    },
    Play,
    Pause,
    SeekTo {
        position: f64,
    },
    SetVolume {
        volume: u32,
    },
    GetVolume,
    GetNowPlaying,
    GetPlaylist,
    Stop,
    Previous,
    Next,
    /// `device_ids`: the senders' Lounge device ids, for /next's MDX context;
    /// `name`: the sender's name, which tells apart senders without an id.
    RemoteConnected { theme: String, device_ids: Vec<String>, name: Option<String> },
    RemoteDisconnected { theme: String, device_id: Option<String>, name: Option<String> },
    /// A loungeStatus says the user ended a cast here (took it back to the
    /// phone): sent before the other commands of its batch. `device_id`: the
    /// leaving device, if known; `nobody_left`: the status lists no sender.
    /// `leaving_ids`: the batch's leaving senders (remoteDisconnected).
    StoppedByUser { theme: String, device_id: Option<String>, leaving_ids: Vec<String>, nobody_left: bool },
    /// A loungeStatus: the senders connected now, `device_ids` for those with
    /// a readable id and `unidentified` for the others. Replaces the known ones.
    SendersListed { theme: String, device_ids: Vec<String>, unidentified: usize },
    /// The receiver's own (re)bind listed nobody: a sender whose leave was
    /// lost meanwhile is no longer counted, but a phone that is still there
    /// may just not be listed yet, so this ends nothing.
    SendersForgotten { theme: String },
    UpdatePlaylist {
        video_ids: Vec<String>,
        list_id: String,
        current_index: Option<usize>,
    },
}

// ---------------------------------------------------------------------------
// Lounge session state
// ---------------------------------------------------------------------------

struct LoungeSession {
    client: reqwest::Client,
    device_name: String,
    uuid: String,
    screen_id: String,
    lounge_token: String,
    theme: String,
    sid: String,
    gsessionid: String,
    aid: i32,
    rid: u32,
    ofs: u32,
}

impl LoungeSession {
    fn new(
        client: reqwest::Client,
        device_name: String,
        uuid: String,
        screen_id: String,
        theme: String,
    ) -> Self {
        let mut rng = rand::thread_rng();
        Self {
            client,
            device_name,
            uuid,
            screen_id,
            lounge_token: String::new(),
            theme,
            sid: String::new(),
            gsessionid: String::new(),
            aid: -1,
            rid: rng.gen_range(10_000..100_000),
            ofs: 0,
        }
    }

    fn next_rid(&mut self) -> u32 {
        let r = self.rid;
        self.rid += 1;
        r
    }

    /// Common query parameters used by all bind requests.
    /// Matches the Node.js yt-cast-receiver BindParams.commonParams exactly.
    fn bind_query_base(&self) -> Vec<(String, String)> {
        vec![
            ("device".into(), "LOUNGE_SCREEN".into()),
            ("id".into(), self.uuid.clone()),
            ("obfuscatedGaiaId".into(), String::new()),
            ("name".into(), self.device_name.clone()),
            ("app".into(), "ytcr".into()),
            ("theme".into(), self.theme.clone()),
            ("capabilities".into(), "dsp,mic,dpa,ntb".into()),
            ("cst".into(), "m".into()),
            ("mdxVersion".into(), "2".into()),
            ("loungeIdToken".into(), self.lounge_token.clone()),
            ("VER".into(), "8".into()),
            ("v".into(), "2".into()),
            ("zx".into(), zx()),
            ("t".into(), "1".into()),
        ]
    }

    /// JSON deviceInfo blob sent with initSession and sendMessage requests.
    fn device_info_json(&self) -> String {
        serde_json::json!({
            "brand": "ytcast-lite",
            "model": "ytcast-lite",
            "year": 0,
            "os": "Linux",
            "osVersion": "1.0",
            "chipset": "",
            "clientName": "TVHTML5",
            "dialAdditionalDataSupportLevel": "unsupported",
            "mdxDialServerType": "MDX_DIAL_SERVER_TYPE_UNKNOWN"
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Pairing / token API calls
// ---------------------------------------------------------------------------

/// Generate a new screen ID. Called once; the result should be persisted.
pub async fn generate_screen_id(client: &reqwest::Client) -> Result<String> {
    let url = format!("{PAIRING_URL}/generate_screen_id");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "generate_screen_id returned {status}"
        )));
    }
    let text = resp.text().await?;
    Ok(text.trim().to_owned())
}

/// Fetch a lounge token for the given screen ID.
pub async fn get_lounge_token(
    client: &reqwest::Client,
    screen_id: &str,
) -> Result<String> {
    let url = format!("{PAIRING_URL}/get_lounge_token_batch");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("screen_ids={screen_id}"))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "get_lounge_token_batch returned {status}"
        )));
    }

    let data: serde_json::Value = resp.json().await?;
    let token = data["screens"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|s| s["loungeToken"].as_str())
        .ok_or_else(|| {
            LoungeError::Protocol(format!(
                "missing loungeToken in response (keys: {:?})",
                data.as_object().map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default()
            ))
        })?
        .to_owned();

    Ok(token)
}

/// Register a pairing code so a phone can discover this screen.
pub async fn register_pairing_code(
    client: &reqwest::Client,
    screen_id: &str,
    device_name: &str,
    device_id: &str,
    code: &str,
) -> Result<()> {
    let url = format!("{PAIRING_URL}/register_pairing_code");
    let screen_name = format!("YouTube on {}", device_name);
    let body = format!(
        "access_type=permanent&app=ytcr&pairing_code={}&screen_id={}&screen_name={}&device_id={}",
        messages::percent_encode_public(code),
        messages::percent_encode_public(screen_id),
        messages::percent_encode_public(&screen_name),
        messages::percent_encode_public(device_id),
    );

    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LoungeError::Protocol(format!(
            "register_pairing_code returned {status} (body len={})",
            resp_body.len()
        )));
    }

    tracing::info!("pairing code registered (status={status}, body len={})", resp_body.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Bind operations
// ---------------------------------------------------------------------------

impl LoungeSession {
    /// Perform the initial bind (POST) to establish the session.
    ///
    /// Parses the response for `c` (SID) and `S` (gsessionid) commands.
    async fn initial_bind(&mut self) -> Result<Vec<IncomingMessage>> {
        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("CVER".into(), "1".into()));

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("count=0")
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST && body.contains("Unknown SID") {
                return Err(LoungeError::UnknownSid);
            }
            return Err(LoungeError::Protocol(format!(
                "initial_bind returned {status} (body len={})",
                body.len()
            )));
        }

        let body = resp.bytes().await?;
        let messages = parse_chunks(&body)?;

        // Extract session identifiers
        for msg in &messages {
            self.handle_session_command(msg);
        }

        if self.sid.is_empty() {
            return Err(LoungeError::Protocol(
                "initial bind did not return a SID (\"c\" command)".into(),
            ));
        }

        tracing::info!("[lounge:{}] session bound", self.theme);
        Ok(messages)
    }

    /// Reconnect after an "Unknown SID" error by passing the old SID/AID as
    /// OSID/OAID parameters.
    async fn reconnect_bind(&mut self) -> Result<Vec<IncomingMessage>> {
        let old_sid = self.sid.clone();
        let old_aid = self.aid;

        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("CVER".into(), "1".into()));
        query.push(("OSID".into(), old_sid));
        query.push(("OAID".into(), old_aid.to_string()));

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("count=0")
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LoungeError::Protocol(format!(
                "reconnect_bind returned {status} (body len={})",
                body.len()
            )));
        }

        let body = resp.bytes().await?;
        let messages = parse_chunks(&body)?;

        // Reset SID/gsession from the new response
        self.sid.clear();
        self.gsessionid.clear();
        for msg in &messages {
            self.handle_session_command(msg);
        }

        if self.sid.is_empty() {
            return Err(LoungeError::Protocol(
                "reconnect bind did not return a new SID".into(),
            ));
        }

        tracing::info!("[lounge:{}] reconnected", self.theme);
        Ok(messages)
    }

    /// Start a streaming long-poll GET. Messages are sent through `msg_tx`
    /// as they arrive from the HTTP response stream. The returned future
    /// completes when the connection closes, times out idle, or errors.
    ///
    /// This does not borrow `self` — it clones everything it needs so the
    /// session can be mutated in other `select!` branches.
    fn start_streaming_poll(
        &self,
        msg_tx: mpsc::Sender<Vec<IncomingMessage>>,
    ) -> impl std::future::Future<Output = Result<()>> {
        let mut query = self.bind_query_base();
        query.push(("SID".into(), self.sid.clone()));
        query.push(("CI".into(), "0".into()));
        query.push(("AID".into(), self.aid.to_string()));
        query.push(("gsessionid".into(), self.gsessionid.clone()));
        query.push(("TYPE".into(), "xmlhttp".into()));
        query.push(("RID".into(), "rpc".into()));

        let client = self.client.clone();

        async move {
            tracing::debug!(
                "streaming poll starting (AID={})",
                query.iter().find(|(k, _)| k == "AID").map(|(_, v)| v.as_str()).unwrap_or("?")
            );

            let resp = client
                .get(BIND_URL)
                .query(&query)
                .send()
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::GONE {
                return Err(LoungeError::Gone);
            }
            if status == reqwest::StatusCode::BAD_REQUEST {
                let body = resp.text().await.unwrap_or_default();
                if body.contains("Unknown SID") {
                    return Err(LoungeError::UnknownSid);
                }
                return Err(LoungeError::Protocol(format!(
                    "poll returned 400 (body len={})",
                    body.len()
                )));
            }
            if !status.is_success() {
                return Err(LoungeError::Protocol(format!(
                    "poll returned {status}"
                )));
            }

            use futures_util::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut parser = messages::ChunkParser::new();

            loop {
                match time::timeout(Duration::from_secs(45), stream.next()).await {
                    Err(_) => {
                        // No data for 45s — normal idle timeout
                        tracing::trace!("streaming poll idle timeout");
                        return Ok(());
                    }
                    Ok(None) => {
                        // Stream ended (server closed connection)
                        tracing::debug!("streaming poll connection closed");
                        return Ok(());
                    }
                    Ok(Some(Ok(bytes))) => {
                        let msgs = parser.feed(&bytes);
                        if !msgs.is_empty() {
                            tracing::debug!("streaming poll: {} messages received", msgs.len());
                            if msg_tx.send(msgs).await.is_err() {
                                return Ok(()); // receiver dropped
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    /// Send outgoing messages to YouTube via POST.
    async fn send_messages(&mut self, messages: &[OutgoingMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let rid = self.next_rid();
        let mut query = self.bind_query_base();
        query.push(("deviceInfo".into(), self.device_info_json()));
        query.push(("SID".into(), self.sid.clone()));
        query.push(("RID".into(), rid.to_string()));
        query.push(("AID".into(), self.aid.to_string()));
        query.push(("gsessionid".into(), self.gsessionid.clone()));

        let body = encode_outgoing(messages, self.ofs);
        self.ofs += messages.len() as u32;

        tracing::debug!("[lounge] POST body len={}", body.len());

        let resp = self
            .client
            .post(BIND_URL)
            .query(&query)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::GONE {
            return Err(LoungeError::Gone);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST && body.contains("Unknown SID") {
                return Err(LoungeError::UnknownSid);
            }
            return Err(LoungeError::Protocol(format!(
                "send_messages returned {status} (body len={})",
                body.len()
            )));
        }

        Ok(())
    }

    /// Process session-management commands (`c`, `S`) from an incoming message.
    fn handle_session_command(&mut self, msg: &IncomingMessage) {
        match msg.command.as_str() {
            "c" => {
                if let Some(sid) = msg.args.get("") {
                    self.sid = sid.clone();
                }
            }
            "S" => {
                if let Some(gsid) = msg.args.get("") {
                    self.gsessionid = gsid.clone();
                }
            }
            _ => {}
        }

        // Track highest message index
        if msg.index > self.aid {
            self.aid = msg.index;
        }
    }
}

// ---------------------------------------------------------------------------
// Command dispatch — map incoming Lounge commands to PlayerCommands
// ---------------------------------------------------------------------------

/// Try to send a player command, logging if the channel is full or closed.
fn send_cmd(player_tx: &mpsc::Sender<PlayerCommand>, cmd: PlayerCommand) {
    if let Err(e) = player_tx.try_send(cmd) {
        tracing::warn!("player command dropped: {e}");
    }
}

/// Where incoming messages came from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Delivery {
    /// The receiver's own bind or rebind (after "Unknown SID"): its
    /// loungeStatus may not list yet a phone that is still connected.
    Bind,
    /// The live poll.
    Poll,
}

/// A bind or rebind response's messages (see `Delivery::Bind`).
fn dispatch_bind_response(
    session: &mut LoungeSession,
    messages: &[IncomingMessage],
    player_tx: &mpsc::Sender<PlayerCommand>,
) -> Vec<OutgoingMessage> {
    dispatch_messages(session, messages, player_tx, Delivery::Bind)
}

/// Process a batch of incoming messages, dispatching player commands and
/// updating session state. Returns any outgoing messages that should be
/// sent back to YouTube in response.
fn dispatch_messages(
    session: &mut LoungeSession,
    messages: &[IncomingMessage],
    player_tx: &mpsc::Sender<PlayerCommand>,
    delivery: Delivery,
) -> Vec<OutgoingMessage> {
    let mut responses: Vec<OutgoingMessage> = Vec::new();
    let from_bind = delivery == Delivery::Bind;
    // A (re)bind that lists nobody forgets the senders before any other
    // command of its response: a cast it replays is then judged with nobody
    // known, and a sender event later in the response counts.
    if from_bind && messages.iter().any(lists_nobody) {
        send_cmd(player_tx, PlayerCommand::SendersForgotten { theme: session.theme.clone() });
    }
    // The user ended the cast here: before the batch's lists and leaves, so
    // no list of nobody lets the queue play on first.
    if let Some((device_id, leaving_ids, nobody_left)) =
        (!from_bind).then(|| ended_by_user(&session.theme, messages)).flatten()
    {
        let theme = session.theme.clone();
        send_cmd(player_tx, PlayerCommand::StoppedByUser { theme, device_id, leaving_ids, nobody_left });
    }
    // A sender counted since then in this response: a later list of nobody
    // must forget it again.
    let mut sender_event_seen = false;

    for msg in messages {
        session.handle_session_command(msg);

        match msg.command.as_str() {
            // -- Session control (handled above) --
            "c" | "S" => {}

            // -- Keepalive --
            "noop" => {}

            // -- Remote device lifecycle --
            "remoteConnected" => {
                let name = msg.args.get("name").map(String::as_str).unwrap_or("unknown");
                tracing::info!("[lounge:{}] remote connected: {name}", session.theme);
                let device_ids = sender_id(&msg.args).into_iter().collect();
                let name = msg.args.get("name").cloned();
                send_cmd(player_tx, PlayerCommand::RemoteConnected { theme: session.theme.clone(), device_ids, name });
                sender_event_seen = true;
                // Tell the phone we're ready — matches yt-cast-receiver handleSenderConnected:
                // 1. onHasPreviousNextChanged
                // 2. nowPlaying (empty = nothing playing)
                // 3. onStateChange (idle)
                // 4. onAutoplayModeChanged (UNSUPPORTED — we don't do autoplay)
                responses.push(
                    OutgoingMessage::new("onHasPreviousNextChanged")
                        .with_arg("hasPrevious", "false")
                        .with_arg("hasNext", "false"),
                );
                responses.push(
                    OutgoingMessage::new("nowPlaying"),
                );
                responses.push(
                    OutgoingMessage::new("onStateChange")
                        .with_arg("state", "-1"),
                );
                responses.push(
                    OutgoingMessage::new("onAutoplayModeChanged")
                        .with_arg("autoplayMode", "UNSUPPORTED"),
                );
            }
            "remoteDisconnected" => {
                let name = msg.args.get("name").map(String::as_str).unwrap_or("unknown");
                tracing::info!("[lounge:{}] remote disconnected: {name}", session.theme);
                send_cmd(
                    player_tx,
                    PlayerCommand::RemoteDisconnected {
                        theme: session.theme.clone(),
                        device_id: sender_id(&msg.args),
                        name: msg.args.get("name").cloned(),
                    },
                );
            }

            // -- Lounge status --
            "loungeStatus" => {
                tracing::debug!("lounge status: keys={:?}", arg_keys(&msg.args));
                if let Some(behavior) = leave_details(&msg.args).and_then(|d| d.behavior) {
                    tracing::info!("[lounge:{}] loungeStatus disconnectBehavior={}", session.theme, loggable(&behavior));
                }
                // Check if any REMOTE_CONTROL senders are connected
                let (device_ids, unidentified) =
                    msg.args.get("devices").map(|d| remote_senders(d)).unwrap_or_default();
                if !device_ids.is_empty() || unidentified > 0 {
                    tracing::info!("[lounge:{}] loungeStatus has remote senders, sending readiness", session.theme);
                    // Readiness: the theme becomes the active one and a playing
                    // song is reported. The connect adds at most an
                    // unidentified sender; SendersListed below sets who is
                    // connected, so repeated lists leave no ghosts.
                    let device_ids = device_ids.clone();
                    send_cmd(player_tx, PlayerCommand::RemoteConnected { theme: session.theme.clone(), device_ids, name: None });
                    responses.push(
                        OutgoingMessage::new("onHasPreviousNextChanged")
                            .with_arg("hasPrevious", "false")
                            .with_arg("hasNext", "false"),
                    );
                    responses.push(
                        OutgoingMessage::new("nowPlaying"),
                    );
                    responses.push(
                        OutgoingMessage::new("onAutoplayModeChanged")
                            .with_arg("autoplayMode", "UNSUPPORTED"),
                    );
                }
                // The list is who is connected now: a sender whose
                // remoteDisconnected was missed is gone. No list, no news;
                // nobody in a (re)bind's list was sent first, as SendersForgotten,
                // and again if a sender was counted since.
                let theme = session.theme.clone();
                if from_bind && lists_nobody(msg) {
                    if sender_event_seen {
                        send_cmd(player_tx, PlayerCommand::SendersForgotten { theme });
                        sender_event_seen = false;
                    }
                } else if msg.args.contains_key("devices") {
                    sender_event_seen |= !device_ids.is_empty() || unidentified > 0;
                    send_cmd(player_tx, PlayerCommand::SendersListed { theme, device_ids, unidentified });
                }
            }

            // -- Playback commands --
            "setPlaylist" => {
                log_cast_keys(&session.theme, "setPlaylist", &msg.args);
                dispatch_set_playlist(&msg.args, player_tx, from_bind);
            }
            "setVideo" => {
                log_cast_keys(&session.theme, "setVideo", &msg.args);
                dispatch_set_video(&msg.args, player_tx, from_bind);
            }
            "play" => {
                send_cmd(player_tx, PlayerCommand::Play);
            }
            "pause" => {
                send_cmd(player_tx, PlayerCommand::Pause);
            }
            "seekTo" => {
                if let Some(pos) = msg.args.get("newTime").and_then(|s| s.parse::<f64>().ok()) {
                    send_cmd(player_tx, PlayerCommand::SeekTo { position: pos });
                }
            }
            "setVolume" => {
                if let Some(vol) = msg.args.get("volume").and_then(|s| s.parse::<u32>().ok()) {
                    send_cmd(player_tx, PlayerCommand::SetVolume { volume: vol });
                }
            }
            "getVolume" => {
                send_cmd(player_tx, PlayerCommand::GetVolume);
            }
            "getNowPlaying" => {
                send_cmd(player_tx, PlayerCommand::GetNowPlaying);
            }
            "getPlaylist" => {
                send_cmd(player_tx, PlayerCommand::GetPlaylist);
            }
            "updatePlaylist" => {
                log_cast_keys(&session.theme, "updatePlaylist", &msg.args);
                dispatch_update_playlist(&msg.args, player_tx);
            }
            "stopVideo" => {
                send_cmd(player_tx, PlayerCommand::Stop);
            }
            "previous" => {
                send_cmd(player_tx, PlayerCommand::Previous);
            }
            "next" => {
                send_cmd(player_tx, PlayerCommand::Next);
            }

            "forceDisconnect" => {
                let reason = msg.args.get("reason").map(String::as_str).unwrap_or("unknown");
                tracing::warn!("forceDisconnect: {reason}");
            }

            other => {
                tracing::debug!("unhandled lounge command: {other} keys={:?}", arg_keys(&msg.args));
            }
        }
    }

    responses
}

/// The disconnectBehavior with which the user ends the cast on the
/// receiver (switches it back to the phone).
const DISCONNECTED_BY_USER: &str = "MDX_SESSION_DISCONNECT_BEHAVIOR_DISCONNECTED_BY_USER";

/// What a loungeStatus's connectionEventDetails (a JSON object in a string)
/// says about a sender's leave.
struct LeaveDetails {
    device_id: Option<String>,
    behavior: Option<String>,
}

/// None if the status has no details or they are no JSON object.
fn leave_details(args: &HashMap<String, String>) -> Option<LeaveDetails> {
    let Ok(serde_json::Value::Object(details)) = serde_json::from_str(args.get("connectionEventDetails")?) else {
        return None;
    };
    let text = |key: &str| details.get(key).and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(str::to_owned);
    Some(LeaveDetails { device_id: text("deviceId"), behavior: text("disconnectBehavior") })
}

/// Whether a loungeStatus in `messages` says the user ended a cast on this
/// receiver: its details say DISCONNECTED_BY_USER and the device they name
/// is not one still listed (another device's leave). With nobody listed any
/// more, or no list, no deviceId is needed; with senders still listed and no
/// deviceId, who left is unknown and nothing stops. Then: the details'
/// deviceId, the batch's leaving senders and whether nobody is listed any
/// more (no list counts as nobody); the player checks who cast.
fn ended_by_user(theme: &str, messages: &[IncomingMessage]) -> Option<(Option<String>, Vec<String>, bool)> {
    let leaving_ids: Vec<String> = messages
        .iter()
        .filter(|msg| msg.command == "remoteDisconnected")
        .filter_map(|msg| sender_id(&msg.args))
        .collect();
    messages.iter().filter(|msg| msg.command == "loungeStatus").find_map(|msg| {
        let details = leave_details(&msg.args).filter(|d| d.behavior.as_deref() == Some(DISCONNECTED_BY_USER))?;
        let matched = match &details.device_id {
            _ if leaving_ids.is_empty() => "no leave in the batch",
            Some(id) if leaving_ids.contains(id) => "yes",
            Some(_) => "no",
            None => "no deviceId",
        };
        tracing::debug!("[lounge:{theme}] disconnected by the user; deviceId matches the leaving sender: {matched}");
        let still_listed = msg.args.get("devices").map(|d| remote_senders(d));
        let nobody_left = still_listed.as_ref().is_none_or(|(ids, unidentified)| ids.is_empty() && *unidentified == 0);
        let ended = match (&still_listed, &details.device_id) {
            _ if nobody_left => true,
            (Some((ids, _)), Some(id)) => !ids.contains(id),
            _ => false,
        };
        ended.then(|| (details.device_id, leaving_ids.clone(), nobody_left))
    })
}

/// An enum value from YouTube as it can be logged: only an enum-like name.
fn loggable(value: &str) -> std::borrow::Cow<'_, str> {
    if value.len() <= 96 && value.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
        value.into()
    } else {
        format!("(not an enum name, len {})", value.len()).into()
    }
}

/// A loungeStatus whose device list names no REMOTE_CONTROL sender.
fn lists_nobody(msg: &IncomingMessage) -> bool {
    msg.command == "loungeStatus"
        && msg.args.get("devices").is_some_and(|d| remote_senders(d) == (Vec::new(), 0))
}

/// Shows what the phone sends with each cast (keys only, see `arg_keys`).
fn log_cast_keys(theme: &str, command: &str, args: &HashMap<String, String>) {
    tracing::info!("[lounge:{}] {} keys={:?}", theme, command, arg_keys(args));
}

/// A remoteConnected / remoteDisconnected sender's device id, if it gave one.
fn sender_id(args: &HashMap<String, String>) -> Option<String> {
    args.get("id").filter(|id| !id.is_empty()).cloned()
}

/// The REMOTE_CONTROL senders in a loungeStatus `devices` list (a JSON
/// array): the readable ids, and how many have none. A list that does not
/// parse but names a REMOTE_CONTROL counts one sender without an id.
fn remote_senders(devices: &str) -> (Vec<String>, usize) {
    let Ok(serde_json::Value::Array(devices)) = serde_json::from_str(devices) else {
        return (Vec::new(), usize::from(devices.contains("REMOTE_CONTROL")));
    };
    let remotes: Vec<_> = devices.iter().filter(|d| d["type"] == "REMOTE_CONTROL").collect();
    let ids: Vec<String> = remotes
        .iter()
        .filter_map(|d| d["id"].as_str().filter(|id| !id.is_empty()).map(str::to_owned))
        .collect();
    let unidentified = remotes.len() - ids.len();
    (ids, unidentified)
}

/// Command args can carry credentials (e.g. `ctt`), so logs get keys only.
fn arg_keys(args: &HashMap<String, String>) -> Vec<&str> {
    let mut keys: Vec<&str> = args.keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

fn dispatch_set_playlist(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>, from_bind: bool) {
    let video_ids: Vec<String> = args
        .get("videoIds")
        .or_else(|| args.get("videoId"))
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();

    if video_ids.is_empty() {
        tracing::warn!("setPlaylist with no video IDs: keys={:?}", arg_keys(args));
        return;
    }

    let index: usize = args
        .get("currentIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        .min(video_ids.len().saturating_sub(1));

    let video_id = args
        .get("videoId")
        .cloned()
        .unwrap_or_else(|| video_ids.get(index).cloned().unwrap_or_default());

    let current_time: f64 = args
        .get("currentTime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let list_id = args.get("listId").cloned().unwrap_or_default();
    let ctt = args.get("ctt").cloned();
    let params = args.get("params").cloned();

    send_cmd(player_tx, PlayerCommand::SetPlaylist {
        video_id,
        video_ids,
        index,
        current_time,
        list_id,
        ctt,
        params,
        from_bind,
    });
}

fn dispatch_set_video(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>, from_bind: bool) {
    let video_id = match args.get("videoId") {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            tracing::warn!("setVideo with no videoId: keys={:?}", arg_keys(args));
            return;
        }
    };

    let ctt = args.get("ctt").cloned();

    let current_time: f64 = args
        .get("currentTime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    send_cmd(player_tx, PlayerCommand::SetVideo {
        video_id,
        ctt,
        current_time,
        from_bind,
    });
}

fn dispatch_update_playlist(args: &HashMap<String, String>, player_tx: &mpsc::Sender<PlayerCommand>) {
    let video_ids: Vec<String> = args
        .get("videoIds")
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();

    if video_ids.is_empty() {
        tracing::warn!("updatePlaylist with no video IDs: keys={:?}", arg_keys(args));
        return;
    }

    let list_id = args.get("listId").cloned().unwrap_or_default();
    let current_index: Option<usize> = args
        .get("currentIndex")
        .and_then(|s| s.parse().ok());

    tracing::info!(
        "[lounge] updatePlaylist: {} videos, currentIndex={:?}",
        video_ids.len(),
        current_index
    );

    send_cmd(player_tx, PlayerCommand::UpdatePlaylist {
        video_ids,
        list_id,
        current_index,
    });
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Lounge protocol loop.
///
/// This is the primary entry point for the YouTube Lounge integration. It:
/// 1. Obtains a lounge token
/// 2. Performs the initial bind to establish a session
/// 3. Runs a receive loop (long-polling) and a send loop concurrently
/// 4. Dispatches incoming commands to `player_cmd_tx`
/// 5. Forwards state updates from `state_rx` to YouTube
/// 6. Handles pairing codes from `pairing_rx`
/// 7. Reconnects automatically on transient errors with exponential backoff
///
/// Returns `Ok(())` only when `shutdown` is signaled.
pub async fn run_lounge(
    client: reqwest::Client,
    device_name: String,
    mut identity: watch::Receiver<crate::identity::Identity>,
    theme: String,
    player_cmd_tx: mpsc::Sender<PlayerCommand>,
    state_rx: mpsc::Receiver<OutgoingMessage>,
    pairing_rx: mpsc::Receiver<String>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let current = identity.borrow_and_update().lounge(&theme);
    let mut session = LoungeSession::new(client.clone(), device_name, current.uuid, current.screen_id, theme);
    let theme_tag = format!("[lounge:{}]", session.theme);
    let mut state_rx = state_rx;
    let mut shutdown = shutdown;

    // Spawn a dedicated task for pairing code registration so it never
    // interrupts the long-poll. This runs independently from the session loop.
    let pair_client = client;
    let pair_name = session.device_name.clone();
    let pair_identity = identity.clone();
    let pair_theme = session.theme.clone();
    tokio::spawn(async move {
        let mut pairing_rx = pairing_rx;
        while let Some(code) = pairing_rx.recv().await {
            // The identity at the time of the code: it can change.
            let current = pair_identity.borrow().lounge(&pair_theme);
            if let Err(e) = register_pairing_code(
                &pair_client,
                &current.screen_id,
                &pair_name,
                &current.uuid,
                &code,
            ).await {
                tracing::error!("failed to register pairing code: {e}");
            }
        }
    });

    // Outer loop: handles full reconnection (token refresh + re-bind)
    loop {
        if *shutdown.borrow() {
            return Err(LoungeError::Shutdown);
        }

        // A new identity (see below) is registered with a new token and bind.
        let current = identity.borrow_and_update().lounge(&session.theme);
        session.uuid = current.uuid;
        session.screen_id = current.screen_id;

        // -- Get lounge token --
        tracing::info!("{} obtaining lounge token", theme_tag);
        match get_lounge_token(&session.client, &session.screen_id).await {
            Ok(token) => {
                session.lounge_token = token;
                tracing::info!("{} lounge token acquired", theme_tag);
            }
            Err(e) => {
                tracing::error!("failed to get lounge token: {e}");
                backoff_sleep(1, &mut shutdown).await?;
                continue;
            }
        }

        // -- Initial bind --
        session.aid = -1;
        session.ofs = 0;
        session.sid.clear();
        session.gsessionid.clear();
        session.rid = rand::thread_rng().gen_range(10_000..100_000);

        let initial_messages = match session.initial_bind().await {
            Ok(msgs) => msgs,
            Err(LoungeError::Gone) => {
                tracing::warn!("session gone during initial bind, refreshing token");
                continue;
            }
            Err(e) => {
                tracing::error!("initial bind failed: {e}");
                backoff_sleep(1, &mut shutdown).await?;
                continue;
            }
        };

        // Dispatch any commands from the initial bind response
        let responses = dispatch_bind_response(&mut session, &initial_messages, &player_cmd_tx);
        if !responses.is_empty() {
            if let Err(e) = session.send_messages(&responses).await {
                tracing::error!("failed to send initial responses: {e}");
            }
        }

        // -- Session loop: streaming long-poll + state send --
        //
        // `start_streaming_poll()` pushes messages through a channel as they
        // arrive from the HTTP stream. The future only completes when the
        // connection closes or errors. A fresh channel is created for each
        // poll connection to avoid stale messages after reconnects.
        let mut retry_count: u32 = 0;

        'session: loop {
            if *shutdown.borrow() {
                return Err(LoungeError::Shutdown);
            }

            // Fresh channel per poll connection
            let (poll_msg_tx, mut poll_msg_rx) = mpsc::channel::<Vec<IncomingMessage>>(16);

            let poll_fut = session.start_streaming_poll(poll_msg_tx);
            tokio::pin!(poll_fut);

            // Inner loop: process messages while the streaming connection is alive
            let poll_result = 'poll: loop {
                tokio::select! {
                    biased;

                    // Shutdown signal (highest priority)
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return Err(LoungeError::Shutdown);
                        }
                    }

                    // A new receiver identity: register its screen afresh.
                    Ok(()) = identity.changed() => {
                        tracing::info!("{} receiver identity changed; binding the new screen", theme_tag);
                        break 'session;
                    }

                    // Incoming messages from the streaming poll
                    Some(messages) = poll_msg_rx.recv() => {
                        retry_count = 0;
                        let resps = dispatch_messages(&mut session, &messages, &player_cmd_tx, Delivery::Poll);
                        if !resps.is_empty() {
                            if let Err(e) = session.send_messages(&resps).await {
                                tracing::error!("failed to send responses: {e}");
                            }
                        }
                    }

                    // Outgoing state update from player
                    Some(msg) = state_rx.recv() => {
                        let mut batch = vec![msg];
                        while let Ok(extra) = state_rx.try_recv() {
                            batch.push(extra);
                        }

                        tracing::info!(
                            "[lounge:{}] sending {} state messages: [{}]",
                            session.theme,
                            batch.len(),
                            batch.iter().map(|m| m.command.as_str()).collect::<Vec<_>>().join(", ")
                        );

                        match session.send_messages(&batch).await {
                            Ok(()) => {
                                retry_count = 0;
                                tracing::info!("[lounge:{}] state messages sent OK", session.theme);
                            }
                            Err(LoungeError::UnknownSid) => {
                                tracing::warn!("unknown SID on send, reconnecting");
                                match session.reconnect_bind().await {
                                    Ok(msgs) => {
                                        let resps = dispatch_bind_response(&mut session, &msgs, &player_cmd_tx);
                                        if !resps.is_empty() {
                                            let _ = session.send_messages(&resps).await;
                                        }
                                        retry_count = 0;
                                        // Restart poll with new SID
                                        break 'poll Ok(());
                                    }
                                    Err(LoungeError::Gone) => break 'session,
                                    Err(e) => {
                                        tracing::error!("reconnect failed: {e}");
                                        break 'session;
                                    }
                                }
                            }
                            Err(LoungeError::Gone) => break 'session,
                            Err(e) => {
                                tracing::error!("send error: {e}");
                                retry_count += 1;
                                if retry_count >= MAX_RETRIES {
                                    tracing::error!("max retries exceeded, full reconnect");
                                    break 'session;
                                }
                                backoff_sleep(retry_count, &mut shutdown).await?;
                            }
                        }
                    }

                    // Streaming poll completed (connection closed or error)
                    result = &mut poll_fut => {
                        break 'poll result;
                    }
                }
            };

            // Handle why the poll connection ended
            match poll_result {
                Ok(()) => {
                    // Normal close or idle timeout — restart immediately
                }
                Err(LoungeError::UnknownSid) => {
                    tracing::warn!("unknown SID on poll, reconnecting");
                    match session.reconnect_bind().await {
                        Ok(msgs) => {
                            let resps = dispatch_bind_response(&mut session, &msgs, &player_cmd_tx);
                            if !resps.is_empty() {
                                let _ = session.send_messages(&resps).await;
                            }
                            retry_count = 0;
                        }
                        Err(LoungeError::Gone) => break 'session,
                        Err(e) => {
                            tracing::error!("reconnect failed: {e}");
                            break 'session;
                        }
                    }
                }
                Err(LoungeError::Gone) => {
                    tracing::warn!("session gone (410), refreshing token");
                    break 'session;
                }
                Err(LoungeError::Http(ref e)) if e.is_timeout() => {
                    tracing::trace!("poll timeout, restarting");
                }
                Err(LoungeError::Http(ref e)) if e.is_connect() || is_io_error(e) => {
                    retry_count += 1;
                    tracing::warn!("poll network error (attempt {retry_count}): {e}");
                    if retry_count >= MAX_RETRIES {
                        tracing::error!("max retries exceeded, full reconnect");
                        break 'session;
                    }
                    backoff_sleep(retry_count, &mut shutdown).await?;
                }
                Err(e) => {
                    retry_count += 1;
                    tracing::error!("poll error (attempt {retry_count}): {e}");
                    if retry_count >= MAX_RETRIES {
                        tracing::error!("max retries exceeded, full reconnect");
                        break 'session;
                    }
                    backoff_sleep(retry_count, &mut shutdown).await?;
                }
            }
        }

        // If we break out of the session loop, wait briefly and restart
        tracing::info!("session ended, will reconnect");
        backoff_sleep(1, &mut shutdown).await?;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a reqwest error is caused by an underlying I/O error (connection
/// reset, broken pipe, etc.).
fn is_io_error(e: &reqwest::Error) -> bool {
    let mut source = e.source();
    while let Some(err) = source {
        if err.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        source = err.source();
    }
    false
}

/// Exponential backoff sleep: `BACKOFF_BASE_MS * 2^(attempt-1)`, clamped.
/// Returns `Err(Shutdown)` if the shutdown signal fires during the wait.
async fn backoff_sleep(attempt: u32, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
    // 500ms, 1s, 2s, 4s, 8s, 16s, ... capped at ~30s
    let delay_ms = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(6));
    let delay = Duration::from_millis(delay_ms);
    tracing::debug!("backoff sleep: {delay_ms}ms (attempt {attempt})");

    tokio::select! {
        _ = time::sleep(delay) => Ok(()),
        _ = shutdown.changed() => {
            if *shutdown.borrow() {
                Err(LoungeError::Shutdown)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_errors_do_not_carry_query_secrets() {
        let logs = crate::storage::test_util::LogCapture::start();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);

        let url = format!(
            "http://{addr}/api/lounge/bc/bind?SID=secret-sid&gsessionid=secret-gs&loungeIdToken=secret-tok"
        );
        let err: LoungeError = reqwest::Client::new().get(&url).send().await.unwrap_err().into();
        tracing::error!("lounge request failed: {err}");
        tracing::error!("lounge request failed: {err:?}");

        assert!(err.to_string().starts_with("lounge http:"), "{err}");
        for text in [err.to_string(), format!("{err:?}"), logs.text()] {
            for secret in ["secret-sid", "secret-gs", "secret-tok"] {
                assert!(!text.contains(secret), "{secret} leaked: {text}");
            }
        }
    }

    fn incoming(command: &str, args: &[(&str, &str)]) -> IncomingMessage {
        IncomingMessage {
            index: 0,
            command: command.to_owned(),
            args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn connected_senders_hand_over_their_device_ids() {
        let mut session = LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "m".to_owned(),
        );
        let (player_tx, mut player_rx) = mpsc::channel(8);
        let devices = r#"[{"app":"lb-v4","id":"screen-self","type":"LOUNGE_SCREEN"},
                          {"app":"android-ytm","id":"sender-device-1","name":"Phone","type":"REMOTE_CONTROL"}]"#;
        let messages = [
            incoming("remoteConnected", &[("id", "sender-device-2"), ("name", "Phone")]),
            incoming("loungeStatus", &[("devices", devices)]),
            incoming("remoteConnected", &[("name", "Phone")]),
        ];

        dispatch_messages(&mut session, &messages, &player_tx, Delivery::Poll);

        let mut ids = Vec::new();
        while let Ok(cmd) = player_rx.try_recv() {
            if let PlayerCommand::RemoteConnected { device_ids, .. } = cmd {
                ids.push(device_ids);
            }
        }
        assert_eq!(
            ids,
            vec![vec!["sender-device-2".to_owned()], vec!["sender-device-1".to_owned()], vec![]]
        );
    }

    #[test]
    fn senders_are_named_when_they_connect_and_leave() {
        let mut session = LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "m".to_owned(),
        );
        let (player_tx, mut player_rx) = mpsc::channel(8);
        let messages = [
            incoming("remoteConnected", &[("id", "sender-device-2"), ("name", "Phone")]),
            incoming("remoteDisconnected", &[("id", "sender-device-2"), ("name", "Phone")]),
            incoming("remoteDisconnected", &[("id", ""), ("name", "iPad")]),
            incoming("remoteDisconnected", &[]),
        ];

        dispatch_messages(&mut session, &messages, &player_tx, Delivery::Poll);

        let mut seen = Vec::new();
        while let Ok(cmd) = player_rx.try_recv() {
            match cmd {
                PlayerCommand::RemoteConnected { name, .. } => seen.push((None, name)),
                PlayerCommand::RemoteDisconnected { device_id, name, .. } => seen.push((device_id, name)),
                _ => {}
            }
        }
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(
            seen,
            vec![
                (None, some("Phone")),
                (some("sender-device-2"), some("Phone")),
                (None, some("iPad")),
                (None, None),
            ]
        );
    }

    #[test]
    fn every_lounge_status_with_a_device_list_lists_the_senders() {
        let mut session = LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "m".to_owned(),
        );
        let (player_tx, mut player_rx) = mpsc::channel(8);
        let phone = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"},{"id":"phone","type":"REMOTE_CONTROL"}]"#;
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let messages = [
            incoming("loungeStatus", &[("devices", phone)]),
            incoming("loungeStatus", &[("devices", nobody)]),
            incoming("loungeStatus", &[("queueId", "q")]),
        ];

        dispatch_messages(&mut session, &messages, &player_tx, Delivery::Poll);

        let mut listed = Vec::new();
        let mut connects = 0;
        while let Ok(cmd) = player_rx.try_recv() {
            match cmd {
                PlayerCommand::SendersListed { device_ids, unidentified, .. } => listed.push((device_ids, unidentified)),
                PlayerCommand::RemoteConnected { .. } => {
                    connects += 1;
                    assert!(listed.is_empty(), "the connect comes before its list");
                }
                _ => {}
            }
        }
        assert_eq!(connects, 1);
        assert_eq!(listed, vec![(vec!["phone".to_owned()], 0), (vec![], 0)], "no list, no news");
    }

    fn test_session() -> LoungeSession {
        LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "m".to_owned(),
        )
    }

    /// The commands a dispatch of `messages` sends to the player.
    fn commands_for(messages: &[IncomingMessage], delivery: Delivery) -> Vec<PlayerCommand> {
        let (player_tx, mut player_rx) = mpsc::channel(16);
        dispatch_messages(&mut test_session(), messages, &player_tx, delivery);
        let mut commands = Vec::new();
        while let Ok(cmd) = player_rx.try_recv() {
            commands.push(cmd);
        }
        commands
    }

    #[test]
    fn a_bind_response_listing_nobody_forgets_the_connected_senders_quietly() {
        let phone = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"},{"id":"phone","type":"REMOTE_CONTROL"}]"#;
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let listed = |commands: Vec<PlayerCommand>| -> Vec<(Vec<String>, usize)> {
            commands
                .into_iter()
                .filter_map(|cmd| match cmd {
                    PlayerCommand::SendersListed { device_ids, unidentified, .. } => Some((device_ids, unidentified)),
                    _ => None,
                })
                .collect()
        };
        let status = |devices| [incoming("loungeStatus", &[("devices", devices)])];
        let forgotten = |commands: &[PlayerCommand]| {
            commands.iter().filter(|c| matches!(c, PlayerCommand::SendersForgotten { theme } if theme == "m")).count()
        };
        // The receiver's own (re)bind may not list a phone that is still
        // there: no last sender gone, only the connected set forgotten.
        let bind = commands_for(&status(nobody), Delivery::Bind);
        assert_eq!(forgotten(&bind), 1);
        assert!(listed(bind).is_empty());
        let poll = commands_for(&status(nobody), Delivery::Poll);
        assert_eq!(forgotten(&poll), 0);
        assert_eq!(listed(poll), vec![(vec![], 0)]);
        let named = commands_for(&status(phone), Delivery::Bind);
        assert_eq!(forgotten(&named), 0);
        assert_eq!(listed(named), vec![(vec!["phone".to_owned()], 0)]);
    }

    #[test]
    fn a_bind_response_is_dispatched_as_a_bind() {
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let (player_tx, mut player_rx) = mpsc::channel(16);
        dispatch_bind_response(&mut test_session(), &[incoming("loungeStatus", &[("devices", nobody)])], &player_tx);
        let cmd = player_rx.try_recv().expect("a command");
        assert!(matches!(cmd, PlayerCommand::SendersForgotten { .. }), "{cmd:?}");
        assert!(player_rx.try_recv().is_err());
    }

    fn bind_commands(messages: &[IncomingMessage]) -> Vec<PlayerCommand> {
        let (player_tx, mut player_rx) = mpsc::channel(16);
        dispatch_bind_response(&mut test_session(), messages, &player_tx);
        let mut commands = Vec::new();
        while let Ok(cmd) = player_rx.try_recv() {
            commands.push(cmd);
        }
        commands
    }

    #[test]
    fn a_bind_response_listing_nobody_forgets_the_senders_first_and_tags_its_casts() {
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let status = incoming("loungeStatus", &[("devices", nobody)]);
        let set_playlist = incoming("setPlaylist", &[("videoId", "ddddddddddd"), ("videoIds", "ddddddddddd"), ("ctt", "old")]);
        let set_video = incoming("setVideo", &[("videoId", "ddddddddddd"), ("ctt", "old")]);
        // Wherever the list stands in the response, the casts in it are
        // judged with nobody known.
        for replayed in [
            [status.clone(), set_playlist.clone(), set_video.clone()],
            [set_playlist.clone(), set_video.clone(), status.clone()],
        ] {
            let commands = bind_commands(&replayed);
            assert!(matches!(commands.as_slice(), [
                PlayerCommand::SendersForgotten { .. },
                PlayerCommand::SetPlaylist { from_bind: true, .. },
                PlayerCommand::SetVideo { from_bind: true, .. },
            ]), "{commands:?}");
        }

        let polled = commands_for(&[status, set_playlist, set_video], Delivery::Poll);
        assert!(polled.iter().any(|c| matches!(c, PlayerCommand::SetPlaylist { from_bind: false, .. })));
        assert!(polled.iter().any(|c| matches!(c, PlayerCommand::SetVideo { from_bind: false, .. })));
    }

    #[test]
    fn a_sender_event_later_in_the_bind_response_comes_after_the_forgetting() {
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let phone = r#"[{"id":"phone","type":"REMOTE_CONTROL"}]"#;
        let cast = incoming("setPlaylist", &[("videoId", "ddddddddddd"), ("videoIds", "ddddddddddd"), ("ctt", "new")]);
        let commands = bind_commands(&[
            incoming("loungeStatus", &[("devices", nobody)]),
            incoming("remoteConnected", &[("id", "phone"), ("name", "Phone")]),
            cast.clone(),
        ]);
        assert!(matches!(commands.as_slice(), [
            PlayerCommand::SendersForgotten { .. },
            PlayerCommand::RemoteConnected { device_ids, .. },
            PlayerCommand::SetPlaylist { from_bind: true, .. },
        ] if device_ids == &["phone".to_owned()]), "{commands:?}");

        // The later event is a list naming the phone.
        let commands = bind_commands(&[
            incoming("loungeStatus", &[("devices", nobody)]),
            incoming("loungeStatus", &[("devices", phone)]),
            cast,
        ]);
        assert!(matches!(commands.as_slice(), [
            PlayerCommand::SendersForgotten { .. },
            PlayerCommand::RemoteConnected { device_ids: connected, .. },
            PlayerCommand::SendersListed { device_ids: listed, unidentified: 0, .. },
            PlayerCommand::SetPlaylist { from_bind: true, .. },
        ] if connected == &["phone".to_owned()] && listed == &["phone".to_owned()]), "{commands:?}");
    }

    #[test]
    fn a_later_list_of_nobody_in_the_bind_response_forgets_a_sender_counted_before_it() {
        let nobody = r#"[{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let x = r#"[{"id":"x","type":"REMOTE_CONTROL"}]"#;
        let commands = bind_commands(&[
            incoming("remoteConnected", &[("id", "x"), ("name", "X")]),
            incoming("loungeStatus", &[("devices", nobody)]),
        ]);
        assert!(matches!(commands.as_slice(), [
            PlayerCommand::SendersForgotten { .. },
            PlayerCommand::RemoteConnected { device_ids, .. },
            PlayerCommand::SendersForgotten { .. },
        ] if device_ids == &["x".to_owned()]), "{commands:?}");

        let commands = bind_commands(&[
            incoming("loungeStatus", &[("devices", x)]),
            incoming("loungeStatus", &[("devices", nobody)]),
        ]);
        assert!(matches!(commands.as_slice(), [
            PlayerCommand::SendersForgotten { .. },
            PlayerCommand::RemoteConnected { device_ids: connected, .. },
            PlayerCommand::SendersListed { device_ids: listed, .. },
            PlayerCommand::SendersForgotten { .. },
        ] if connected == &["x".to_owned()] && listed == &["x".to_owned()]), "{commands:?}");

        // Without a sender event before it, the list of nobody is not sent twice.
        let commands = bind_commands(&[
            incoming("loungeStatus", &[("devices", nobody)]),
            incoming("loungeStatus", &[("devices", nobody)]),
        ]);
        assert!(matches!(commands.as_slice(), [PlayerCommand::SendersForgotten { .. }]), "{commands:?}");
    }

    #[test]
    fn a_lounge_status_counts_senders_without_a_readable_id_and_connects_all_it_lists() {
        let mixed = r#"[{"id":"phone","type":"REMOTE_CONTROL"},{"name":"iPad","type":"REMOTE_CONTROL"},
                        {"id":"","type":"REMOTE_CONTROL"},{"id":"screen-self","type":"LOUNGE_SCREEN"}]"#;
        let commands = commands_for(&[incoming("loungeStatus", &[("devices", mixed)])], Delivery::Poll);
        let connects: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::RemoteConnected { device_ids, .. } => Some(device_ids.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(connects, vec![vec!["phone".to_owned()]], "one connect for the list, with its readable ids");
        assert!(commands.iter().any(|c| matches!(c,
            PlayerCommand::SendersListed { device_ids, unidentified: 2, .. } if device_ids == &["phone".to_owned()])));

        // No readable id at all: still a connect (theme switch, the playing
        // song reported), without ids; the list right after counts them.
        for devices in [r#"[{"type":"REMOTE_CONTROL"}]"#, "REMOTE_CONTROL"] {
            let commands = commands_for(&[incoming("loungeStatus", &[("devices", devices)])], Delivery::Poll);
            assert!(matches!(&commands[0],
                PlayerCommand::RemoteConnected { device_ids, name: None, .. } if device_ids.is_empty()), "{devices}");
            assert!(commands.iter().any(|c| matches!(c,
                PlayerCommand::SendersListed { device_ids, unidentified: 1, .. } if device_ids.is_empty())), "{devices}");
        }
    }

    const BY_USER: &str = "MDX_SESSION_DISCONNECT_BEHAVIOR_DISCONNECTED_BY_USER";

    // Lounge device ids have the 36-character UUID form in both places.
    const IPAD: &str = "a1b2c3d4-0000-1111-2222-333344445555";
    const PHONE: &str = "f0e1d2c3-9999-8888-7777-666655554444";

    /// A loungeStatus listing `remotes` (as REMOTE_CONTROL senders), with `details`.
    fn status_with(remotes: &[&str], details: &str) -> IncomingMessage {
        let mut devices = vec![r#"{"id":"screen-self","type":"LOUNGE_SCREEN"}"#.to_owned()];
        devices.extend(remotes.iter().map(|id| format!(r#"{{"id":"{id}","type":"REMOTE_CONTROL"}}"#)));
        incoming(
            "loungeStatus",
            &[("devices", &format!("[{}]", devices.join(","))), ("connectionEventDetails", details)],
        )
    }

    fn by_user_for(id: &str) -> String {
        format!(r#"{{"deviceId":"{id}","disconnectBehavior":"{BY_USER}"}}"#)
    }

    fn ipad_leaves() -> IncomingMessage {
        incoming("remoteDisconnected", &[("id", IPAD), ("name", "iPad")])
    }

    /// Where in the dispatch of `messages` the player is told to stop, if at all.
    fn stop_at(messages: &[IncomingMessage], delivery: Delivery) -> Option<usize> {
        let commands = commands_for(messages, delivery);
        let stops: Vec<usize> = commands
            .iter()
            .enumerate()
            .filter(|(_, cmd)| matches!(cmd, PlayerCommand::StoppedByUser { theme, .. } if theme == "m"))
            .map(|(at, _)| at)
            .collect();
        assert!(stops.len() <= 1, "{commands:?}");
        stops.first().copied()
    }

    #[test]
    fn the_user_ending_the_cast_stops_it_before_the_batchs_other_commands() {
        // As live: the status (the iPad no longer listed) before the iPad's leave.
        let live = [status_with(&[], &by_user_for(IPAD)), ipad_leaves()];
        assert_eq!(stop_at(&live, Delivery::Poll), Some(0));
        let reversed = [ipad_leaves(), status_with(&[], &by_user_for(IPAD))];
        assert_eq!(stop_at(&reversed, Delivery::Poll), Some(0));
        // Nobody listed any more: no deviceId needed, nor a leave in the batch.
        let details = format!(r#"{{"disconnectBehavior":"{BY_USER}"}}"#);
        assert_eq!(stop_at(&[status_with(&[], &details)], Delivery::Poll), Some(0));
        // The leaving device is not among those still listed.
        assert_eq!(stop_at(&[status_with(&[PHONE], &by_user_for(IPAD)), ipad_leaves()], Delivery::Poll), Some(0));
        // A status without a device list.
        let unlisted = incoming("loungeStatus", &[("connectionEventDetails", &by_user_for(IPAD))]);
        assert_eq!(stop_at(&[unlisted], Delivery::Poll), Some(0));
    }

    /// What the dispatch of `messages` reports the user's end with: the
    /// details' device, the batch's leaving senders, whether nobody is left.
    fn stopped_by(messages: &[IncomingMessage]) -> Option<(Option<String>, Vec<String>, bool)> {
        commands_for(messages, Delivery::Poll).into_iter().find_map(|cmd| match cmd {
            PlayerCommand::StoppedByUser { device_id, leaving_ids, nobody_left, .. } => {
                Some((device_id, leaving_ids, nobody_left))
            }
            _ => None,
        })
    }

    #[test]
    fn the_users_end_names_the_devices_for_the_player_to_check() {
        let ipad = || Some(IPAD.to_owned());
        let leaving = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        assert_eq!(
            stopped_by(&[status_with(&[], &by_user_for(IPAD)), ipad_leaves()]),
            Some((ipad(), leaving(&[IPAD]), true))
        );
        assert_eq!(
            stopped_by(&[status_with(&[PHONE], &by_user_for(IPAD)), ipad_leaves()]),
            Some((ipad(), leaving(&[IPAD]), false))
        );
        // The details name the iPad while the phone leaves: both are reported.
        let phone_leaves = incoming("remoteDisconnected", &[("id", PHONE), ("name", "Phone")]);
        assert_eq!(
            stopped_by(&[status_with(&[], &by_user_for(IPAD)), phone_leaves]),
            Some((ipad(), leaving(&[PHONE]), true))
        );
        // Without a deviceId in the details: the leaving senders only.
        let details = format!(r#"{{"disconnectBehavior":"{BY_USER}"}}"#);
        assert_eq!(stopped_by(&[ipad_leaves(), status_with(&[], &details)]), Some((None, leaving(&[IPAD]), true)));
        assert_eq!(stopped_by(&[status_with(&[], &details)]), Some((None, vec![], true)));
        // Only a sender without a readable id still listed: someone is left.
        let unidentified = incoming(
            "loungeStatus",
            &[("devices", r#"[{"type":"REMOTE_CONTROL"}]"#), ("connectionEventDetails", &by_user_for(IPAD))],
        );
        assert_eq!(stopped_by(&[unidentified, ipad_leaves()]), Some((ipad(), leaving(&[IPAD]), false)));
        // A status without a device list counts as listing nobody.
        let unlisted = incoming("loungeStatus", &[("connectionEventDetails", &by_user_for(IPAD))]);
        assert_eq!(stopped_by(&[unlisted]), Some((ipad(), vec![], true)));
    }

    #[test]
    fn an_id_as_disconnect_behavior_is_logged_by_its_length() {
        let logs = crate::storage::test_util::LogCapture::start();
        commands_for(&[status_with(&[], r#"{"disconnectBehavior":"phone-id-1"}"#)], Delivery::Poll);
        let text = logs.text();
        assert!(text.contains("disconnectBehavior=(not an enum name, len 10)"), "{text}");
        assert!(!text.contains("phone-id-1"), "{text}");
    }

    #[test]
    fn other_leaves_do_not_stop_the_cast() {
        for (name, batch) in [
            ("no details", vec![status_with(&[], "{}"), ipad_leaves()]),
            ("no behavior", vec![status_with(&[], &format!(r#"{{"deviceId":"{IPAD}"}}"#)), ipad_leaves()]),
            (
                "another behavior",
                vec![
                    status_with(&[], &format!(r#"{{"deviceId":"{IPAD}","disconnectBehavior":"MDX_SESSION_DISCONNECT_BEHAVIOR_SOMETHING_ELSE"}}"#)),
                    ipad_leaves(),
                ],
            ),
            ("no JSON", vec![status_with(&[], "not json"), ipad_leaves()]),
            ("no JSON object", vec![status_with(&[], &format!(r#"["{BY_USER}"]"#)), ipad_leaves()]),
            ("a leave alone", vec![ipad_leaves()]),
            // The device the details name is still listed: another one left.
            ("named device still listed", vec![status_with(&[PHONE], &by_user_for(PHONE)), ipad_leaves()]),
            // Several still listed and no deviceId: who left is unknown.
            (
                "no deviceId, others listed",
                vec![status_with(&[PHONE], &format!(r#"{{"disconnectBehavior":"{BY_USER}"}}"#)), ipad_leaves()],
            ),
        ] {
            assert_eq!(stop_at(&batch, Delivery::Poll), None, "{name}");
        }
        // A bind response replays an earlier status: no news of a leave now.
        assert_eq!(stop_at(&[status_with(&[], &by_user_for(IPAD)), ipad_leaves()], Delivery::Bind), None);
    }

    #[test]
    fn whether_the_details_name_the_leaving_sender_is_logged_at_debug() {
        let logs = crate::storage::test_util::LogCapture::start();
        commands_for(&[status_with(&[], &by_user_for(IPAD)), ipad_leaves()], Delivery::Poll);
        commands_for(&[status_with(&[], &by_user_for(PHONE)), ipad_leaves()], Delivery::Poll);
        commands_for(&[status_with(&[], &by_user_for(IPAD))], Delivery::Poll);
        let text = logs.text();
        for matched in ["yes", "no", "no leave in the batch"] {
            assert!(text.contains(&format!("deviceId matches the leaving sender: {matched}")), "{matched}: {text}");
        }
        assert!(!text.contains(IPAD) && !text.contains(PHONE), "{text}");
    }

    #[test]
    fn only_enum_names_from_youtube_are_logged_as_they_are() {
        assert_eq!(loggable(BY_USER), BY_USER);
        assert_eq!(loggable("phone-id-1"), "(not an enum name, len 10)");
        assert_eq!(loggable(&"A".repeat(97)), "(not an enum name, len 97)");
    }

    #[test]
    fn the_disconnect_behavior_is_logged_without_ids() {
        let logs = crate::storage::test_util::LogCapture::start();
        let other = "MDX_SESSION_DISCONNECT_BEHAVIOR_SOMETHING_ELSE";
        commands_for(
            &[
                incoming("remoteDisconnected", &[("id", "phone-id-1"), ("name", "Phone")]),
                status_with(&[], &format!(r#"{{"deviceId":"phone-id-1","disconnectBehavior":"{other}"}}"#)),
            ],
            Delivery::Poll,
        );
        let text = logs.text();
        assert!(text.contains(&format!("disconnectBehavior={other}")), "{text}");
        assert!(!text.contains("phone-id-1"), "{text}");
    }

    #[test]
    fn lounge_status_devices_that_do_not_parse_give_no_ids() {
        assert_eq!(remote_senders("REMOTE_CONTROL"), (vec![], 1));
        assert_eq!(remote_senders(r#"{"type":"REMOTE_CONTROL"}"#), (vec![], 1));
        assert_eq!(remote_senders("[]"), (vec![], 0));
        assert_eq!(remote_senders("garbage"), (vec![], 0));
    }

    #[test]
    fn dispatch_logs_arg_keys_not_ctt_value() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut session = LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "cl".to_owned(),
        );
        let (player_tx, _player_rx) = mpsc::channel(8);
        let messages = [
            incoming("someFutureCommand", &[("ctt", "secret-ctt"), ("foo", "bar")]),
            incoming("setVideo", &[("ctt", "secret-ctt")]),
        ];

        dispatch_messages(&mut session, &messages, &player_tx, Delivery::Poll);

        let logs = logs.text();
        assert!(logs.contains("unhandled lounge command: someFutureCommand"), "{logs}");
        assert!(logs.contains("setVideo with no videoId"), "{logs}");
        assert!(logs.contains("\"ctt\""), "{logs}");
        assert!(!logs.contains("secret-ctt"), "{logs}");
    }

    #[test]
    fn cast_commands_log_their_arg_keys_at_info() {
        let logs = crate::storage::test_util::LogCapture::start();
        let mut session = LoungeSession::new(
            reqwest::Client::new(),
            "Test Screen".to_owned(),
            "uuid".to_owned(),
            "screen".to_owned(),
            "cl".to_owned(),
        );
        let (player_tx, _player_rx) = mpsc::channel(8);
        let messages = [
            incoming(
                "setPlaylist",
                &[("ctt", "secret-ctt"), ("videoId", "dQw4w9WgXcQ"), ("listId", "secret-list"), ("params", "secret-params")],
            ),
            incoming("setVideo", &[("ctt", "secret-ctt"), ("videoId", "dQw4w9WgXcQ")]),
            incoming("updatePlaylist", &[("videoIds", "dQw4w9WgXcQ"), ("listId", "secret-list")]),
        ];

        dispatch_messages(&mut session, &messages, &player_tx, Delivery::Poll);

        let logs = logs.text();
        for expected in [
            r#"setPlaylist keys=["ctt", "listId", "params", "videoId"]"#,
            r#"setVideo keys=["ctt", "videoId"]"#,
            r#"updatePlaylist keys=["listId", "videoIds"]"#,
        ] {
            let line = logs.lines().find(|l| l.contains(expected)).unwrap_or_else(|| panic!("{expected}: {logs}"));
            assert!(line.contains(" INFO "), "{line}");
        }
        for secret in ["secret-ctt", "secret-list", "secret-params"] {
            assert!(!logs.contains(secret), "{secret} leaked: {logs}");
        }
    }

    #[test]
    fn arg_keys_lists_keys_without_values() {
        let args = HashMap::from([
            ("videoId".to_owned(), "abc".to_owned()),
            ("ctt".to_owned(), "secret-ctt-value".to_owned()),
        ]);
        let logged = format!("{:?}", arg_keys(&args));
        assert_eq!(logged, r#"["ctt", "videoId"]"#);
        assert!(!logged.contains("secret-ctt-value"));
    }

    #[test]
    fn log_and_error_formats_do_not_include_secret_values() {
        let src = include_str!("lounge.rs").split("#[cfg(test)]").next().unwrap();
        let forbidden = [
            "pairing code {code}",
            "{resp_body}",
            "{body}\"",
            "SID={}",
            "gsession={}",
            "{args:?}",
            "{:?}\", msg.args",
            "in response: {data}",
            "POST body: {}",
            "for screen {}",
        ];
        for pattern in forbidden {
            assert!(!src.contains(pattern), "log/error format still leaks: {pattern}");
        }
    }
}
