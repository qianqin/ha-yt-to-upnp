use std::fmt;

pub type Result<T> = std::result::Result<T, MpdError>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
pub enum MpdError {
    Io(std::io::Error),
    /// MPD returned `ACK [error@pos] {command} message`
    Ack(String),
    /// Unexpected protocol response
    Protocol(String),
}

impl fmt::Display for MpdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MpdError::Io(e) => write!(f, "mpd io: {e}"),
            MpdError::Ack(msg) => write!(f, "mpd ack: {msg}"),
            MpdError::Protocol(msg) => write!(f, "mpd protocol: {msg}"),
        }
    }
}

impl std::error::Error for MpdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MpdError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MpdError {
    fn from(e: std::io::Error) -> Self {
        MpdError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MpdStatus {
    pub state: String,
    pub elapsed: Option<f64>,
    #[allow(dead_code)]
    pub duration: Option<f64>,
    pub volume: u32,
}

// ---------------------------------------------------------------------------
// Idle events + client-to-client messages
// ---------------------------------------------------------------------------

/// MPD client-to-client channel on which the speaker adapter talks to us.
pub const RELEASE_CHANNEL: &str = "ytcast";
/// Sent by the adapter when the speaker went to standby or another source:
/// the cast has to end.
pub const RELEASE_MESSAGE: &str = "release";
/// `message` wakes the idle connection when the adapter posts to a channel
/// this connection subscribed to; `mixer` when MPD's volume changes (the
/// adapter gives MPD the speaker's own volume).
pub const IDLE_COMMAND: &str = "idle player mixer message";

pub fn subscribe_command(channel: &str) -> String {
    format!("subscribe {channel}")
}

/// Subsystems reported by an `idle` response (`changed: <subsystem>`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct IdleChanges {
    pub player: bool,
    pub mixer: bool,
    pub message: bool,
}

pub fn parse_idle_changes(lines: &[String]) -> IdleChanges {
    let mut changes = IdleChanges::default();
    for line in lines {
        match line.strip_prefix("changed: ") {
            Some("player") => changes.player = true,
            Some("mixer") => changes.mixer = true,
            Some("message") => changes.message = true,
            _ => {}
        }
    }
    changes
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMessage {
    pub channel: String,
    pub message: String,
}

/// Parse a `readmessages` response: `channel: X` / `message: Y` pairs.
pub fn parse_channel_messages(lines: &[String]) -> Vec<ChannelMessage> {
    let mut out = Vec::new();
    let mut channel: Option<String> = None;
    for line in lines {
        if let Some(c) = line.strip_prefix("channel: ") {
            channel = Some(c.to_owned());
        } else if let Some(m) = line.strip_prefix("message: ") {
            if let Some(c) = channel.take() {
                out.push(ChannelMessage { channel: c, message: m.to_owned() });
            }
        }
    }
    out
}

/// True if the adapter asked us to release the speaker. Anything else on
/// any channel is logged and ignored.
pub fn contains_release(messages: &[ChannelMessage]) -> bool {
    let mut release = false;
    for m in messages {
        if m.channel == RELEASE_CHANNEL && m.message == RELEASE_MESSAGE {
            release = true;
        } else {
            tracing::debug!("[MPD idle] ignoring message {:?} on channel {:?}", m.message, m.channel);
        }
    }
    release
}

/// What the idle connection observed in one wake-up.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct IdleEvent {
    /// Player state changed (song end, play/pause/stop).
    pub player: bool,
    /// The adapter sent `release` on the `ytcast` channel.
    pub release: bool,
    /// MPD's volume changed.
    pub mixer: bool,
}

impl IdleEvent {
    /// Combine queued events so draining the channel never drops a release.
    pub fn merge(self, other: IdleEvent) -> IdleEvent {
        IdleEvent {
            player: self.player || other.player,
            release: self.release || other.release,
            mixer: self.mixer || other.mixer,
        }
    }
}

// ---------------------------------------------------------------------------
// Real MPD client (production)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock-mpd"))]
mod real {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
    use tokio::net::TcpStream;

    pub struct MpdClient {
        reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
        writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
        host: String,
        port: u16,
    }

    /// A URI as one quoted MPD argument. Quotes and backslashes are escaped
    /// so they cannot end the argument; line breaks would end the command
    /// and cannot be escaped, so they are refused.
    fn quoted_uri(uri: &str) -> Result<String> {
        if uri.contains('\n') || uri.contains('\r') {
            return Err(MpdError::Protocol("URI contains a line break".into()));
        }
        let escaped = uri.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(format!("\"{escaped}\""))
    }

    impl MpdClient {
        /// Connect to MPD and consume the greeting line (`OK MPD x.x.x`).
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            let (reader, writer) = Self::connect_inner(host, port).await?;
            Ok(Self {
                reader,
                writer,
                host: host.to_owned(),
                port,
            })
        }

        async fn connect_inner(
            host: &str,
            port: u16,
        ) -> Result<(
            BufReader<tokio::io::ReadHalf<TcpStream>>,
            BufWriter<tokio::io::WriteHalf<TcpStream>>,
        )> {
            let stream = TcpStream::connect((host, port)).await?;
            let (rd, wr) = tokio::io::split(stream);
            let mut reader = BufReader::new(rd);
            let writer = BufWriter::new(wr);

            // Read greeting
            let mut greeting = String::new();
            let n = reader.read_line(&mut greeting).await?;
            if n == 0 {
                return Err(MpdError::Protocol("connection closed before greeting".into()));
            }
            if !greeting.starts_with("OK MPD") {
                return Err(MpdError::Protocol(format!(
                    "expected OK MPD greeting, got: {greeting}"
                )));
            }

            tracing::info!("connected to MPD at {host}:{port} — {}", greeting.trim());
            Ok((reader, writer))
        }

        async fn reconnect(&mut self) -> Result<()> {
            tracing::info!("[MPD] reconnecting command connection...");
            let (reader, writer) = Self::connect_inner(&self.host, self.port).await?;
            self.reader = reader;
            self.writer = writer;
            Ok(())
        }

        // -- queue / playback ------------------------------------------------

        pub async fn clear(&mut self) -> Result<()> {
            self.command("clear").await?;
            Ok(())
        }

        pub async fn add(&mut self, uri: &str) -> Result<()> {
            self.command(&format!("add {}", quoted_uri(uri)?)).await?;
            Ok(())
        }

        /// Append `uri` to the queue and return its song id, which stays
        /// valid while other songs are added or removed around it.
        pub async fn addid(&mut self, uri: &str) -> Result<u32> {
            let lines = self.command(&format!("addid {}", quoted_uri(uri)?)).await?;
            lines
                .iter()
                .find_map(|l| l.strip_prefix("Id: ")?.parse().ok())
                .ok_or_else(|| MpdError::Protocol("addid response without Id".into()))
        }

        pub async fn playid(&mut self, id: u32) -> Result<()> {
            self.command(&format!("playid {id}")).await?;
            Ok(())
        }

        pub async fn deleteid(&mut self, id: u32) -> Result<()> {
            self.command(&format!("deleteid {id}")).await?;
            Ok(())
        }

        /// Run `commands` as one command list: MPD executes them in one go,
        /// before it serves any other client or plays on, and stops at the
        /// first failing one, whose ACK is returned.
        pub async fn command_list(&mut self, commands: &[String]) -> Result<()> {
            if commands.iter().any(|c| c.contains('\n') || c.contains('\r')) {
                return Err(MpdError::Protocol("command contains a line break".into()));
            }
            let list = format!("command_list_begin\n{}\ncommand_list_end", commands.join("\n"));
            self.command(&list).await?;
            Ok(())
        }

        pub async fn play(&mut self) -> Result<()> {
            self.command("play").await?;
            Ok(())
        }

        pub async fn pause(&mut self) -> Result<()> {
            self.command("pause 1").await?;
            Ok(())
        }

        pub async fn resume(&mut self) -> Result<()> {
            self.command("pause 0").await?;
            Ok(())
        }

        pub async fn stop(&mut self) -> Result<()> {
            self.command("stop").await?;
            Ok(())
        }

        pub async fn seekcur(&mut self, seconds: f64) -> Result<()> {
            if !seconds.is_finite() || seconds < 0.0 {
                return Err(MpdError::Protocol("invalid seek position".into()));
            }
            self.command(&format!("seekcur {seconds}")).await?;
            Ok(())
        }

        pub async fn setvol(&mut self, vol: u32) -> Result<()> {
            self.command(&format!("setvol {vol}")).await?;
            Ok(())
        }

        pub async fn single(&mut self, mode: &str) -> Result<()> {
            self.command(&format!("single {mode}")).await?;
            Ok(())
        }

        pub async fn consume(&mut self, mode: &str) -> Result<()> {
            self.command(&format!("consume {mode}")).await?;
            Ok(())
        }

        // -- status ----------------------------------------------------------

        pub async fn status(&mut self) -> Result<MpdStatus> {
            let lines = self.command("status").await?;
            let mut state = String::from("stop");
            let mut elapsed: Option<f64> = None;
            let mut duration: Option<f64> = None;
            let mut volume: u32 = 0;

            for line in &lines {
                if let Some((key, val)) = line.split_once(": ") {
                    match key {
                        "state" => state = val.to_owned(),
                        "elapsed" => elapsed = val.parse().ok(),
                        "duration" => duration = val.parse().ok(),
                        "volume" => volume = val.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }

            Ok(MpdStatus {
                state,
                elapsed,
                duration,
                volume,
            })
        }

        // -- protocol layer --------------------------------------------------

        /// Send a command, auto-reconnecting once on connection errors
        /// (Io errors, "connection closed"). ACK errors from MPD are NOT retried.
        async fn command(&mut self, cmd: &str) -> Result<Vec<String>> {
            match self.command_raw(cmd).await {
                Ok(lines) => Ok(lines),
                Err(e @ MpdError::Ack(_)) => Err(e),
                Err(_) => {
                    // Io or Protocol error = connection broken. Reconnect + retry.
                    if let Err(e) = self.reconnect().await {
                        tracing::error!("[MPD] reconnect failed: {e}");
                        return Err(e);
                    }
                    self.command_raw(cmd).await
                }
            }
        }

        /// Send a raw command (no reconnect logic).
        async fn command_raw(&mut self, cmd: &str) -> Result<Vec<String>> {
            // Write command
            self.writer.write_all(cmd.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;

            // Read response lines until OK or ACK
            const MAX_LINE_LEN: usize = 64 * 1024;
            const MAX_LINES: usize = 10_000;
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await?;
                if n == 0 {
                    return Err(MpdError::Protocol("connection closed".into()));
                }
                if line.len() > MAX_LINE_LEN {
                    return Err(MpdError::Protocol("response line too long".into()));
                }

                let trimmed = line.trim_end();

                if trimmed == "OK" {
                    return Ok(lines);
                }
                if trimmed.starts_with("ACK ") {
                    return Err(MpdError::Ack(trimmed.to_owned()));
                }

                if lines.len() >= MAX_LINES {
                    return Err(MpdError::Protocol("too many response lines".into()));
                }
                lines.push(trimmed.to_owned());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Idle client — dedicated connection for real-time player state events
    // -----------------------------------------------------------------------

    pub struct MpdIdleClient {
        reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
        writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
    }

    impl MpdIdleClient {
        /// Connect to MPD and consume the greeting line.
        ///
        /// This connection is dedicated to `idle` commands for real-time event
        /// detection — the same approach the Node.js mpd2 library uses.
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            let stream = TcpStream::connect((host, port)).await?;
            let (rd, wr) = tokio::io::split(stream);
            let mut reader = BufReader::new(rd);
            let writer = BufWriter::new(wr);

            let mut greeting = String::new();
            let n = reader.read_line(&mut greeting).await?;
            if n == 0 {
                return Err(MpdError::Protocol("connection closed before greeting".into()));
            }
            if !greeting.starts_with("OK MPD") {
                return Err(MpdError::Protocol(format!(
                    "expected OK MPD greeting, got: {greeting}"
                )));
            }

            let mut client = Self { reader, writer };
            // Subscriptions are per connection, so every (re)connect subscribes.
            client.command(&subscribe_command(RELEASE_CHANNEL)).await?;
            Ok(client)
        }

        /// Block until the player changes (song ends, play/pause/stop) or the
        /// adapter posts on our channel.
        ///
        /// `idle player mixer message` blocks server-side, which gives instant
        /// song-end detection without polling. Pending channel messages are
        /// read on this same connection, which is free again once idle returns.
        pub async fn wait_event(&mut self) -> Result<IdleEvent> {
            let changes = parse_idle_changes(&self.command(IDLE_COMMAND).await?);
            let release = if changes.message {
                contains_release(&parse_channel_messages(&self.command("readmessages").await?))
            } else {
                false
            };
            Ok(IdleEvent { player: changes.player, release, mixer: changes.mixer })
        }

        /// Send one command and collect its response lines up to `OK`.
        async fn command(&mut self, cmd: &str) -> Result<Vec<String>> {
            self.writer.write_all(cmd.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;

            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                let n = self.reader.read_line(&mut line).await?;
                if n == 0 {
                    return Err(MpdError::Protocol("idle connection closed".into()));
                }
                let trimmed = line.trim_end();
                if trimmed == "OK" {
                    return Ok(lines);
                }
                if trimmed.starts_with("ACK ") {
                    return Err(MpdError::Ack(trimmed.to_owned()));
                }
                lines.push(trimmed.to_owned());
            }
        }
    }
}

#[cfg(not(feature = "mock-mpd"))]
pub use real::{MpdClient, MpdIdleClient};

// ---------------------------------------------------------------------------
// Mock MPD client (for Windows development without a real MPD)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock-mpd")]
mod mock {
    use super::*;

    /// The first 80 characters; a byte slice could split a character.
    fn log_prefix(uri: &str) -> String {
        uri.chars().take(80).collect()
    }

    pub struct MpdClient;

    impl MpdClient {
        pub async fn connect(host: &str, port: u16) -> Result<Self> {
            tracing::info!("[mock-mpd] skipping connection to {host}:{port}");
            Ok(Self)
        }

        pub async fn clear(&mut self) -> Result<()> {
            Ok(())
        }

        pub async fn add(&mut self, uri: &str) -> Result<()> {
            tracing::info!("[mock-mpd] add: {}", log_prefix(uri));
            Ok(())
        }

        pub async fn play(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] play");
            Ok(())
        }

        pub async fn addid(&mut self, uri: &str) -> Result<u32> {
            tracing::info!("[mock-mpd] addid: {}", log_prefix(uri));
            Ok(1)
        }

        pub async fn playid(&mut self, id: u32) -> Result<()> {
            tracing::info!("[mock-mpd] playid {id}");
            Ok(())
        }

        pub async fn deleteid(&mut self, id: u32) -> Result<()> {
            tracing::info!("[mock-mpd] deleteid {id}");
            Ok(())
        }

        pub async fn command_list(&mut self, commands: &[String]) -> Result<()> {
            tracing::info!("[mock-mpd] command list: {}", commands.join("; "));
            Ok(())
        }

        pub async fn pause(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] pause");
            Ok(())
        }

        pub async fn resume(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] resume");
            Ok(())
        }

        pub async fn stop(&mut self) -> Result<()> {
            tracing::info!("[mock-mpd] stop");
            Ok(())
        }

        pub async fn seekcur(&mut self, seconds: f64) -> Result<()> {
            tracing::info!("[mock-mpd] seekcur {seconds}");
            Ok(())
        }

        pub async fn setvol(&mut self, vol: u32) -> Result<()> {
            tracing::info!("[mock-mpd] setvol {vol}");
            Ok(())
        }

        pub async fn single(&mut self, _mode: &str) -> Result<()> {
            Ok(())
        }

        pub async fn consume(&mut self, _mode: &str) -> Result<()> {
            Ok(())
        }

        pub async fn status(&mut self) -> Result<MpdStatus> {
            Ok(MpdStatus {
                state: "stop".into(),
                elapsed: Some(0.0),
                duration: Some(0.0),
                volume: 50,
            })
        }
    }

    pub struct MpdIdleClient;

    impl MpdIdleClient {
        pub async fn connect(_host: &str, _port: u16) -> Result<Self> {
            tracing::info!("[mock-mpd] idle client (no-op)");
            Ok(Self)
        }

        pub async fn wait_event(&mut self) -> Result<IdleEvent> {
            // Mock: never fires — no real MPD to generate events
            std::future::pending().await
        }
    }
}

#[cfg(feature = "mock-mpd")]
pub use mock::{MpdClient, MpdIdleClient};

#[cfg(all(test, feature = "mock-mpd"))]
mod mock_tests {
    use super::*;

    #[tokio::test]
    async fn mock_logs_long_multibyte_uris_without_panicking() {
        // 'ä' is two bytes: byte 80 falls inside a character.
        let uri = format!("http://x/{}", "ä".repeat(60));
        // Log arguments are only evaluated with a subscriber listening.
        let capture = crate::storage::test_util::LogCapture::start();
        let mut mpd = MpdClient::connect("127.0.0.1", 0).await.unwrap();
        mpd.add(&uri).await.unwrap();
        mpd.addid(&uri).await.unwrap();
        assert!(capture.text().contains("[mock-mpd] addid: http://x/ä"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn idle_command_listens_for_player_volume_and_messages() {
        assert_eq!(IDLE_COMMAND, "idle player mixer message");
        assert_eq!(subscribe_command(RELEASE_CHANNEL), "subscribe ytcast");
    }

    #[test]
    fn idle_changes_are_parsed_per_subsystem() {
        let only = |player, mixer, message| IdleChanges { player, mixer, message };
        assert_eq!(parse_idle_changes(&lines("changed: player")), only(true, false, false));
        assert_eq!(parse_idle_changes(&lines("changed: mixer")), only(false, true, false));
        assert_eq!(parse_idle_changes(&lines("changed: message")), only(false, false, true));
        assert_eq!(
            parse_idle_changes(&lines("changed: player\nchanged: mixer\nchanged: message")),
            only(true, true, true)
        );
        assert_eq!(parse_idle_changes(&[]), IdleChanges::default());
    }

    #[test]
    fn readmessages_response_is_parsed_into_channel_messages() {
        let msgs = parse_channel_messages(&lines(
            "channel: ytcast\nmessage: release\nchannel: other\nmessage: hello world",
        ));
        assert_eq!(
            msgs,
            vec![
                ChannelMessage { channel: "ytcast".into(), message: "release".into() },
                ChannelMessage { channel: "other".into(), message: "hello world".into() },
            ]
        );
        assert!(parse_channel_messages(&[]).is_empty());
    }

    #[test]
    fn only_release_on_the_ytcast_channel_counts_as_release() {
        let msg = |c: &str, m: &str| ChannelMessage { channel: c.into(), message: m.into() };
        assert!(contains_release(&[msg("ytcast", "release")]));
        assert!(contains_release(&[msg("ytcast", "bogus"), msg("ytcast", "release")]));
        assert!(!contains_release(&[msg("ytcast", "bogus")]));
        assert!(!contains_release(&[msg("other", "release")]));
        assert!(!contains_release(&[]));
    }

    #[test]
    fn idle_events_merge_without_losing_a_release() {
        let a = IdleEvent { player: true, release: false, mixer: false };
        let b = IdleEvent { player: false, release: true, mixer: false };
        assert_eq!(a.merge(b), IdleEvent { player: true, release: true, mixer: false });
    }

    #[test]
    fn idle_events_merge_without_losing_a_volume_change() {
        let a = IdleEvent { player: true, release: false, mixer: false };
        let b = IdleEvent { player: false, release: false, mixer: true };
        assert_eq!(a.merge(b), IdleEvent { player: true, release: false, mixer: true });
    }

    #[cfg(not(feature = "mock-mpd"))]
    mod command_client {
        use super::super::*;
        use crate::mpd::fake::fake_mpd;

        #[tokio::test]
        async fn addid_returns_the_new_song_id() {
            let (port, server) = fake_mpd(vec![("addid", "Id: 42\nOK\n")]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            assert_eq!(mpd.addid("http://localhost:8008/silence").await.unwrap(), 42);
            drop(mpd);
            assert_eq!(server.await.unwrap(), vec!["addid \"http://localhost:8008/silence\""]);
        }

        #[tokio::test]
        async fn addid_without_an_id_is_a_protocol_error() {
            let (port, _server) = fake_mpd(vec![("addid", "OK\n")]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            assert!(matches!(mpd.addid("http://x/").await, Err(MpdError::Protocol(_))));
        }

        #[tokio::test]
        async fn addid_escapes_quotes_and_backslashes() {
            // Unescaped, a trailing backslash would swallow the closing quote.
            let (port, server) = fake_mpd(vec![("addid", "Id: 1\nOK\n"), ("add", "OK\n")]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            mpd.addid(r#"http://x/a"b\"#).await.unwrap();
            mpd.add(r#"http://x/c\d"#).await.unwrap();
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec![r#"addid "http://x/a\"b\\""#, r#"add "http://x/c\\d""#]
            );
        }

        #[tokio::test]
        async fn addid_rejects_uris_that_could_inject_commands() {
            let (port, server) = fake_mpd(vec![]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            for uri in ["http://x/\nstop", "http://x/\r"] {
                let err = mpd.addid(uri).await.unwrap_err();
                assert!(matches!(err, MpdError::Protocol(_)), "{uri:?}: {err}");
                let err = mpd.add(uri).await.unwrap_err();
                assert!(matches!(err, MpdError::Protocol(_)), "{uri:?}: {err}");
            }
            drop(mpd);
            assert!(server.await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn command_list_is_sent_as_one_batch_with_one_reply() {
            let (port, server) = fake_mpd(vec![
                ("command_list_begin", ""),
                ("playid 6", ""),
                ("pause 1", ""),
                ("command_list_end", "OK\n"),
                ("status", "state: pause\nOK\n"),
            ])
            .await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            mpd.command_list(&["playid 6".into(), "pause 1".into()]).await.unwrap();
            // The connection is in step: the next reply is the status's.
            assert_eq!(mpd.status().await.unwrap().state, "pause");
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec!["command_list_begin", "playid 6", "pause 1", "command_list_end", "status"]
            );
        }

        #[tokio::test]
        async fn command_list_refuses_a_command_with_a_line_break() {
            // A line break would end the command and smuggle in the next one.
            let (port, server) = fake_mpd(vec![]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            for command in ["pause 1\nclear", "x\r"] {
                let err = mpd.command_list(&["playid 6".into(), command.into()]).await.unwrap_err();
                assert!(matches!(err, MpdError::Protocol(_)), "{command:?}: {err}");
            }
            drop(mpd);
            assert!(server.await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_failing_command_in_the_list_fails_the_list() {
            let (port, _server) = fake_mpd(vec![
                ("command_list_begin", ""),
                ("playid 6", ""),
                ("command_list_end", "ACK [50@0] {playid} No such song\n"),
            ])
            .await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            let err = mpd.command_list(&["playid 6".into()]).await.unwrap_err();
            assert!(matches!(err, MpdError::Ack(ref m) if m.contains("{playid}")), "{err}");
        }

        #[tokio::test]
        async fn playid_and_deleteid_address_songs_by_id() {
            let (port, server) = fake_mpd(vec![("playid", "OK\n"), ("deleteid", "OK\n")]).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            mpd.playid(7).await.unwrap();
            mpd.deleteid(3).await.unwrap();
            drop(mpd);
            assert_eq!(server.await.unwrap(), vec!["playid 7", "deleteid 3"]);
        }
    }

    #[cfg(not(feature = "mock-mpd"))]
    mod idle_client {
        use super::super::*;
        use crate::mpd::fake::fake_mpd;

        #[tokio::test]
        async fn connect_subscribes_and_release_message_is_reported() {
            let (port, server) = fake_mpd(vec![
                ("subscribe ytcast", "OK\n"),
                ("idle player mixer message", "changed: message\nOK\n"),
                ("readmessages", "channel: ytcast\nmessage: release\nOK\n"),
            ])
            .await;

            let mut idle = MpdIdleClient::connect("127.0.0.1", port).await.unwrap();
            let event = idle.wait_event().await.unwrap();
            assert_eq!(event, IdleEvent { player: false, release: true, mixer: false });

            drop(idle);
            assert_eq!(
                server.await.unwrap(),
                vec!["subscribe ytcast", "idle player mixer message", "readmessages"]
            );
        }

        #[tokio::test]
        async fn player_change_alone_does_not_read_messages() {
            let (port, server) = fake_mpd(vec![
                ("subscribe ytcast", "OK\n"),
                ("idle player mixer message", "changed: player\nOK\n"),
            ])
            .await;

            let mut idle = MpdIdleClient::connect("127.0.0.1", port).await.unwrap();
            let event = idle.wait_event().await.unwrap();
            assert_eq!(event, IdleEvent { player: true, release: false, mixer: false });

            drop(idle);
            assert_eq!(server.await.unwrap(), vec!["subscribe ytcast", "idle player mixer message"]);
        }

        #[tokio::test]
        async fn volume_change_is_reported_without_reading_messages() {
            let (port, server) = fake_mpd(vec![
                ("subscribe ytcast", "OK\n"),
                ("idle player mixer message", "changed: mixer\nOK\n"),
            ])
            .await;

            let mut idle = MpdIdleClient::connect("127.0.0.1", port).await.unwrap();
            let event = idle.wait_event().await.unwrap();
            assert_eq!(event, IdleEvent { player: false, release: false, mixer: true });

            drop(idle);
            assert_eq!(server.await.unwrap(), vec!["subscribe ytcast", "idle player mixer message"]);
        }

        #[tokio::test]
        async fn unknown_messages_are_ignored() {
            let (port, _server) = fake_mpd(vec![
                ("subscribe ytcast", "OK\n"),
                ("idle player mixer message", "changed: message\nOK\n"),
                ("readmessages", "channel: ytcast\nmessage: something-else\nOK\n"),
            ])
            .await;

            let mut idle = MpdIdleClient::connect("127.0.0.1", port).await.unwrap();
            let event = idle.wait_event().await.unwrap();
            assert_eq!(event, IdleEvent::default());
        }

        #[tokio::test]
        async fn subscribe_failure_fails_connect() {
            let (port, _server) = fake_mpd(vec![(
                "subscribe ytcast",
                "ACK [5@0] {subscribe} unknown command \"subscribe\"\n",
            )])
            .await;
            assert!(MpdIdleClient::connect("127.0.0.1", port).await.is_err());
        }
    }
}

/// Scripted in-process MPD shared by the tests of every module that drives
/// an `MpdClient` or `MpdIdleClient`.
#[cfg(all(test, not(feature = "mock-mpd")))]
pub(crate) mod fake {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// Scripted in-process MPD: for each expected command, reply with the
    /// given response. Returns the commands it actually received.
    pub async fn fake_mpd(script: Vec<(&'static str, &'static str)>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        fake_mpd_within(script, Duration::from_secs(5)).await
    }

    /// `fake_mpd` that gives up on a command not sent within `wait`. A client
    /// whose commands differ from the script can end up waiting on a reply
    /// the script never sends while the script waits on its next command;
    /// the server then panics, and its closed connection fails the client.
    async fn fake_mpd_within(
        script: Vec<(&'static str, &'static str)>,
        wait: Duration,
    ) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = tokio::io::split(stream);
            let mut rd = BufReader::new(rd);
            wr.write_all(b"OK MPD 0.23.5\n").await.unwrap();
            let mut seen = Vec::new();
            for (expected, reply) in script {
                let mut line = String::new();
                let read = tokio::time::timeout(wait, rd.read_line(&mut line)).await;
                let Ok(read) = read else {
                    panic!("fake MPD: no command within {wait:?}, expected {expected:?}; received so far: {seen:?}");
                };
                if read.unwrap() == 0 {
                    break;
                }
                seen.push(line.trim_end().to_owned());
                wr.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (port, handle)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::mpd::MpdClient;

        #[tokio::test]
        async fn a_client_waiting_on_an_unscripted_reply_fails_instead_of_hanging() {
            // The script expects a second command that never comes, and the
            // client waits on a reply the script never sends.
            let (port, server) =
                fake_mpd_within(vec![("status", ""), ("next", "OK\n")], Duration::from_millis(100)).await;
            let mut mpd = MpdClient::connect("127.0.0.1", port).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(5), mpd.status()).await;
            assert!(reply.expect("the client hung").is_err());
            let err = server.await.unwrap_err();
            assert!(err.is_panic(), "{err}");
        }
    }
}
