mod cast_tokens;
mod config;
mod dial;
mod innertube;
mod lounge;
mod messages;
mod mpd;
mod music_session;
mod nsig;
mod preroll;
mod sabr;
mod ssdp;
mod storage;
mod watch_next;

use std::sync::Arc;

// ---------------------------------------------------------------------------
// Playlist tracking state (local to main)
// ---------------------------------------------------------------------------

struct PlaylistState {
    video_ids: Vec<String>,
    current_index: usize,
    list_id: String,
    /// Each video's own ctt and params (from the cast or /next), echoed in
    /// nowPlaying for the current video.
    tokens: cast_tokens::CastTokens,
    /// Current track title (from InnerTube).
    current_title: String,
    /// Current track duration in seconds.
    current_duration: u64,
    /// Stable content playback nonce — generated once, reused for all state reports.
    cpn: String,
    /// Last reported player state (for reference-matching NowPlaying logic).
    last_state: messages::PlayerState,
    /// Silence MPD plays from a DIAL launch until the first song.
    preroll: preroll::PreRoll,
    /// Tracks in a row whose stream failed (see `stop_action`).
    failed_in_row: u32,
}

impl Default for PlaylistState {
    fn default() -> Self {
        Self {
            video_ids: Vec::new(),
            current_index: 0,
            list_id: String::new(),
            tokens: cast_tokens::CastTokens::default(),
            current_title: String::new(),
            current_duration: 0,
            cpn: messages::generate_cpn(),
            last_state: messages::PlayerState::Idle,
            preroll: preroll::PreRoll::default(),
            failed_in_row: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory logging (Linux only)
// ---------------------------------------------------------------------------

fn log_memory() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    tracing::info!("[mem] {}", line.trim());
                    return;
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Memory introspection not available on this platform.
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Init logging
    tracing_subscriber::fmt::init();
    if let Some(dir) = storage::data_dir_env() {
        storage::warn_if_relative_data_dir(&dir);
    }

    // 2. Load/create config
    let mut config = config::Config::load()?;
    if config.uuid.is_empty() {
        config.uuid = uuid::Uuid::new_v4().to_string();
        config.save()?;
    }
    tracing::info!("[init] UUID: {}", config.uuid);

    // 3. Read env vars
    let device_name = std::env::var("DEVICE_NAME").unwrap_or_else(|_| "Living Room Pi".into());
    let mpd_host = std::env::var("MPD_HOST").unwrap_or_else(|_| "localhost".into());
    let mpd_port: u16 = std::env::var("MPD_PORT")
        .unwrap_or_else(|_| "6600".into())
        .parse()
        .unwrap_or(6600);
    let dial_port: u16 = 8008;

    // 4. Create the reqwest client for the Lounge sessions. No cookie store:
    // the only authentication is the ctt each cast hands over. InnerTube, SABR
    // and the session fetch use sabr::stream::credential_client(), which only
    // follows redirects on Google hosts.
    let http_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    // 5. Fetch the anonymous YouTube Music session (INNERTUBE_CONTEXT from page)
    // This is critical: without the full 22+ field context from ytcfg,
    // WEB_REMIX /player returns UNPLAYABLE even with a ctt. yt-dlp does the same thing.
    let yt_session = match music_session::fetch_music_session().await {
        Some(session) => {
            tracing::info!("[init] YouTube Music session extracted (anonymous)");
            Some(Arc::new(session))
        }
        None => {
            tracing::warn!("[init] No YouTube Music session — WEB_REMIX + ctt is off until restart; every cast uses the IOS client");
            None
        }
    };

    // 6. Generate screen_ids on first run (one per theme)
    let mut config_changed = false;
    if config.screen_id.is_empty() {
        config.screen_id = lounge::generate_screen_id(&http_client).await?;
        config_changed = true;
        tracing::info!("[init] Generated screen_id (cl)");
    }
    if config.screen_id_m.is_empty() {
        config.screen_id_m = lounge::generate_screen_id(&http_client).await?;
        config_changed = true;
        tracing::info!("[init] Generated screen_id (m)");
    }
    if config_changed {
        config.save()?;
    }

    // 7. Connect to MPD (command connection)
    let mut mpd_client = mpd::MpdClient::connect(&mpd_host, mpd_port).await?;
    mpd_client.consume("0").await?;
    mpd_client.clear().await?;
    tracing::info!("[MPD] Connected to {}:{}", mpd_host, mpd_port);

    // 8. Connect MPD idle listener (dedicated connection for real-time events)
    //
    // MPD's protocol is single-command-at-a-time per connection — you can't
    // send `idle` and `status` on the same socket. Two connections is the
    // standard pattern used by every serious MPD client (mpd2, ncmpcpp, etc.).
    let mut mpd_idle = mpd::MpdIdleClient::connect(&mpd_host, mpd_port).await?;
    tracing::info!("[MPD] Idle listener connected");

    // 9. Create channels
    //
    // Dual Lounge sessions: "cl" (YouTube) and "m" (YouTube Music).
    // Both run concurrently, each with their own long-poll + pairing channel.
    // State updates are routed to whichever session is currently active
    // (determined by which session's remoteConnected fired last).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (player_cmd_tx, mut player_cmd_rx) = tokio::sync::mpsc::channel::<lounge::PlayerCommand>(32);

    // Player → router → active session
    let (state_tx, mut state_router_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);
    let (state_cl_tx, state_cl_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);
    let (state_m_tx, state_m_rx) = tokio::sync::mpsc::channel::<messages::OutgoingMessage>(32);

    // Pairing channels (one per session)
    let (pairing_cl_tx, pairing_cl_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (pairing_m_tx, pairing_m_rx) = tokio::sync::mpsc::channel::<String>(8);

    // DIAL launch → player loop: start the silence pre-roll
    let (preroll_tx, mut preroll_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Active session tracking — router checks this to decide where state goes
    let active_theme: std::sync::Arc<std::sync::RwLock<String>> =
        std::sync::Arc::new(std::sync::RwLock::new("cl".into()));

    // State router: forwards state messages to the active session's channel
    let router_theme = active_theme.clone();
    tokio::spawn(async move {
        while let Some(msg) = state_router_rx.recv().await {
            let theme = router_theme.read().unwrap().clone();
            let tx = if theme == "m" { &state_m_tx } else { &state_cl_tx };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 10. Get local IP
    let local_ip = ssdp::get_local_ip()?;
    tracing::info!("[net] Local IP: {}", local_ip);

    // 11. Create shared DIAL state
    let dial_state = Arc::new(dial::DialState {
        running: std::sync::atomic::AtomicBool::new(false),
        pairing_tx: pairing_cl_tx,
        pairing_tx_m: pairing_m_tx,
        preroll_tx,
        device_name: device_name.clone(),
        uuid: config.uuid.clone(),
        local_ip,
        port: dial_port,
        ctt: std::sync::RwLock::new(None),
        yt_session: yt_session.clone(),
        screen_id: config.screen_id.clone(),
        stream_failure: Default::default(),
        silence_slots: Default::default(),
    });

    // 12. Spawn background tasks

    // SSDP discovery responder
    let ssdp_shutdown = shutdown_rx.clone();
    let ssdp_uuid = config.uuid.clone();
    tokio::spawn(async move {
        if let Err(e) = ssdp::run_ssdp(local_ip, dial_port, &ssdp_uuid, ssdp_shutdown).await {
            tracing::error!("[SSDP] Error: {}", e);
        }
    });

    // DIAL HTTP server
    let dial_shutdown = shutdown_rx.clone();
    let dial_state_server = dial_state.clone();
    tokio::spawn(async move {
        if let Err(e) = dial::run_dial_server(dial_port, dial_state_server, dial_shutdown).await {
            tracing::error!("[DIAL] Error: {}", e);
        }
    });

    // Lounge API — "cl" session (YouTube theme)
    let lounge_cl_shutdown = shutdown_rx.clone();
    let lounge_cl_client = http_client.clone();
    let lounge_cl_name = device_name.clone();
    let lounge_cl_uuid = config.uuid.clone();
    let lounge_cl_screen = config.screen_id.clone();
    let player_cmd_tx_cl = player_cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            lounge_cl_client,
            lounge_cl_name,
            lounge_cl_uuid,
            lounge_cl_screen,
            "cl".to_string(),
            player_cmd_tx_cl,
            state_cl_rx,
            pairing_cl_rx,
            lounge_cl_shutdown,
        )
        .await
        {
            tracing::error!("[Lounge:cl] Error: {}", e);
        }
    });

    // Lounge API — "m" session (YouTube Music theme)
    let lounge_m_shutdown = shutdown_rx.clone();
    let lounge_m_client = http_client.clone();
    let lounge_m_name = device_name.clone();
    let lounge_m_uuid = uuid::Uuid::new_v4().to_string(); // separate device id per session
    let lounge_m_screen = config.screen_id_m.clone();
    tokio::spawn(async move {
        if let Err(e) = lounge::run_lounge(
            lounge_m_client,
            lounge_m_name,
            lounge_m_uuid,
            lounge_m_screen,
            "m".to_string(),
            player_cmd_tx,
            state_m_rx,
            pairing_m_rx,
            lounge_m_shutdown,
        )
        .await
        {
            tracing::error!("[Lounge:m] Error: {}", e);
        }
    });

    // Periodic memory logging (every 60s)
    let mem_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if *mem_shutdown.borrow() {
                break;
            }
            log_memory();
        }
    });

    // 13. MPD idle listener — real-time player state change detection
    //
    // Uses MPD's `idle player message` command for instant notification when a
    // song ends, just like the Node.js mpd2 library's event system. No polling.
    // `message` carries the speaker adapter's `release` on channel `ytcast`.
    let (idle_tx, mut idle_rx) = tokio::sync::mpsc::channel::<mpd::IdleEvent>(8);
    let idle_mpd_host = mpd_host.clone();
    let idle_mpd_port = mpd_port;
    let idle_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            if *idle_shutdown.borrow() {
                break;
            }
            match mpd_idle.wait_event().await {
                Ok(event) if event == mpd::IdleEvent::default() => {}
                Ok(event) => {
                    if idle_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("[MPD idle] {e}, reconnecting...");
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if *idle_shutdown.borrow() {
                            return;
                        }
                        match mpd::MpdIdleClient::connect(&idle_mpd_host, idle_mpd_port).await {
                            Ok(new_idle) => {
                                mpd_idle = new_idle;
                                tracing::info!("[MPD idle] Reconnected");
                                break;
                            }
                            Err(re) => {
                                tracing::error!("[MPD idle] Reconnect failed: {re}");
                            }
                        }
                    }
                }
            }
        }
    });

    tracing::info!("[yt-mpd] \"{}\" is now discoverable", device_name);
    log_memory();

    // 14. Player command loop (runs on main thread)
    //
    // - Idle events detect song endings instantly for auto-advance
    // - Position timer reports playback progress to YouTube every 3s (seekbar sync)
    let innertube_session = yt_session;
    let mut playlist = PlaylistState::default();
    let mut play_started = false;
    let mut has_remote = false;
    // The volume the phone was last told (or set itself): a change of MPD's
    // volume from elsewhere (the speaker adapter) is reported, the phone's own
    // setVolume not echoed.
    let mut reported_volume: Option<u32> = None;
    // localhost like the /stream URLs, so the DIAL server's loopback check passes.
    let silence_url = format!("http://localhost:{dial_port}{}", dial::SILENCE_PATH);
    let mut position_timer = tokio::time::interval(std::time::Duration::from_secs(3));
    // The active theme's senders, for /next's MDX context (never logged).
    let mut sender_ids: Vec<String> = Vec::new();
    // /next answers: the next video's own ctt, one request per song.
    let (prefetch_tx, mut prefetch_rx) = tokio::sync::mpsc::channel::<PrefetchResult>(4);

    loop {
        // Once a song plays with its own ctt, ask /next for the next one's.
        // Here, before select!, it also runs after a branch's `continue`.
        if let Some(session) = innertube_session.as_ref() {
            if let Some(job) = prefetch_job(&mut playlist, play_started) {
                spawn_prefetch(job, session.clone(), sender_ids.clone(), prefetch_tx.clone());
            }
        }

        tokio::select! {
            // Player commands from the Lounge protocol
            cmd = player_cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    tracing::info!("[player] Command channel closed, shutting down");
                    break;
                };

                // Track remote connection state + active session
                match &cmd {
                    lounge::PlayerCommand::RemoteConnected { theme, device_ids } => {
                        tracing::info!(
                            "[player] Remote connected on theme={} ({} sender id(s)), ready for commands",
                            theme, device_ids.len()
                        );
                        remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active_theme, has_remote, theme, device_ids);
                        has_remote = true;
                        continue;
                    }
                    lounge::PlayerCommand::RemoteDisconnected { theme } => {
                        let current = active_theme.read().unwrap().clone();
                        if *theme != current {
                            tracing::debug!("[player] Ignoring disconnect from non-active theme={}", theme);
                            continue;
                        }
                        has_remote = false;
                        // A song playing on keeps its senders for /next's
                        // MDX context, as if they were still connected.
                        if !plays_on_without_sender(play_started, &playlist) {
                            sender_ids.clear();
                        }
                        // Fall through to handle_command: play on, or stop and clean up
                    }
                    lounge::PlayerCommand::SetVolume { volume } => reported_volume = Some(*volume),
                    _ => {}
                }

                // Ignore stale playback commands replayed before any remote connects.
                // These come from the initial bind and would pollute YouTube's
                // cached state with duration=0 (innertube can't resolve without ctt).
                if !has_remote && matches!(&cmd,
                    lounge::PlayerCommand::SetPlaylist { .. } |
                    lounge::PlayerCommand::SetVideo { .. }
                ) {
                    tracing::info!("[player] Ignoring stale command (no remote connected yet)");
                    continue;
                }

                if let Err(e) = handle_command(
                    cmd,
                    &mut mpd_client,
                    &state_tx,
                    &mut playlist,
                    &mut play_started,
                    &dial_state,
                    innertube_session.as_ref().map(|s| s.as_ref()),
                ).await {
                    tracing::error!("[player] Error handling command: {}", e);
                }
            }

            // MPD idle wake-up — instant song-end detection, or the speaker
            // adapter releasing us (standby / other source)
            Some(event) = idle_rx.recv() => {
                // Merge extra queued events (e.g. from clear+add+play sequence)
                let mut event = event;
                while let Ok(more) = idle_rx.try_recv() {
                    event = event.merge(more);
                }
                let plan = idle_plan(event, play_started, playlist.preroll.is_active());

                if event.mixer {
                    match mpd_client.status().await {
                        Ok(status) => {
                            if let Some(volume) = volume_to_report(has_remote, reported_volume, status.volume) {
                                tracing::info!("[player] volume {} set on MPD (speaker adapter)", volume);
                                reported_volume = Some(volume);
                                send_volume(&state_tx, volume).await.ok();
                            }
                        }
                        Err(e) => tracing::warn!("[player] mpd.status() after a volume change FAILED: {}", e),
                    }
                }

                if plan.release {
                    match release_action(has_remote, &playlist) {
                        ReleaseAction::Ignore => {
                            tracing::debug!("[player] release from adapter ignored: no active cast");
                            continue;
                        }
                        ReleaseAction::StopPreRoll => {
                            stop_preroll(&mut mpd_client, &mut playlist, &mut play_started, "released by speaker (adapter)").await;
                            continue;
                        }
                        ReleaseAction::EndCast => {}
                    }
                    if playlist.preroll.is_active() {
                        tracing::info!("[preroll] pre-roll ended: released by speaker (adapter)");
                    }
                    tracing::info!("[player] released by speaker (adapter) — ending cast session");
                    has_remote = false;
                    end_released_cast(
                        &mut mpd_client, &mut playlist, &mut play_started, &mut sender_ids, &dial_state, &state_tx,
                    ).await;
                    continue;
                }

                if plan.check_song_end {
                    if let Ok(status) = mpd_client.status().await {
                        if song_ended(&status) {
                            play_started = false;

                            let ended_vid = current_video_id(&playlist);
                            let ended_index = playlist.current_index;
                            let failure = dial_state.take_stream_failure(&ended_vid);
                            let next = match stop_action(failure, &mut playlist) {
                                StopAction::StreamFailed(reason) => {
                                    tracing::error!(
                                        "[player] Stream failed for {} (index {}): {}; not advancing{}",
                                        ended_vid, ended_index, reason,
                                        if playlist.failed_in_row >= MAX_FAILED_IN_ROW {
                                            format!(" ({} tracks in a row failed)", playlist.failed_in_row)
                                        } else {
                                            String::new()
                                        }
                                    );
                                    playlist.last_state = messages::PlayerState::Idle;
                                    state_tx.send(stream_failed_message(&playlist)).await.ok();
                                    if !has_remote {
                                        end_unattended_cast(
                                            &mut mpd_client, &mut playlist, &mut play_started, &mut sender_ids, &dial_state,
                                        ).await;
                                    }
                                    continue;
                                }
                                StopAction::SkipFailed { next, reason } => {
                                    tracing::warn!(
                                        "[player] Stream failed for {} (index {}): {}; skipping to index {} ({} of {} in a row)",
                                        ended_vid, ended_index, reason, next, playlist.failed_in_row, MAX_FAILED_IN_ROW
                                    );
                                    // Unstarted at 0, not ended: the app follows
                                    // the skip instead of advancing itself.
                                    playlist.last_state = messages::PlayerState::Idle;
                                    state_tx.send(stream_failed_message(&playlist)).await.ok();
                                    Some((next, "Skipping"))
                                }
                                // Tell YouTube the current track has ended BEFORE
                                // attempting auto-advance. If InnerTube fails
                                // (LOGIN_REQUIRED), YouTube already has the ended
                                // signal and will send a recovery setPlaylist fast.
                                StopAction::Advance(next) => {
                                    tracing::info!("[player] Song ended: {} (index {})", ended_vid, ended_index);
                                    playlist.last_state = messages::PlayerState::Stopped;
                                    state_tx.send(song_ended_message(&playlist)).await.ok();
                                    Some((next, "Advancing"))
                                }
                                StopAction::Finished => {
                                    tracing::info!("[player] Song ended: {} (index {})", ended_vid, ended_index);
                                    playlist.last_state = messages::PlayerState::Stopped;
                                    state_tx.send(song_ended_message(&playlist)).await.ok();
                                    None
                                }
                            };

                            if let Some((next, why)) = next {
                                let video_id = adopt_index(&mut playlist, &dial_state, next, why);

                                match play_video(
                                    &mut mpd_client, &state_tx,
                                    &mut playlist, &video_id, 0.0,
                                    innertube_session.as_ref().map(|s| s.as_ref()),
                                ).await {
                                    Ok(true) => play_started = true,
                                    Ok(false) => {
                                        tracing::warn!(
                                            "[player] Auto-advance: no stream for {}, waiting for YouTube recovery",
                                            video_id
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!("[player] Auto-advance error: {}", e);
                                    }
                                }
                            } else {
                                tracing::info!("[player] Playlist finished");
                                if !has_remote {
                                    end_unattended_cast(
                                        &mut mpd_client, &mut playlist, &mut play_started, &mut sender_ids, &dial_state,
                                    ).await;
                                }
                            }
                        }
                    }
                }
            }

            // The next video's own ctt from /next
            Some(result) = prefetch_rx.recv() => {
                apply_prefetch(&mut playlist, result);
            }

            // DIAL launch: play silence so the speaker wakes while the phone loads
            Some(()) = preroll_rx.recv() => {
                start_preroll(&mut mpd_client, &mut playlist.preroll, play_started, &silence_url).await;
            }

            // No song came in time: stop the silence so the speaker can idle
            _ = preroll_expired(playlist.preroll.deadline()) => {
                stop_preroll(&mut mpd_client, &mut playlist, &mut play_started, "timed out without a song").await;
            }

            // Position reporting + keepalive (every 3s)
            //
            // Always queries MPD status to keep the command connection alive
            // (MPD closes idle connections after 60s). Only reports to YouTube
            // when actively playing.
            _ = position_timer.tick() => {
                match mpd_client.status().await {
                    Ok(status) => {
                        if reports_position(play_started, &playlist) {
                            if status.state == "play" {
                                let elapsed = status.elapsed.unwrap_or(0.0);
                                let duration = playlist.current_duration as f64;
                                tracing::info!(
                                    "[tick] pos={} dur={} vid={}",
                                    messages::fmt_time(elapsed),
                                    messages::fmt_time(duration),
                                    current_video_id(&playlist),
                                );
                                // Position tick: only send onStateChange, NOT nowPlaying.
                                state_tx.send(
                                    messages::OutgoingMessage::new("onStateChange")
                                        .with_arg("state", "1")
                                        .with_arg("currentTime", messages::fmt_time(elapsed))
                                        .with_arg("duration", messages::fmt_time(duration))
                                        .with_arg("loadedTime", messages::fmt_time(duration))
                                        .with_arg("seekableStartTime", "0")
                                        .with_arg("seekableEndTime", messages::fmt_time(duration))
                                        .with_arg("cpn", &playlist.cpn),
                                ).await.ok();
                            } else {
                                tracing::info!(
                                    "[tick] mpd_state={} (not play), skipping tick",
                                    status.state
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("[tick] mpd.status() FAILED: {}", e);
                    }
                }
            }

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT, shutting down...");
                break;
            }
        }
    }

    let _ = shutdown_tx.send(true);
    tracing::info!("Shutdown complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

type CmdResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn handle_command(
    cmd: lounge::PlayerCommand,
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    dial_state: &Arc<dial::DialState>,
    yt_session: Option<&music_session::YtMusicSession>,
) -> CmdResult {
    use lounge::PlayerCommand;

    match cmd {
        PlayerCommand::SetPlaylist {
            video_id,
            video_ids,
            list_id,
            index,
            ctt,
            params,
            current_time,
        } => {
            tracing::info!(
                "[player] SetPlaylist: {} videos, starting at index {} ({})",
                video_ids.len(),
                index,
                video_id
            );

            let auth = cast_tokens::VideoAuth { ctt, params };
            adopt_playlist(playlist, dial_state, &video_id, video_ids, index, list_id, auth);

            *play_started = false;
            *play_started = play_video(mpd, state_tx, playlist, &video_id, current_time, yt_session).await?;
        }

        PlayerCommand::SetVideo {
            video_id,
            ctt,
            current_time,
        } => {
            tracing::info!("[player] SetVideo: {}", video_id);

            adopt_video(playlist, dial_state, &video_id, ctt);

            *play_started = false;
            *play_started = play_video(mpd, state_tx, playlist, &video_id, current_time, yt_session).await?;
        }

        // The silence is no song: resuming, pausing or seeking it would make
        // its later stop look like a song end. The phone hears that nothing
        // plays, as MPD would say without the pre-roll.
        PlayerCommand::Play | PlayerCommand::Pause | PlayerCommand::SeekTo { .. }
            if playlist.preroll.is_active() =>
        {
            tracing::info!("[player] {cmd:?} ignored: only the pre-roll silence is playing");
            send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
        }

        PlayerCommand::Play => {
            tracing::info!("[player] Play (resume)");
            mpd.resume().await?;
            *play_started = true;
            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
        }

        PlayerCommand::Pause => {
            tracing::info!("[player] Pause");
            mpd.pause().await?;
            // Don't clear play_started — we still want auto-advance after resume
            send_state(mpd, state_tx, playlist, messages::PlayerState::Paused).await?;
        }

        PlayerCommand::SeekTo { position } => {
            tracing::info!("[player] SeekTo: {}s", position);
            match mpd.seekcur(position).await {
                Ok(()) => {}
                Err(e) => {
                    // SABR streams aren't seekable until fully buffered.
                    // Don't crash — report current position so phone doesn't hang.
                    tracing::warn!("[player] seek failed (non-fatal): {e}");
                }
            }
            // Always respond with current state so the phone doesn't spin
            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
        }

        PlayerCommand::SetVolume { volume } => {
            tracing::info!("[player] SetVolume: {}", volume);
            mpd.setvol(volume).await?;
            send_volume(state_tx, volume).await?;
        }

        PlayerCommand::GetVolume => {
            let status = mpd.status().await?;
            tracing::info!("[player] GetVolume: {}", status.volume);
            send_volume(state_tx, status.volume).await?;
        }

        PlayerCommand::GetNowPlaying => {
            tracing::info!("[player] GetNowPlaying");
            // Query response — always include NowPlaying regardless of state.
            // Temporarily reset last_state so send_state includes NowPlaying.
            playlist.last_state = messages::PlayerState::Idle;
            let status = mpd.status().await?;
            let (state, _) = reported_playback(playlist.preroll.is_active(), &status);
            send_state(mpd, state_tx, playlist, state).await?;
        }

        PlayerCommand::GetPlaylist => {
            tracing::info!("[player] GetPlaylist");
            state_tx
                .send(
                    messages::OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.current_index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .await
                .ok();
        }

        PlayerCommand::Stop => {
            tracing::info!("[player] Stop");
            forget_queue(playlist, play_started, dial_state);
            end_preroll(&mut playlist.preroll, "stop from the phone");
            mpd.stop().await?;
            mpd.clear().await?;
            send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
        }

        PlayerCommand::UpdatePlaylist {
            video_ids,
            list_id,
            current_index,
        } => {
            tracing::info!(
                "[player] UpdatePlaylist: {} videos (was {})",
                video_ids.len(),
                playlist.video_ids.len()
            );

            // Find current video in the new list to maintain position
            let current_vid = current_video_id(playlist);
            let new_index = if let Some(idx) = current_index {
                idx.min(video_ids.len().saturating_sub(1))
            } else {
                // Try to find current video in new list
                video_ids.iter().position(|id| id == &current_vid).unwrap_or(0)
            };

            playlist.video_ids = video_ids;
            playlist.current_index = new_index;
            if !list_id.is_empty() {
                playlist.list_id = list_id;
            }

            // Update nav buttons for the new playlist
            let has_prev = playlist.current_index > 0;
            let has_next = playlist.current_index + 1 < playlist.video_ids.len();
            state_tx
                .send(
                    messages::OutgoingMessage::new("onHasPreviousNextChanged")
                        .with_arg("hasPrevious", if has_prev { "true" } else { "false" })
                        .with_arg("hasNext", if has_next { "true" } else { "false" }),
                )
                .await
                .ok();

            // Send updated playlist info
            state_tx
                .send(
                    messages::OutgoingMessage::new("nowPlayingPlaylist")
                        .with_arg("videoIds", playlist.video_ids.join(","))
                        .with_arg("currentIndex", playlist.current_index.to_string())
                        .with_arg("listId", &playlist.list_id),
                )
                .await
                .ok();
        }

        PlayerCommand::Previous => {
            if playlist.current_index > 0 {
                playlist.failed_in_row = 0;
                let video_id = adopt_index(playlist, dial_state, playlist.current_index - 1, "Previous");
                *play_started = false;
                *play_started = play_video(mpd, state_tx, playlist, &video_id, 0.0, yt_session).await?;
            } else {
                tracing::info!("[player] Previous: already at start");
            }
        }

        PlayerCommand::Next => {
            if playlist.current_index + 1 < playlist.video_ids.len() {
                playlist.failed_in_row = 0;
                let video_id = adopt_index(playlist, dial_state, playlist.current_index + 1, "Next");
                *play_started = false;
                *play_started = play_video(mpd, state_tx, playlist, &video_id, 0.0, yt_session).await?;
            } else {
                tracing::info!("[player] Next: end of playlist");
                *play_started = false;
                end_preroll(&mut playlist.preroll, "next at the end of the playlist");
                mpd.stop().await?;
                mpd.clear().await?;
                send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
            }
        }

        PlayerCommand::RemoteConnected { .. } => {
            // Handled in the main select! loop before handle_command
        }

        // A sender leaves when the phone sleeps: like a Chromecast, the
        // queue plays on. Its ctt stays for the queue until the cast ends
        // (queue end, stop, release) or another sender connects.
        PlayerCommand::RemoteDisconnected { .. } if plays_on_without_sender(*play_started, playlist) => {
            tracing::info!(
                "[player] Remote disconnected, playback continues without a sender (index {} of {})",
                playlist.current_index, playlist.video_ids.len()
            );
        }

        PlayerCommand::RemoteDisconnected { .. } => {
            tracing::info!("[player] Remote disconnected, stopping playback");
            // nowPlaying must not echo the departed cast's ctt or params.
            forget_queue(playlist, play_started, dial_state);
            end_preroll(&mut playlist.preroll, "remote disconnected");
            mpd.stop().await?;
            mpd.clear().await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// What to do when MPD stops while a track was playing.
#[derive(Debug, PartialEq)]
enum StopAction {
    /// The receiver could not serve the stream, and there is no next track
    /// or `MAX_FAILED_IN_ROW` tracks failed in a row: stay on this track and
    /// report the failure.
    StreamFailed(String),
    /// The receiver could not serve the stream; play this index next.
    SkipFailed { next: usize, reason: String },
    /// The track ended; play this playlist index next.
    Advance(usize),
    /// The last track ended.
    Finished,
}

/// Failed tracks in a row after which the queue stops: with no sender to
/// step in, a dead queue (expired ctt chain, network down) must not be
/// skipped through song by song.
const MAX_FAILED_IN_ROW: u32 = 3;

/// Also counts `playlist.failed_in_row`: a failure adds one, a song that
/// played resets it.
fn stop_action(stream_failure: Option<String>, playlist: &mut PlaylistState) -> StopAction {
    let next = playlist.current_index + 1;
    let has_next = next < playlist.video_ids.len();
    match stream_failure {
        Some(reason) => {
            playlist.failed_in_row += 1;
            if has_next && playlist.failed_in_row < MAX_FAILED_IN_ROW {
                StopAction::SkipFailed { next, reason }
            } else {
                StopAction::StreamFailed(reason)
            }
        }
        None => {
            playlist.failed_in_row = 0;
            if has_next {
                StopAction::Advance(next)
            } else {
                StopAction::Finished
            }
        }
    }
}

/// Whether the queue plays on when the last sender leaves: only a song
/// that started (also paused), not the pre-roll or a stopped queue.
fn plays_on_without_sender(play_started: bool, playlist: &PlaylistState) -> bool {
    play_started && !playlist.video_ids.is_empty() && !playlist.preroll.is_active()
}

/// Forget the queue and every song's ctt and params (and the pins that
/// hold them). The caller stops MPD.
fn forget_queue(playlist: &mut PlaylistState, play_started: &mut bool, dial_state: &dial::DialState) {
    *play_started = false;
    set_cast_ctt(dial_state, None, sabr::stream::clear_pins);
    playlist.tokens.clear();
    playlist.video_ids.clear();
    playlist.current_index = 0;
    playlist.list_id.clear();
    playlist.current_title.clear();
    playlist.current_duration = 0;
}

/// A cast that played on without a sender stopped (queue end, failed
/// tracks): nobody can resume it, so forget it as a phone's Stop would.
async fn end_unattended_cast(
    mpd: &mut mpd::MpdClient,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    sender_ids: &mut Vec<String>,
    dial_state: &dial::DialState,
) {
    tracing::info!("[player] No sender connected: cast ended, queue and ctt forgotten");
    forget_queue(playlist, play_started, dial_state);
    sender_ids.clear();
    if let Err(e) = mpd.stop().await {
        tracing::warn!("[player] mpd stop failed: {e}");
    }
    if let Err(e) = mpd.clear().await {
        tracing::warn!("[player] mpd clear failed: {e}");
    }
}

/// The volume to tell the phone after MPD's volume changed: a change from
/// elsewhere (the speaker adapter shows the speaker's own volume), not the
/// phone's own setVolume coming back, and only to a connected phone.
fn volume_to_report(has_remote: bool, reported: Option<u32>, volume: u32) -> Option<u32> {
    (has_remote && reported != Some(volume)).then_some(volume)
}

/// What the player loop does with one (merged) idle wake-up.
#[derive(Debug, PartialEq)]
struct IdlePlan {
    release: bool,
    check_song_end: bool,
}

/// A release wins over a player change seen in the same wake-up: the speaker
/// is gone, so a stop must never be read as a song end and auto-advance.
/// The pre-roll silence is no song, so its stop is no song end either.
fn idle_plan(event: mpd::IdleEvent, play_started: bool, preroll_active: bool) -> IdlePlan {
    IdlePlan {
        release: event.release,
        check_song_end: event.player && play_started && !preroll_active && !event.release,
    }
}

#[derive(Debug, PartialEq)]
enum ReleaseAction {
    /// End the phone's cast and stop MPD.
    EndCast,
    /// Only the pre-roll runs, the phone may still be connecting: stop the
    /// silence and leave the Lounge session alone.
    StopPreRoll,
    /// Nobody is casting.
    Ignore,
}

/// The adapter may release on every speaker standby, also with nobody casting.
/// While the pre-roll runs no song has started yet, so the phone keeps its
/// cast: when its song arrives it takes the speaker again, and the pre-roll
/// cooldown keeps launches from looping.
fn release_action(has_remote: bool, playlist: &PlaylistState) -> ReleaseAction {
    if playlist.preroll.is_active() {
        ReleaseAction::StopPreRoll
    } else if has_remote || !playlist.video_ids.is_empty() {
        ReleaseAction::EndCast
    } else {
        ReleaseAction::Ignore
    }
}

/// Only a stopped player ends a song. A pause is no end: neither the phone's
/// nor the speaker adapter's, which holds a song handed over from a held
/// pre-roll until the speaker plays.
fn song_ended(status: &mpd::MpdStatus) -> bool {
    status.state == "stop"
}

/// The 3 s position report is for a song the phone started, never the
/// pre-roll silence.
fn reports_position(play_started: bool, playlist: &PlaylistState) -> bool {
    play_started && !playlist.video_ids.is_empty() && !playlist.preroll.is_active()
}

/// Player state and position the phone is told. The pre-roll silence is
/// reported as nothing playing, as MPD would be without it.
fn reported_playback(preroll_active: bool, status: &mpd::MpdStatus) -> (messages::PlayerState, f64) {
    if preroll_active {
        (messages::PlayerState::Stopped, 0.0)
    } else {
        (mpd_state_to_player_state(&status.state), status.elapsed.unwrap_or(0.0))
    }
}

/// Store the playing video and its own ctt for `/stream`, which uses it for
/// that video only. When the ctt changes (another video or cast, or the cast
/// ended), `clear_pins` drops the pinned formats, which hold the old ctt.
/// Returns whether it changed.
fn set_cast_ctt(
    dial_state: &dial::DialState,
    video_ctt: Option<(String, String)>,
    clear_pins: impl FnOnce(),
) -> bool {
    let mut current = dial_state.ctt.write().unwrap();
    let ctt_of = |v: &Option<(String, String)>| v.as_ref().map(|(_, ctt)| ctt.clone());
    let changed = ctt_of(&current) != ctt_of(&video_ctt);
    if changed {
        clear_pins();
    }
    *current = video_ctt;
    changed
}

/// `video_id` paired with its own ctt, for `set_cast_ctt`.
fn video_ctt(video_id: &str, ctt: Option<String>) -> Option<(String, String)> {
    ctt.map(|ctt| (video_id.to_owned(), ctt))
}

/// Move to queue position `index` (auto-advance, next, previous): that video
/// streams with its own ctt (from the cast or /next), never another video's;
/// without one it streams as a song without ctt. Returns its video id.
fn adopt_index(playlist: &mut PlaylistState, dial_state: &dial::DialState, index: usize, why: &str) -> String {
    playlist.current_index = index;
    let video_id = playlist.video_ids[index].clone();
    let ctt = resolve_ctt(playlist, &video_id);
    tracing::info!(
        "[player] {why} to index {index} ({video_id}): {}",
        if ctt.is_some() { "own ctt" } else { "no ctt for this video" }
    );
    set_cast_ctt(dial_state, video_ctt(&video_id, ctt), sabr::stream::clear_pins);
    // Track change — ensure NowPlaying is sent
    playlist.last_state = messages::PlayerState::Idle;
    video_id
}

/// The ctt `video_id` resolves (and streams) with: its own, never another
/// video's.
fn resolve_ctt(playlist: &PlaylistState, video_id: &str) -> Option<String> {
    playlist.tokens.auth_for(video_id).ctt
}

/// A sender connected on `theme` (`active` is the theme before it): its ids
/// replace the known ones. A connect without ids (a loungeStatus that
/// listed none) keeps them, unless the theme changed: another session's
/// senders are not this one's.
fn update_sender_ids(sender_ids: &mut Vec<String>, active: &str, theme: &str, device_ids: &[String]) {
    if theme != active || !device_ids.is_empty() {
        *sender_ids = device_ids.to_vec();
    }
}

/// A sender connected on `theme` (`had_remote`: one was connected before):
/// keep its ids for /next, forget the earlier cast's tokens if it is another
/// cast, and make `theme` the active one.
fn remote_connected(
    sender_ids: &mut Vec<String>,
    playlist: &mut PlaylistState,
    dial_state: &dial::DialState,
    active_theme: &std::sync::RwLock<String>,
    had_remote: bool,
    theme: &str,
    device_ids: &[String],
) {
    let active = active_theme.read().unwrap().clone();
    forget_tokens_on_new_sender(playlist, dial_state, had_remote, sender_ids, device_ids);
    update_sender_ids(sender_ids, &active, theme, device_ids);
    forget_tokens_on_theme_change(playlist, dial_state, &active, theme);
    *active_theme.write().unwrap() = theme.to_owned();
}

/// A queue may play on after its last sender left. A sender connecting then
/// that is none of the departed ones (or gives no id) may be someone else:
/// the departed cast's tokens must not reach it in nowPlaying. The queue
/// plays on without them until its setPlaylist.
fn forget_tokens_on_new_sender(
    playlist: &mut PlaylistState,
    dial_state: &dial::DialState,
    had_remote: bool,
    departed: &[String],
    device_ids: &[String],
) {
    if !had_remote && !device_ids.iter().any(|id| departed.contains(id)) {
        playlist.tokens.clear();
        set_cast_ctt(dial_state, None, sabr::stream::clear_pins);
    }
}

/// A sender on the other Lounge theme is another cast: the earlier cast's
/// per-video tokens must not reach it in nowPlaying before its own
/// setPlaylist, and a /next answer still on its way is dropped.
fn forget_tokens_on_theme_change(
    playlist: &mut PlaylistState,
    dial_state: &dial::DialState,
    active: &str,
    theme: &str,
) {
    if theme != active {
        playlist.tokens.clear();
        set_cast_ctt(dial_state, None, sabr::stream::clear_pins);
    }
}

/// A /next request for the song now playing.
struct PrefetchJob {
    generation: u64,
    video_id: String,
    expected_next: String,
    list_id: String,
    params: Option<String>,
    index: usize,
    ctt: String,
}

/// The /next answer for a `PrefetchJob`.
struct PrefetchResult {
    generation: u64,
    video_id: String,
    expected_next: String,
    outcome: Result<Option<(watch_next::NextSource, watch_next::NextVideo)>, String>,
}

/// What became of a /next answer.
#[derive(Debug, PartialEq)]
enum PrefetchVerdict {
    /// The next video's own ctt (and params) are stored.
    Stored,
    /// The cast changed since the request.
    Stale,
    /// /next named another video than the queue's next.
    Mismatch,
    /// /next named the queue's next video, without a ctt.
    NoToken,
    /// No next item in the response, or the request failed.
    NoNext,
}

/// /next is asked once per song, while a song plays with its own ctt and the
/// queue has a next video whose ctt is not known yet.
fn prefetch_job(playlist: &mut PlaylistState, play_started: bool) -> Option<PrefetchJob> {
    if !play_started || playlist.preroll.is_active() {
        return None;
    }
    let video_id = current_video_id(playlist);
    let expected_next = playlist.video_ids.get(playlist.current_index + 1)?.clone();
    let auth = playlist.tokens.auth_for(&video_id);
    let ctt = auth.ctt?;
    if playlist.tokens.auth_for(&expected_next).ctt.is_some()
        || !playlist.tokens.start_prefetch(&video_id, &expected_next)
    {
        return None;
    }
    Some(PrefetchJob {
        generation: playlist.tokens.generation(),
        video_id,
        expected_next,
        list_id: playlist.list_id.clone(),
        params: auth.params,
        index: playlist.current_index,
        ctt,
    })
}

/// Run the /next request off the player loop; the answer comes back on `tx`.
fn spawn_prefetch(
    job: PrefetchJob,
    session: Arc<music_session::YtMusicSession>,
    sender_ids: Vec<String>,
    tx: tokio::sync::mpsc::Sender<PrefetchResult>,
) {
    tracing::info!(
        "[next] asking /next (TVHTML5) for the video after {} ({} sender id(s))",
        job.video_id, sender_ids.len()
    );
    tokio::spawn(async move {
        let target = watch_next::WatchTarget {
            video_id: &job.video_id,
            playlist_id: &job.list_id,
            params: job.params.as_deref(),
            index: job.index,
            ctt: &job.ctt,
            device_ids: &sender_ids,
        };
        let outcome = watch_next::fetch_next(&target, &session).await.map_err(|e| e.to_string());
        let result = PrefetchResult {
            generation: job.generation,
            video_id: job.video_id,
            expected_next: job.expected_next,
            outcome,
        };
        tx.send(result).await.ok();
    });
}

/// Store the next video's own ctt and params from a /next answer, if it is
/// for this cast and names the queue's next video. Anything else leaves that
/// video without a ctt: it then streams like a song without one.
fn apply_prefetch(playlist: &mut PlaylistState, result: PrefetchResult) -> PrefetchVerdict {
    let after = &result.video_id;
    let verdict = match result.outcome {
        _ if result.generation != playlist.tokens.generation() => PrefetchVerdict::Stale,
        Err(e) => {
            tracing::warn!("[next] /next after {after} failed: {e}; the next video plays without ctt");
            PrefetchVerdict::NoNext
        }
        Ok(None) => PrefetchVerdict::NoNext,
        Ok(Some((source, next))) => {
            tracing::info!(
                "[next] /next after {after}: {} via {}, ctt={}, params={}, index={:?}",
                next.video_id,
                source.as_str(),
                next.ctt.as_deref().map_or("none".to_owned(), |c| format!("len {}", c.len())),
                next.params.is_some(),
                next.index,
            );
            if next.video_id != result.expected_next {
                PrefetchVerdict::Mismatch
            } else if next.ctt.is_none() {
                PrefetchVerdict::NoToken
            } else {
                let auth = cast_tokens::VideoAuth { ctt: next.ctt, params: next.params };
                playlist.tokens.remember(&next.video_id, auth);
                PrefetchVerdict::Stored
            }
        }
    };
    let expected = &result.expected_next;
    match verdict {
        PrefetchVerdict::Stored => tracing::info!("[next] {expected} will play with its own ctt"),
        PrefetchVerdict::Stale => tracing::info!("[next] /next answer after {after} is for an earlier cast; dropped"),
        PrefetchVerdict::Mismatch => {
            tracing::warn!("[next] /next names another video than the queue's next {expected}; not used")
        }
        PrefetchVerdict::NoToken => tracing::warn!("[next] /next gave no ctt for {expected}"),
        PrefetchVerdict::NoNext => tracing::warn!("[next] /next gave no next video after {after}"),
    }
    verdict
}

/// Forget the cast and build the messages that end it on the sender.
///
/// `play_started` is cleared first so the MPD stop that follows is not
/// treated as a song end. `loungeScreenDisconnected` is what
/// yt-cast-receiver sends to drop its senders while keeping the screen's
/// Lounge session bound (Session.end() and
/// YouTubeApp#checkAndSwitchActiveSession; Message.LoungeScreenDisconnected:
/// "Notifies senders that their Cast session with the receiver has ended").
/// The empty `nowPlaying` before it clears the phone's mini-player, as
/// yt-cast-receiver's player reset does.
fn release_cast(playlist: &mut PlaylistState, play_started: &mut bool) -> Vec<messages::OutgoingMessage> {
    *play_started = false;
    // The token generation carries on: a /next answer still on its way
    // belongs to the released cast and must not match the next one.
    let mut tokens = std::mem::take(&mut playlist.tokens);
    tokens.clear();
    *playlist = PlaylistState { tokens, ..PlaylistState::default() };
    vec![
        messages::OutgoingMessage::new("nowPlaying"),
        messages::OutgoingMessage::new("loungeScreenDisconnected"),
    ]
}

/// The speaker released the cast: end it (see `release_cast`), forget its
/// ctt and pinned formats, stop the DIAL app and MPD, tell the phone.
async fn end_released_cast(
    mpd_client: &mut mpd::MpdClient,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    sender_ids: &mut Vec<String>,
    dial_state: &dial::DialState,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
) {
    let end_msgs = release_cast(playlist, play_started);
    // loungeScreenDisconnected drops the senders.
    sender_ids.clear();
    set_cast_ctt(dial_state, None, sabr::stream::clear_pins);
    dial_state.running.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = mpd_client.stop().await {
        tracing::warn!("[player] release: mpd stop failed: {e}");
    }
    if let Err(e) = mpd_client.clear().await {
        tracing::warn!("[player] release: mpd clear failed: {e}");
    }
    for msg in end_msgs {
        state_tx.send(msg).await.ok();
    }
}

/// SetPlaylist's state change before its song loads: the new queue, the
/// cast's ctt and params for `video_id` (earlier per-video tokens are
/// forgotten), and that ctt for `/stream` (a new ctt drops the old pins).
fn adopt_playlist(
    playlist: &mut PlaylistState,
    dial_state: &dial::DialState,
    video_id: &str,
    video_ids: Vec<String>,
    index: usize,
    list_id: String,
    auth: cast_tokens::VideoAuth,
) {
    playlist.video_ids = video_ids;
    playlist.current_index = index;
    playlist.list_id = list_id;
    playlist.current_duration = 0; // Reset — will be set by play_video after InnerTube resolve
    playlist.current_title.clear();
    // New track — reset last_state so NowPlaying is sent (nowPlayingChanged).
    // Without this, Playing→Loading would suppress NowPlaying since prev==Playing.
    playlist.last_state = messages::PlayerState::Idle;
    adopt_cast_auth(playlist, dial_state, video_id, auth);
}

/// SetVideo's state change before its song loads (see `adopt_playlist`).
fn adopt_video(playlist: &mut PlaylistState, dial_state: &dial::DialState, video_id: &str, ctt: Option<String>) {
    playlist.video_ids = vec![video_id.to_owned()];
    playlist.current_index = 0;
    playlist.list_id.clear();
    playlist.current_duration = 0;
    playlist.current_title.clear();
    playlist.last_state = messages::PlayerState::Idle; // New track — ensure NowPlaying is sent
    adopt_cast_auth(playlist, dial_state, video_id, cast_tokens::VideoAuth { ctt, params: None });
}

/// The phone's tokens start a new chain: only `video_id` has a known ctt.
fn adopt_cast_auth(
    playlist: &mut PlaylistState,
    dial_state: &dial::DialState,
    video_id: &str,
    auth: cast_tokens::VideoAuth,
) {
    if auth.ctt.is_some() {
        tracing::info!("[player] ctt received from cast session");
    }
    let ctt = auth.ctt.clone();
    playlist.failed_in_row = 0;
    playlist.tokens.clear();
    playlist.tokens.remember(video_id, auth);
    set_cast_ctt(dial_state, video_ctt(video_id, ctt), sabr::stream::clear_pins);
}

/// Stopped (0) at the full duration: the song ended. Only onStateChange,
/// no nowPlaying, as the reference does when the previous state was playing.
fn song_ended_message(playlist: &PlaylistState) -> messages::OutgoingMessage {
    let duration = messages::fmt_time(playlist.current_duration as f64);
    messages::OutgoingMessage::new("onStateChange")
        .with_arg("state", "0")
        .with_arg("currentTime", &duration)
        .with_arg("duration", &duration)
        .with_arg("loadedTime", &duration)
        .with_arg("seekableStartTime", "0")
        .with_arg("seekableEndTime", &duration)
        .with_arg("cpn", &playlist.cpn)
}

/// Unstarted (-1) at position 0: unlike Stopped (0) at the full duration,
/// the app does not read it as "track ended" and advance the queue.
fn stream_failed_message(playlist: &PlaylistState) -> messages::OutgoingMessage {
    let duration = messages::fmt_time(playlist.current_duration as f64);
    messages::OutgoingMessage::new("onStateChange")
        .with_arg("state", messages::PlayerState::Idle.as_str())
        .with_arg("currentTime", "0")
        .with_arg("duration", &duration)
        .with_arg("loadedTime", "0")
        .with_arg("seekableStartTime", "0")
        .with_arg("seekableEndTime", &duration)
        .with_arg("cpn", &playlist.cpn)
}

fn current_video_id(playlist: &PlaylistState) -> String {
    playlist
        .video_ids
        .get(playlist.current_index)
        .cloned()
        .unwrap_or_default()
}

/// Start the pre-roll if the player is idle. Never fails the launch: MPD
/// trouble only costs the early wake.
async fn start_preroll(
    mpd: &mut mpd::MpdClient,
    preroll: &mut preroll::PreRoll,
    play_started: bool,
    silence_url: &str,
) {
    let status = match mpd.status().await {
        Ok(status) => status,
        Err(e) => {
            tracing::warn!("[preroll] no pre-roll, mpd status failed: {e}");
            return;
        }
    };
    if !preroll.should_start(play_started, &status.state, tokio::time::Instant::now()) {
        tracing::debug!(
            "[preroll] not started: pre-roll running or cooling down, or player busy ({})",
            status.state
        );
        return;
    }
    if let Err(e) = preroll.start(mpd, silence_url).await {
        tracing::warn!("[preroll] pre-roll failed to start: {e}");
    }
}

/// Resolves when the pre-roll's deadline passes; never without a pre-roll.
async fn preroll_expired(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// End a pre-roll no song replaced (timeout, release) and stop MPD.
/// `play_started` is cleared so that stop is not read as a song end.
async fn stop_preroll(
    mpd: &mut mpd::MpdClient,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    why: &str,
) {
    *play_started = false;
    match playlist.preroll.stop(mpd).await {
        Ok(Some(played)) => tracing::info!(
            "[preroll] pre-roll stopped after {:.1} s: {why}; MPD stopped",
            played.as_secs_f64()
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!("[preroll] pre-roll {why}; stopping MPD failed: {e}"),
    }
}

/// For callers that stop and clear MPD themselves.
fn end_preroll(preroll: &mut preroll::PreRoll, why: &str) {
    if let Some(played) = preroll.forget() {
        tracing::info!("[preroll] pre-roll ended after {:.1} s: {why}", played.as_secs_f64());
    }
}

/// Resolve an audio stream via InnerTube and play it through MPD.
///
/// Returns `true` if playback started, `false` if no stream was available.
/// If InnerTube can't resolve the track, reports error state to YouTube
/// and returns `false`. Does NOT auto-skip — let YouTube handle navigation.
async fn play_video(
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    video_id: &str,
    start_position: f64,
    yt_session: Option<&music_session::YtMusicSession>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    play_video_with(mpd, state_tx, playlist, video_id, start_position, |video_id, ctt, list_id| async move {
        innertube::resolve_stream(&video_id, ctt.as_deref(), list_id.as_deref(), yt_session).await
    })
    .await
}

/// [`play_video`] with the InnerTube resolve given as `resolve(video_id,
/// ctt, list_id)`.
async fn play_video_with<F, Fut>(
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    video_id: &str,
    start_position: f64,
    resolve: F,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>
where
    F: FnOnce(String, Option<String>, Option<String>) -> Fut,
    Fut: std::future::Future<Output = innertube::Result<Option<innertube::StreamInfo>>>,
{
    // Tell YouTube we're loading — app shows spinner immediately
    send_state(mpd, state_tx, playlist, messages::PlayerState::Loading).await?;

    let list_id = (!playlist.list_id.is_empty()).then(|| playlist.list_id.clone());
    let stream = resolve(video_id.to_owned(), resolve_ctt(playlist, video_id), list_id).await?;

    match stream {
        Some(info) => {
            tracing::info!(
                "[player] Playing: {} [{}s] ({})",
                info.title,
                info.duration,
                video_id
            );

            playlist.current_title = info.title;
            playlist.current_duration = info.duration;

            playlist.preroll.play_song(mpd, &info.stream_url).await?;

            if start_position > 0.0 {
                // Non-fatal: MPD may not have buffered enough data to seek yet
                if let Err(e) = mpd.seekcur(start_position).await {
                    tracing::warn!("[player] seek to {start_position}s failed (non-fatal): {e}");
                }
            }

            send_state(mpd, state_tx, playlist, messages::PlayerState::Playing).await?;
            Ok(true)
        }
        None => {
            tracing::error!("[player] No stream for {video_id}, reporting error to YouTube");
            // Tell YouTube we couldn't play it — phone will handle skip/retry
            send_state(mpd, state_tx, playlist, messages::PlayerState::Stopped).await?;
            Ok(false)
        }
    }
}

fn mpd_state_to_player_state(state: &str) -> messages::PlayerState {
    match state {
        "play" => messages::PlayerState::Playing,
        "pause" => messages::PlayerState::Paused,
        "stop" => messages::PlayerState::Stopped,
        _ => messages::PlayerState::Idle,
    }
}

/// Send state messages to the Lounge session, matching yt-cast-receiver's
/// conditional logic for when NowPlaying is included.
///
/// Reference rules (YouTubeApp.ts #handlePlayerStateEvent):
/// - OnStateChange: ALWAYS sent on status or position change
/// - NowPlaying + OnHasPreviousNextChanged: ONLY sent when:
///   (a) video/queue changed (nowPlayingChanged), OR
///   (b) status changed AND previous status was NOT Playing
///
/// This means Pause (Playing→Paused) and Stop (Playing→Stopped) only send
/// OnStateChange — NOT NowPlaying. Sending extra NowPlaying confuses the app.
async fn send_state(
    mpd: &mut mpd::MpdClient,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    state: messages::PlayerState,
) -> CmdResult {
    let status = mpd.status().await?;
    let (_, elapsed) = reported_playback(playlist.preroll.is_active(), &status);
    let duration = playlist.current_duration as f64;
    let vid = current_video_id(playlist);

    // loadedTime: report fully buffered when playing/paused/loading, 0 otherwise
    let loaded_time = match state {
        messages::PlayerState::Playing
        | messages::PlayerState::Paused
        | messages::PlayerState::Loading => duration,
        _ => 0.0,
    };

    let prev = playlist.last_state;
    let status_changed = state != prev;

    // Reference condition: send NowPlaying when status changed AND
    // previous was NOT Playing. This avoids extra NowPlaying on
    // Pause (Playing→Paused) and Stop (Playing→Stopped).
    let send_now_playing = status_changed && prev != messages::PlayerState::Playing;

    tracing::info!(
        "[state] -> state={} currentTime={:.1} duration={:.1} vid={} cpn={} nowPlaying={}",
        state.as_str(), elapsed, duration, &vid, &playlist.cpn, send_now_playing
    );

    // Update last_state for next comparison
    playlist.last_state = state;

    // onStateChange — ALWAYS sent
    state_tx
        .send(
            messages::OutgoingMessage::new("onStateChange")
                .with_arg("state", state.as_str())
                .with_arg("currentTime", messages::fmt_time(elapsed))
                .with_arg("duration", messages::fmt_time(duration))
                .with_arg("loadedTime", messages::fmt_time(loaded_time))
                .with_arg("seekableStartTime", "0")
                .with_arg("seekableEndTime", messages::fmt_time(duration))
                .with_arg("cpn", &playlist.cpn),
        )
        .await
        .ok();

    // nowPlaying + nav — only when condition matches reference
    if send_now_playing {
        let mut np = messages::OutgoingMessage::new("nowPlaying")
            .with_arg("currentTime", messages::fmt_time(elapsed))
            .with_arg("duration", messages::fmt_time(duration))
            .with_arg("cpn", &playlist.cpn)
            .with_arg("loadedTime", messages::fmt_time(loaded_time))
            .with_arg("videoId", &vid)
            .with_arg("state", state.as_str())
            .with_arg("seekableStartTime", "0")
            .with_arg("seekableEndTime", messages::fmt_time(duration));

        if !playlist.list_id.is_empty() {
            np = np.with_arg("listId", &playlist.list_id);
        }
        np = np.with_arg("currentIndex", playlist.current_index.to_string());
        // The current video's own tokens, never another video's.
        let auth = playlist.tokens.auth_for(&vid);
        if let Some(ref ctt) = auth.ctt {
            np = np.with_arg("ctt", ctt);
        }
        if let Some(ref params) = auth.params {
            np = np.with_arg("params", params);
        }

        state_tx.send(np).await.ok();

        // Tell phone whether next/previous buttons should be enabled
        let has_prev = playlist.current_index > 0;
        let has_next = playlist.current_index + 1 < playlist.video_ids.len();
        state_tx
            .send(
                messages::OutgoingMessage::new("onHasPreviousNextChanged")
                    .with_arg("hasPrevious", if has_prev { "true" } else { "false" })
                    .with_arg("hasNext", if has_next { "true" } else { "false" }),
            )
            .await
            .ok();
    }

    Ok(())
}

async fn send_volume(
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    volume: u32,
) -> CmdResult {
    state_tx
        .send(
            messages::OutgoingMessage::new("onVolumeChanged")
                .with_arg("volume", volume.to_string())
                .with_arg("muted", if volume == 0 { "true" } else { "false" }),
        )
        .await
        .ok();
    Ok(())
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    #[test]
    fn a_volume_set_by_the_speaker_adapter_is_reported_to_the_phone() {
        // The adapter gives MPD the speaker's volume (cast start, buttons on the speaker).
        assert_eq!(volume_to_report(true, Some(50), 40), Some(40));
        assert_eq!(volume_to_report(true, None, 40), Some(40));
    }

    #[test]
    fn the_phones_own_volume_is_not_echoed() {
        assert_eq!(volume_to_report(true, Some(40), 40), None);
    }

    #[test]
    fn no_volume_is_reported_without_a_phone() {
        assert_eq!(volume_to_report(false, Some(50), 40), None);
    }

    fn playlist(len: usize, index: usize) -> PlaylistState {
        PlaylistState {
            video_ids: (0..len).map(|i| format!("vid{i}")).collect(),
            current_index: index,
            current_duration: 200,
            ..Default::default()
        }
    }

    #[test]
    fn release_wins_over_song_end_in_the_same_wake_up() {
        let both = mpd::IdleEvent { player: true, release: true, mixer: false };
        assert_eq!(idle_plan(both, true, false), IdlePlan { release: true, check_song_end: false });
    }

    #[test]
    fn player_change_checks_song_end_only_while_playing() {
        let player = mpd::IdleEvent { player: true, release: false, mixer: false };
        assert_eq!(idle_plan(player, true, false), IdlePlan { release: false, check_song_end: true });
        assert_eq!(idle_plan(player, false, false), IdlePlan { release: false, check_song_end: false });
    }

    #[test]
    fn the_pre_roll_silence_never_ends_a_song() {
        let player = mpd::IdleEvent { player: true, release: false, mixer: false };
        // Even if a stray resume from the phone set play_started.
        assert!(!idle_plan(player, true, true).check_song_end);
        assert!(!idle_plan(player, false, true).check_song_end);
    }

    #[test]
    fn a_held_song_has_not_ended() {
        let status = |state: &str| mpd::MpdStatus { state: state.into(), elapsed: Some(0.0), duration: None, volume: 50 };
        assert!(!song_ended(&status("pause")));
        assert!(!song_ended(&status("play")));
        assert!(song_ended(&status("stop")));
    }

    #[test]
    fn the_pre_roll_silence_is_reported_as_nothing_playing() {
        let status = |state: &str| mpd::MpdStatus {
            state: state.into(),
            elapsed: Some(3.2),
            duration: None,
            volume: 50,
        };
        assert_eq!(reported_playback(true, &status("play")), (messages::PlayerState::Stopped, 0.0));
        assert_eq!(reported_playback(true, &status("pause")), (messages::PlayerState::Stopped, 0.0));
        assert_eq!(reported_playback(false, &status("play")), (messages::PlayerState::Playing, 3.2));
        assert_eq!(reported_playback(false, &status("pause")), (messages::PlayerState::Paused, 3.2));
    }

    #[test]
    fn release_ends_the_cast_and_leaves_nothing_to_advance() {
        let mut pl = playlist(3, 0);
        pl.list_id = "PL1".into();
        let mut play_started = true;

        let msgs = release_cast(&mut pl, &mut play_started);

        // The MPD stop that follows must not look like a song end.
        assert!(!play_started);
        assert!(pl.video_ids.is_empty());
        assert_eq!(pl.current_index, 0);
        assert!(pl.list_id.is_empty());
        assert!(!idle_plan(mpd::IdleEvent { player: true, release: false, mixer: false }, play_started, false).check_song_end);
        // Fresh state for the next cast: NowPlaying will be sent again.
        assert_eq!(pl.last_state, messages::PlayerState::Idle);

        let cmds: Vec<&str> = msgs.iter().map(|m| m.command.as_str()).collect();
        assert_eq!(cmds, vec!["nowPlaying", "loungeScreenDisconnected"]);
        assert!(msgs.iter().all(|m| m.args.is_empty()));
    }

    #[test]
    fn release_ends_a_cast_only_while_one_is_active() {
        assert_eq!(release_action(true, &PlaylistState::default()), ReleaseAction::EndCast);
        assert_eq!(release_action(false, &playlist(1, 0)), ReleaseAction::EndCast);
        assert_eq!(release_action(false, &PlaylistState::default()), ReleaseAction::Ignore);
    }

    #[test]
    fn release_before_any_song_stops_only_the_silence() {
        // No song has started, so the phone keeps its cast, connected or
        // still connecting: when its song arrives the speaker is taken
        // again (the cast wins), and the cooldown stops a release loop.
        let pre_rolling = PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() };
        assert_eq!(release_action(false, &pre_rolling), ReleaseAction::StopPreRoll);
        assert_eq!(release_action(true, &pre_rolling), ReleaseAction::StopPreRoll);
    }

    #[test]
    fn position_is_reported_only_for_a_started_song() {
        assert!(reports_position(true, &playlist(1, 0)));
        assert!(!reports_position(false, &playlist(1, 0)));
        assert!(!reports_position(true, &PlaylistState::default()));
        let mut pre_rolling = playlist(1, 0);
        pre_rolling.preroll = preroll::PreRoll::running();
        assert!(!reports_position(true, &pre_rolling));
    }

    #[tokio::test]
    async fn pre_roll_deadline_resolves_only_once_it_passes() {
        use std::time::Duration;
        use tokio::time::{timeout, Instant};
        assert!(timeout(Duration::from_millis(50), preroll_expired(None)).await.is_err());
        let soon = Instant::now() + Duration::from_millis(300);
        assert!(timeout(Duration::from_millis(50), preroll_expired(Some(soon))).await.is_err());
        assert!(timeout(Duration::from_secs(1), preroll_expired(Some(soon))).await.is_ok());
        let past = Instant::now() - Duration::from_millis(1);
        assert!(timeout(Duration::from_millis(50), preroll_expired(Some(past))).await.is_ok());
    }

    #[test]
    fn ending_the_cast_ends_its_pre_roll() {
        let mut pl = PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() };
        release_cast(&mut pl, &mut false);
        assert!(!pl.preroll.is_active());
    }

    #[test]
    fn natural_end_advances_or_finishes() {
        assert_eq!(stop_action(None, &mut playlist(3, 0)), StopAction::Advance(1));
        assert_eq!(stop_action(None, &mut playlist(3, 2)), StopAction::Finished);
    }

    #[test]
    fn a_failed_track_is_skipped() {
        let mut pl = playlist(3, 0);
        assert_eq!(
            stop_action(Some("nsig decode failed".into()), &mut pl),
            StopAction::SkipFailed { next: 1, reason: "nsig decode failed".into() }
        );
    }

    #[test]
    fn a_failed_last_track_does_not_advance() {
        assert_eq!(
            stop_action(Some("UNPLAYABLE".into()), &mut playlist(3, 2)),
            StopAction::StreamFailed("UNPLAYABLE".into())
        );
    }

    #[test]
    fn three_failed_tracks_in_a_row_stop_the_queue() {
        let mut pl = playlist(6, 0);
        for expected_next in [1, 2] {
            match stop_action(Some("UNPLAYABLE".into()), &mut pl) {
                StopAction::SkipFailed { next, .. } => assert_eq!(next, expected_next),
                other => panic!("expected a skip, got {other:?}"),
            }
            pl.current_index = expected_next;
        }
        assert_eq!(stop_action(Some("UNPLAYABLE".into()), &mut pl), StopAction::StreamFailed("UNPLAYABLE".into()));
        assert_eq!(pl.failed_in_row, MAX_FAILED_IN_ROW);
    }

    #[test]
    fn a_played_track_resets_the_failure_count() {
        let mut pl = playlist(6, 0);
        assert!(matches!(stop_action(Some("x".into()), &mut pl), StopAction::SkipFailed { next: 1, .. }));
        pl.current_index = 1;
        assert!(matches!(stop_action(Some("x".into()), &mut pl), StopAction::SkipFailed { next: 2, .. }));
        pl.current_index = 2;
        assert_eq!(stop_action(None, &mut pl), StopAction::Advance(3));
        assert_eq!(pl.failed_in_row, 0);
        pl.current_index = 3;
        assert!(matches!(stop_action(Some("x".into()), &mut pl), StopAction::SkipFailed { next: 4, .. }));
    }

    #[test]
    fn a_song_end_is_reported_as_stopped_at_its_duration() {
        let msg = song_ended_message(&playlist(3, 0));
        let arg = |k: &str| msg.args.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let duration = messages::fmt_time(200.0);
        assert_eq!(msg.command, "onStateChange");
        assert_eq!(arg("state"), Some("0"));
        assert_eq!(arg("currentTime"), Some(duration.as_str()));
        assert_eq!(arg("loadedTime"), Some(duration.as_str()));
    }

    #[test]
    fn stream_failure_is_reported_as_unstarted_at_zero() {
        let msg = stream_failed_message(&playlist(3, 0));
        let arg = |k: &str| msg.args.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(msg.command, "onStateChange");
        assert_eq!(arg("state"), Some(messages::PlayerState::Idle.as_str()));
        assert_eq!(arg("currentTime"), Some("0"));
        assert_eq!(arg("loadedTime"), Some("0"));
    }
}

#[cfg(all(test, not(feature = "mock-mpd")))]
mod preroll_wiring_tests {
    use super::*;
    use crate::mpd::fake::fake_mpd;

    const SILENCE: &str = "http://localhost:8008/silence";
    const STATUS_PLAY: &str = "state: play\nvolume: 50\nelapsed: 3.0\nOK\n";
    const STATUS_STOP: &str = "state: stop\nvolume: 50\nOK\n";

    fn pre_rolling() -> PlaylistState {
        PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() }
    }

    /// Run one phone command against a scripted MPD; returns the MPD
    /// commands sent and the messages for the phone.
    async fn command(
        cmd: lounge::PlayerCommand,
        playlist: &mut PlaylistState,
        play_started: &mut bool,
        script: Vec<(&'static str, &'static str)>,
    ) -> (Vec<String>, Vec<messages::OutgoingMessage>) {
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        command_on(&dial_state, cmd, playlist, play_started, script).await
    }

    /// `command` with the receiver's shared state given.
    async fn command_on(
        dial_state: &Arc<dial::DialState>,
        cmd: lounge::PlayerCommand,
        playlist: &mut PlaylistState,
        play_started: &mut bool,
        script: Vec<(&'static str, &'static str)>,
    ) -> (Vec<String>, Vec<messages::OutgoingMessage>) {
        // Commands such as RemoteDisconnected clear the real pin store.
        let _lock = sabr::stream::test_support::global_pins_lock();
        let (port, server) = fake_mpd(script).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        let (state_tx, mut state_rx) = tokio::sync::mpsc::channel(16);
        handle_command(cmd, &mut mpd, &state_tx, playlist, play_started, dial_state, None)
            .await
            .unwrap();
        drop(mpd);
        let mut sent = Vec::new();
        while let Ok(msg) = state_rx.try_recv() {
            sent.push(msg);
        }
        (server.await.unwrap(), sent)
    }

    fn arg<'a>(msg: &'a messages::OutgoingMessage, key: &str) -> Option<&'a str> {
        msg.args.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_changed_ctt_clears_the_pins() {
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let clears = std::cell::Cell::new(0);
        let clear = || clears.set(clears.get() + 1);

        assert!(set_cast_ctt(&dial_state, video_ctt("aaaaaaaaaaa", Some("ctt-a".into())), clear));
        assert!(!set_cast_ctt(&dial_state, video_ctt("aaaaaaaaaaa", Some("ctt-a".into())), clear));
        assert!(set_cast_ctt(&dial_state, video_ctt("bbbbbbbbbbb", Some("ctt-b".into())), clear));
        assert!(set_cast_ctt(&dial_state, None, clear));
        assert_eq!(clears.get(), 3);
        assert_eq!(*dial_state.ctt.read().unwrap(), None);
    }

    fn auth(ctt: &str) -> cast_tokens::VideoAuth {
        cast_tokens::VideoAuth { ctt: Some(ctt.into()), params: Some(format!("{ctt}-params")) }
    }

    /// A three-song queue cast with `ctt-a` for its first song, playing.
    fn cast_queue(dial_state: &dial::DialState) -> PlaylistState {
        let mut playlist = PlaylistState::default();
        let ids = vec!["aaaaaaaaaaa".to_owned(), "bbbbbbbbbbb".to_owned(), "ccccccccccc".to_owned()];
        adopt_playlist(&mut playlist, dial_state, "aaaaaaaaaaa", ids, 0, "RDAMVMaaaaaaaaaaa".into(), auth("ctt-a"));
        playlist
    }

    fn next_answer(
        playlist: &PlaylistState,
        video_id: &str,
        ctt: Option<&str>,
    ) -> PrefetchResult {
        PrefetchResult {
            generation: playlist.tokens.generation(),
            video_id: "aaaaaaaaaaa".into(),
            expected_next: "bbbbbbbbbbb".into(),
            outcome: Ok(Some((
                watch_next::NextSource::NextVideoRenderer,
                watch_next::NextVideo {
                    video_id: video_id.into(),
                    ctt: ctt.map(str::to_owned),
                    params: Some("next-params".into()),
                    playlist_id: Some("RDAMVMaaaaaaaaaaa".into()),
                    index: Some(1),
                },
            ))),
        }
    }

    /// /next answered `ctt-b` for the queue's second song.
    fn store_next_ctt(playlist: &mut PlaylistState) {
        let answer = next_answer(playlist, "bbbbbbbbbbb", Some("ctt-b"));
        assert_eq!(apply_prefetch(playlist, answer), PrefetchVerdict::Stored);
    }

    #[test]
    fn next_is_asked_once_per_playing_song_with_its_own_ctt() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);

        assert!(prefetch_job(&mut playlist, false).is_none(), "not before the song plays");
        let job = prefetch_job(&mut playlist, true).expect("a playing song with a ctt");
        assert_eq!(
            (job.video_id.as_str(), job.expected_next.as_str(), job.ctt.as_str(), job.index),
            ("aaaaaaaaaaa", "bbbbbbbbbbb", "ctt-a", 0)
        );
        assert_eq!(job.params.as_deref(), Some("ctt-a-params"));
        assert_eq!(job.list_id, "RDAMVMaaaaaaaaaaa");
        assert!(prefetch_job(&mut playlist, true).is_none(), "one request per song");

        // The last song has no next; a song without its own ctt asks nothing.
        playlist.current_index = 2;
        assert!(prefetch_job(&mut playlist, true).is_none());
        let mut no_ctt = cast_queue(&dial_state);
        no_ctt.tokens.clear();
        assert!(prefetch_job(&mut no_ctt, true).is_none());
    }

    #[test]
    fn advancing_uses_the_next_videos_own_ctt_from_next() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        assert_eq!(dial_state.ctt_for("aaaaaaaaaaa").as_deref(), Some("ctt-a"));

        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b"));
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stored);
        let video_id = adopt_index(&mut playlist, &dial_state, 1, "Advancing");

        assert_eq!(video_id, "bbbbbbbbbbb");
        assert_eq!(dial_state.ctt_for("bbbbbbbbbbb").as_deref(), Some("ctt-b"));
        assert_eq!(dial_state.ctt_for("aaaaaaaaaaa"), None, "the first song's ctt is no longer offered");
        assert_eq!(
            playlist.tokens.auth_for("bbbbbbbbbbb"),
            cast_tokens::VideoAuth { ctt: Some("ctt-b".into()), params: Some("next-params".into()) }
        );
        // The chain goes on: the new song asks /next with its own ctt.
        let job = prefetch_job(&mut playlist, true).unwrap();
        assert_eq!((job.video_id.as_str(), job.ctt.as_str()), ("bbbbbbbbbbb", "ctt-b"));
    }

    #[test]
    fn without_a_ctt_from_next_the_stale_ctt_is_never_sent_for_another_video() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let failed = |playlist: &PlaylistState| PrefetchResult {
            outcome: Err("/next HTTP 400".into()),
            ..next_answer(playlist, "bbbbbbbbbbb", None)
        };
        let none = |playlist: &PlaylistState| PrefetchResult { outcome: Ok(None), ..next_answer(playlist, "x", None) };
        type Answer = fn(&PlaylistState) -> PrefetchResult;
        let cases: [(Answer, PrefetchVerdict); 4] = [
            (|p| next_answer(p, "bbbbbbbbbbb", None), PrefetchVerdict::NoToken),
            (|p| next_answer(p, "zzzzzzzzzzz", Some("ctt-z")), PrefetchVerdict::Mismatch),
            (failed, PrefetchVerdict::NoNext),
            (none, PrefetchVerdict::NoNext),
        ];
        for (answer, verdict) in cases {
            let mut playlist = cast_queue(&dial_state);
            let answer = answer(&playlist);
            assert_eq!(apply_prefetch(&mut playlist, answer), verdict);
            adopt_index(&mut playlist, &dial_state, 1, "Advancing");
            assert_eq!(dial_state.ctt_for("bbbbbbbbbbb"), None, "{verdict:?}");
            assert_eq!(*dial_state.ctt.read().unwrap(), None, "{verdict:?}");
            assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
            assert_eq!(playlist.tokens.auth_for("zzzzzzzzzzz"), cast_tokens::VideoAuth::default());
        }
    }

    #[test]
    fn a_next_answer_from_an_earlier_cast_is_dropped() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b"));
        // A new setPlaylist arrives before the answer.
        playlist = {
            let mut p = playlist;
            let ids = vec!["aaaaaaaaaaa".to_owned(), "bbbbbbbbbbb".to_owned()];
            adopt_playlist(&mut p, &dial_state, "aaaaaaaaaaa", ids, 0, String::new(), auth("ctt-a2"));
            p
        };
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale);
        assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
    }

    #[test]
    fn next_and_previous_use_each_videos_own_ctt() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        store_next_ctt(&mut playlist);

        adopt_index(&mut playlist, &dial_state, 1, "Next");
        assert_eq!(dial_state.ctt_for("bbbbbbbbbbb").as_deref(), Some("ctt-b"));
        adopt_index(&mut playlist, &dial_state, 0, "Previous");
        assert_eq!(dial_state.ctt_for("aaaaaaaaaaa").as_deref(), Some("ctt-a"));
        adopt_index(&mut playlist, &dial_state, 2, "Next");
        assert_eq!(*dial_state.ctt.read().unwrap(), None, "no ctt known for the third song");
    }

    #[tokio::test]
    async fn now_playing_echoes_the_current_videos_own_ctt_and_params() {
        const STATUS: &str = "state: play\nvolume: 50\nelapsed: 1.0\nOK\n";
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = {
            let _lock = global_pins_lock();
            let mut playlist = cast_queue(&dial_state);
            store_next_ctt(&mut playlist);
            playlist
        };
        let script = || vec![("status", STATUS), ("status", STATUS)];
        let now_playing = |sent: Vec<messages::OutgoingMessage>| {
            sent.into_iter().find(|m| m.command == "nowPlaying").expect("nowPlaying sent")
        };

        let (_, sent) = command(lounge::PlayerCommand::GetNowPlaying, &mut playlist, &mut true, script()).await;
        let np = now_playing(sent);
        assert_eq!((arg(&np, "ctt"), arg(&np, "params")), (Some("ctt-a"), Some("ctt-a-params")));

        {
            let _lock = global_pins_lock();
            adopt_index(&mut playlist, &dial_state, 1, "Advancing");
        }
        let (_, sent) = command(lounge::PlayerCommand::GetNowPlaying, &mut playlist, &mut true, script()).await;
        let np = now_playing(sent);
        assert_eq!(arg(&np, "videoId"), Some("bbbbbbbbbbb"));
        assert_eq!((arg(&np, "ctt"), arg(&np, "params")), (Some("ctt-b"), Some("next-params")));

        // A video without its own tokens echoes none, not the first song's.
        {
            let _lock = global_pins_lock();
            adopt_index(&mut playlist, &dial_state, 2, "Advancing");
        }
        let (_, sent) = command(lounge::PlayerCommand::GetNowPlaying, &mut playlist, &mut true, script()).await;
        let np = now_playing(sent);
        assert_eq!((arg(&np, "ctt"), arg(&np, "params")), (None, None));
    }

    use sabr::stream::test_support::{global_has_pin, global_pins_lock, pin_global};

    #[tokio::test]
    async fn remote_disconnect_keeps_a_playing_queue_and_its_ctt_chain() {
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = {
            let _lock = global_pins_lock();
            let queue = cast_queue(&dial_state);
            pin_global("aaaaaaaaaaa", "ctt-a", "u", 10);
            queue
        };
        let mut play_started = true;
        assert!(plays_on_without_sender(play_started, &playlist));

        // No MPD command at all: the song plays on.
        let (seen, sent) = command_on(
            &dial_state,
            lounge::PlayerCommand::RemoteDisconnected { theme: "m".into() },
            &mut playlist,
            &mut play_started,
            vec![],
        )
        .await;
        assert!(seen.is_empty(), "MPD untouched: {seen:?}");
        assert!(sent.is_empty());
        assert!(play_started, "the song end still auto-advances");
        assert_eq!(playlist.video_ids.len(), 3);
        assert_eq!(playlist.list_id, "RDAMVMaaaaaaaaaaa");
        assert_eq!(dial_state.ctt_for("aaaaaaaaaaa").as_deref(), Some("ctt-a"));
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"));

        // The /next chain carries on as with a sender connected.
        let _lock = global_pins_lock();
        assert!(global_has_pin("aaaaaaaaaaa"), "the playing song keeps its pins");
        assert!(prefetch_job(&mut playlist, play_started).is_some());
        store_next_ctt(&mut playlist);
        assert_eq!(stop_action(None, &mut playlist), StopAction::Advance(1));
        adopt_index(&mut playlist, &dial_state, 1, "Advancing");
        assert_eq!(dial_state.ctt_for("bbbbbbbbbbb").as_deref(), Some("ctt-b"));
    }

    #[test]
    fn only_a_playing_song_plays_on_without_a_sender() {
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let _lock = global_pins_lock();
        let queue = cast_queue(&dial_state);
        assert!(plays_on_without_sender(true, &queue));
        assert!(!plays_on_without_sender(false, &queue), "nothing started (or a failed stream)");
        assert!(!plays_on_without_sender(true, &PlaylistState::default()), "no queue");
        assert!(!plays_on_without_sender(true, &PlaylistState { preroll: preroll::PreRoll::running(), ..cast_queue(&dial_state) }));
    }

    #[test]
    fn the_same_sender_reconnecting_takes_over_the_playing_cast() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let active = std::sync::RwLock::new("m".to_owned());
        // The iPad left; its id is still known.
        let mut sender_ids = vec!["ipad-1".to_owned()];

        remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active, false, "m", &["ipad-1".to_owned()]);

        assert_eq!(sender_ids, vec!["ipad-1".to_owned()]);
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"));
        assert_eq!(dial_state.ctt_for("aaaaaaaaaaa").as_deref(), Some("ctt-a"));
    }

    #[tokio::test]
    async fn a_new_sender_after_the_last_left_gets_none_of_the_departed_casts_tokens() {
        const STATUS: &str = "state: play\nvolume: 50\nelapsed: 1.0\nOK\n";
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let active = std::sync::RwLock::new("m".to_owned());
        for newcomer in [vec!["phone-2".to_owned()], vec![]] {
            let mut playlist = {
                let _lock = global_pins_lock();
                let mut playlist = cast_queue(&dial_state);
                store_next_ctt(&mut playlist);
                let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b2"));
                let mut sender_ids = vec!["ipad-1".to_owned()];
                remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active, false, "m", &newcomer);
                assert_eq!(*dial_state.ctt.read().unwrap(), None, "{newcomer:?}");
                assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
                assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale, "{newcomer:?}");
                assert_eq!(playlist.video_ids.len(), 3, "the queue plays on");
                playlist
            };
            let script = vec![("status", STATUS), ("status", STATUS)];
            let (_, sent) = command_on(&dial_state, lounge::PlayerCommand::GetNowPlaying, &mut playlist, &mut true, script).await;
            let np = sent.into_iter().find(|m| m.command == "nowPlaying").expect("nowPlaying sent");
            assert_eq!(arg(&np, "videoId"), Some("aaaaaaaaaaa"));
            assert_eq!((arg(&np, "ctt"), arg(&np, "params")), (None, None), "{newcomer:?}");
        }
    }

    #[test]
    fn a_sender_joining_a_connected_one_keeps_the_cast() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let active = std::sync::RwLock::new("m".to_owned());
        let mut sender_ids = vec!["phone-1".to_owned()];
        remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active, true, "m", &["phone-2".to_owned()]);
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"));
    }

    #[tokio::test]
    async fn a_cast_without_a_sender_is_forgotten_when_it_ends() {
        let _lock = global_pins_lock();
        let (port, server) = fake_mpd(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        store_next_ctt(&mut playlist);
        pin_global("aaaaaaaaaaa", "ctt-a", "u", 10);
        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b2"));
        let mut sender_ids = vec!["ipad-1".to_owned()];
        let mut play_started = true;

        end_unattended_cast(&mut mpd, &mut playlist, &mut play_started, &mut sender_ids, &dial_state).await;
        drop(mpd);

        assert_eq!(server.await.unwrap(), ["stop", "clear"]);
        assert!(!play_started);
        assert!(sender_ids.is_empty());
        assert!(playlist.video_ids.is_empty());
        assert!(playlist.list_id.is_empty());
        assert_eq!(*dial_state.ctt.read().unwrap(), None);
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), cast_tokens::VideoAuth::default());
        assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
        assert!(!global_has_pin("aaaaaaaaaaa"));
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale);
    }

    #[test]
    fn an_unplayable_queue_entry_after_a_next_mismatch_is_skipped() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        // /next proposes another video than the queue's next: not used.
        let answer = next_answer(&playlist, "zzzzzzzzzzz", Some("ctt-z"));
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Mismatch);
        assert_eq!(stop_action(None, &mut playlist), StopAction::Advance(1));
        assert_eq!(adopt_index(&mut playlist, &dial_state, 1, "Advancing"), "bbbbbbbbbbb");
        assert_eq!(*dial_state.ctt.read().unwrap(), None);

        // The queue's next, played without a ctt, is unplayable: skip on in the queue.
        let action = stop_action(Some("UNPLAYABLE (This video is not available)".into()), &mut playlist);
        assert!(matches!(action, StopAction::SkipFailed { next: 2, .. }), "{action:?}");
        assert_eq!(adopt_index(&mut playlist, &dial_state, 2, "Skipping"), "ccccccccccc");
        assert_eq!(playlist.tokens.auth_for("zzzzzzzzzzz"), cast_tokens::VideoAuth::default());
    }

    #[tokio::test]
    async fn remote_disconnect_with_nothing_playing_forgets_the_cast_ctt_params_and_pins() {
        let _lock = global_pins_lock();
        let (port, server) = fake_mpd(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        let (state_tx, _state_rx) = tokio::sync::mpsc::channel(16);
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        *dial_state.ctt.write().unwrap() = video_ctt("mainpin0001", Some("cast-ctt".into()));
        pin_global("mainpin0001", "cast-ctt", "u", 10);
        let mut playlist = PlaylistState { video_ids: vec!["mainpin0001".into()], ..PlaylistState::default() };
        playlist.tokens.remember(
            "mainpin0001",
            cast_tokens::VideoAuth { ctt: Some("cast-ctt".into()), params: Some("cast-params".into()) },
        );
        playlist.tokens.remember("mainpin0009", auth("ctt-next"));

        handle_command(
            lounge::PlayerCommand::RemoteDisconnected { theme: "m".into() },
            &mut mpd, &state_tx, &mut playlist, &mut false, &dial_state, None,
        )
        .await
        .unwrap();
        drop(mpd);
        server.await.unwrap();

        assert_eq!(*dial_state.ctt.read().unwrap(), None);
        assert_eq!(playlist.tokens.auth_for("mainpin0001"), cast_tokens::VideoAuth::default());
        assert_eq!(playlist.tokens.auth_for("mainpin0009"), cast_tokens::VideoAuth::default());
        assert!(!global_has_pin("mainpin0001"));
    }

    #[tokio::test]
    async fn a_release_by_the_speaker_forgets_the_cast_and_its_pins() {
        let _lock = global_pins_lock();
        let (port, server) = fake_mpd(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        let (state_tx, mut state_rx) = tokio::sync::mpsc::channel(16);
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        *dial_state.ctt.write().unwrap() = video_ctt("mainpin0002", Some("cast-ctt".into()));
        pin_global("mainpin0002", "cast-ctt", "u", 10);
        let mut playlist = PlaylistState::default();
        playlist.tokens.remember("mainpin0002", auth("cast-ctt"));

        let mut sender_ids = vec!["sender-device-1".to_owned()];
        end_released_cast(&mut mpd, &mut playlist, &mut true, &mut sender_ids, &dial_state, &state_tx).await;
        drop(mpd);

        assert_eq!(server.await.unwrap(), ["stop", "clear"]);
        assert!(sender_ids.is_empty(), "the released senders are not sent to /next");
        assert_eq!(*dial_state.ctt.read().unwrap(), None);
        assert_eq!(playlist.tokens.auth_for("mainpin0002"), cast_tokens::VideoAuth::default());
        assert!(!global_has_pin("mainpin0002"));
        assert_eq!(state_rx.try_recv().unwrap().command, "nowPlaying");
        assert_eq!(state_rx.try_recv().unwrap().command, "loungeScreenDisconnected");
    }

    #[test]
    fn a_new_cast_ctt_from_set_playlist_or_set_video_clears_the_pins() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = PlaylistState::default();
        let ids = || vec!["mainpin0003".to_owned()];
        let adopt = |playlist: &mut PlaylistState, ctt: &str| {
            adopt_playlist(playlist, &dial_state, "mainpin0003", ids(), 0, String::new(), auth(ctt));
        };

        adopt(&mut playlist, "ctt-a");
        pin_global("mainpin0003", "ctt-a", "u", 10);
        adopt(&mut playlist, "ctt-a");
        assert!(global_has_pin("mainpin0003"), "the same cast keeps its pins");
        adopt(&mut playlist, "ctt-b");
        assert!(!global_has_pin("mainpin0003"));

        pin_global("mainpin0004", "ctt-b", "u", 10);
        adopt_video(&mut playlist, &dial_state, "mainpin0004", Some("ctt-b".into()));
        assert!(global_has_pin("mainpin0004"));
        adopt_video(&mut playlist, &dial_state, "mainpin0004", Some("ctt-c".into()));
        assert!(!global_has_pin("mainpin0004"));
        assert_eq!(dial_state.ctt_for("mainpin0004").as_deref(), Some("ctt-c"));
        // SetVideo has no params; the earlier setPlaylist's are not echoed.
        assert_eq!(playlist.tokens.auth_for("mainpin0004"), cast_tokens::VideoAuth { ctt: Some("ctt-c".into()), params: None });
    }

    #[test]
    fn a_new_set_playlist_forgets_the_earlier_chain() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        store_next_ctt(&mut playlist);

        let ids = vec!["ccccccccccc".to_owned(), "bbbbbbbbbbb".to_owned()];
        adopt_playlist(&mut playlist, &dial_state, "ccccccccccc", ids, 0, String::new(), auth("ctt-c"));

        assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), cast_tokens::VideoAuth::default());
        assert_eq!(dial_state.ctt_for("ccccccccccc").as_deref(), Some("ctt-c"));
    }

    #[test]
    fn a_late_next_answer_from_a_released_cast_is_dropped() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b"));

        release_cast(&mut playlist, &mut true);
        let ids = vec!["aaaaaaaaaaa".to_owned(), "bbbbbbbbbbb".to_owned()];
        adopt_playlist(&mut playlist, &dial_state, "aaaaaaaaaaa", ids, 0, String::new(), auth("ctt-other"));

        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale);
        assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
    }

    #[tokio::test]
    async fn stop_from_the_phone_forgets_the_chain() {
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = {
            let _lock = global_pins_lock();
            let mut playlist = cast_queue(&dial_state);
            store_next_ctt(&mut playlist);
            playlist
        };
        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b2"));
        let script = vec![("stop", "OK\n"), ("clear", "OK\n"), ("status", STATUS_STOP)];
        let (seen, _) = command_on(&dial_state, lounge::PlayerCommand::Stop, &mut playlist, &mut true, script).await;
        assert_eq!(seen, vec!["stop", "clear", "status"]);

        assert_eq!(*dial_state.ctt.read().unwrap(), None);
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), cast_tokens::VideoAuth::default());
        assert_eq!(playlist.tokens.auth_for("bbbbbbbbbbb"), cast_tokens::VideoAuth::default());
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale);
    }

    #[test]
    fn a_song_resolves_with_its_own_ctt() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        store_next_ctt(&mut playlist);
        assert_eq!(resolve_ctt(&playlist, "aaaaaaaaaaa").as_deref(), Some("ctt-a"));
        assert_eq!(resolve_ctt(&playlist, "bbbbbbbbbbb").as_deref(), Some("ctt-b"));
        assert_eq!(resolve_ctt(&playlist, "ccccccccccc"), None);
    }

    #[test]
    fn next_is_not_asked_when_the_next_ctt_is_known_or_during_the_pre_roll() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut known = cast_queue(&dial_state);
        store_next_ctt(&mut known);
        assert!(prefetch_job(&mut known, true).is_none(), "the next video's ctt is already known");

        let mut pre_rolling = cast_queue(&dial_state);
        pre_rolling.preroll = preroll::PreRoll::running();
        assert!(prefetch_job(&mut pre_rolling, true).is_none());
        pre_rolling.preroll = preroll::PreRoll::default();
        assert!(prefetch_job(&mut pre_rolling, true).is_some(), "asked once the pre-roll is over");
    }

    #[test]
    fn a_sender_on_the_other_theme_gets_none_of_the_earlier_casts_tokens() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let answer = next_answer(&playlist, "bbbbbbbbbbb", Some("ctt-b"));

        forget_tokens_on_theme_change(&mut playlist, &dial_state, "m", "m");
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"), "same theme keeps the cast");

        forget_tokens_on_theme_change(&mut playlist, &dial_state, "m", "cl");
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), cast_tokens::VideoAuth::default());
        assert_eq!(*dial_state.ctt.read().unwrap(), None);
        assert_eq!(apply_prefetch(&mut playlist, answer), PrefetchVerdict::Stale);
    }

    #[tokio::test]
    async fn a_song_is_resolved_with_the_current_videos_own_ctt() {
        const STATUS: &str = "state: play\nvolume: 50\nelapsed: 0.0\nOK\n";
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = {
            let _lock = global_pins_lock();
            let mut playlist = cast_queue(&dial_state);
            store_next_ctt(&mut playlist);
            adopt_index(&mut playlist, &dial_state, 1, "Advancing");
            playlist
        };
        let script = vec![
            ("status", STATUS),
            ("clear", "OK\n"),
            ("addid", "Id: 5\nOK\n"),
            ("play", "OK\n"),
            ("status", STATUS),
        ];
        let (port, server) = fake_mpd(script).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        let (state_tx, _state_rx) = tokio::sync::mpsc::channel(16);
        let asked = std::sync::Mutex::new(None);

        let played = play_video_with(&mut mpd, &state_tx, &mut playlist, "bbbbbbbbbbb", 0.0, |vid, ctt, list| {
            *asked.lock().unwrap() = Some((vid.clone(), ctt, list));
            async move {
                Ok(Some(innertube::StreamInfo {
                    title: "Song".into(),
                    duration: 200,
                    stream_url: format!("http://localhost:8008/stream/{vid}"),
                }))
            }
        })
        .await
        .unwrap();
        drop(mpd);
        server.await.unwrap();

        assert!(played);
        assert_eq!(
            asked.into_inner().unwrap(),
            Some(("bbbbbbbbbbb".to_owned(), Some("ctt-b".to_owned()), Some("RDAMVMaaaaaaaaaaa".to_owned())))
        );
    }

    #[test]
    fn a_connecting_sender_is_recorded_and_its_theme_made_active() {
        let _lock = global_pins_lock();
        let (preroll_tx, _) = tokio::sync::mpsc::channel(1);
        let dial_state = dial::DialState::for_tests(preroll_tx);
        let mut playlist = cast_queue(&dial_state);
        let active = std::sync::RwLock::new("m".to_owned());
        let mut sender_ids = Vec::new();

        remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active, true, "m", &["phone-1".to_owned()]);
        assert_eq!(sender_ids, vec!["phone-1".to_owned()]);
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"));

        remote_connected(&mut sender_ids, &mut playlist, &dial_state, &active, true, "cl", &["phone-2".to_owned()]);
        assert_eq!(sender_ids, vec!["phone-2".to_owned()]);
        assert_eq!(*active.read().unwrap(), "cl");
        assert_eq!(playlist.tokens.auth_for("aaaaaaaaaaa"), cast_tokens::VideoAuth::default());
        assert_eq!(*dial_state.ctt.read().unwrap(), None);
    }

    #[test]
    fn sender_ids_follow_the_active_theme_and_its_newest_sender() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut senders = ids(&["phone-1"]);
        update_sender_ids(&mut senders, "m", "m", &[]);
        assert_eq!(senders, ids(&["phone-1"]), "a status without ids keeps the known sender");
        update_sender_ids(&mut senders, "m", "m", &ids(&["phone-2"]));
        assert_eq!(senders, ids(&["phone-2"]));
        update_sender_ids(&mut senders, "m", "cl", &[]);
        assert!(senders.is_empty(), "another theme's sender is not this one");
    }

    #[tokio::test]
    async fn phone_play_pause_and_seek_leave_the_pre_roll_alone() {
        // The silence is no song: resuming, pausing or seeking it must not
        // start a song for MPD's later stop to end.
        for cmd in [
            lounge::PlayerCommand::Play,
            lounge::PlayerCommand::Pause,
            lounge::PlayerCommand::SeekTo { position: 30.0 },
        ] {
            let name = format!("{cmd:?}");
            let mut playlist = pre_rolling();
            let mut play_started = false;
            let (seen, sent) =
                command(cmd, &mut playlist, &mut play_started, vec![("status", STATUS_PLAY)]).await;
            assert_eq!(seen, vec!["status"], "{name}: only the state is read");
            assert!(!play_started, "{name}");
            assert!(playlist.preroll.is_active(), "{name}");
            assert_eq!(sent[0].command, "onStateChange", "{name}");
            assert_eq!(arg(&sent[0], "state"), Some(messages::PlayerState::Stopped.as_str()), "{name}");
            assert_eq!(arg(&sent[0], "currentTime"), Some(messages::fmt_time(0.0).as_str()), "{name}");
        }
    }

    #[tokio::test]
    async fn phone_stop_ends_the_pre_roll() {
        let mut playlist = pre_rolling();
        let (seen, _) = command(
            lounge::PlayerCommand::Stop,
            &mut playlist,
            &mut false,
            vec![("stop", "OK\n"), ("clear", "OK\n"), ("status", STATUS_STOP)],
        )
        .await;
        assert!(!playlist.preroll.is_active());
        assert_eq!(seen, vec!["stop", "clear", "status"]);
    }

    #[tokio::test]
    async fn next_at_the_end_of_the_playlist_ends_the_pre_roll() {
        let mut playlist = pre_rolling();
        let (seen, _) = command(
            lounge::PlayerCommand::Next,
            &mut playlist,
            &mut false,
            vec![("stop", "OK\n"), ("clear", "OK\n"), ("status", STATUS_STOP)],
        )
        .await;
        assert!(!playlist.preroll.is_active());
        assert_eq!(seen, vec!["stop", "clear", "status"]);
    }

    #[tokio::test]
    async fn remote_disconnect_ends_the_pre_roll() {
        let mut playlist = pre_rolling();
        let (seen, _) = command(
            lounge::PlayerCommand::RemoteDisconnected { theme: "m".into() },
            &mut playlist,
            &mut false,
            vec![("stop", "OK\n"), ("clear", "OK\n")],
        )
        .await;
        assert!(!playlist.preroll.is_active());
        assert_eq!(seen, vec!["stop", "clear"]);
    }

    /// `start_preroll` against a scripted MPD; returns the MPD commands.
    async fn start(
        preroll: &mut preroll::PreRoll,
        play_started: bool,
        script: Vec<(&'static str, &'static str)>,
    ) -> Vec<String> {
        let (port, server) = fake_mpd(script).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        start_preroll(&mut mpd, preroll, play_started, SILENCE).await;
        drop(mpd);
        server.await.unwrap()
    }

    /// The whole script a pre-roll start would run. Refusal cases use it
    /// too, so a wrong start is recorded instead of hitting a silent server.
    fn start_script(status: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![("status", status), ("clear", "OK\n"), ("addid", "Id: 5\nOK\n"), ("play", "OK\n")]
    }

    #[tokio::test]
    async fn launch_starts_the_silence_only_on_an_idle_player() {
        // A song between two tracks (stopped, but started) is not idle.
        let mut preroll = preroll::PreRoll::default();
        assert_eq!(start(&mut preroll, true, start_script(STATUS_STOP)).await, vec!["status"]);
        assert!(!preroll.is_active());

        assert_eq!(start(&mut preroll, false, start_script(STATUS_PLAY)).await, vec!["status"]);
        assert!(!preroll.is_active());

        let seen = start(&mut preroll, false, start_script(STATUS_STOP)).await;
        assert_eq!(seen, vec!["status", "clear", "addid \"http://localhost:8008/silence\"", "play"]);
        assert!(preroll.is_active());
    }

    #[tokio::test]
    async fn pre_roll_stopped_without_a_song_is_no_song_end_and_cools_down() {
        // A stray resume during the silence, then the timeout (or a release)
        // stops MPD: that stop must not read as a song end and auto-advance.
        let mut playlist = pre_rolling();
        playlist.video_ids = vec!["aaaaaaaaaaa".into()];
        let mut play_started = false;
        command(lounge::PlayerCommand::Play, &mut playlist, &mut play_started, vec![("status", STATUS_PLAY)]).await;

        let (port, server) = fake_mpd(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
        let mut mpd = mpd::MpdClient::connect("127.0.0.1", port).await.unwrap();
        // Even if something had set it, the stop clears it.
        play_started = true;
        stop_preroll(&mut mpd, &mut playlist, &mut play_started, "timed out").await;
        drop(mpd);
        assert_eq!(server.await.unwrap(), vec!["stop", "clear"]);

        assert!(!play_started);
        assert!(!playlist.preroll.is_active());
        let mpd_stopped = mpd::IdleEvent { player: true, release: false, mixer: false };
        assert!(!idle_plan(mpd_stopped, play_started, playlist.preroll.is_active()).check_song_end);

        // A further launch within the cooldown gets no silence.
        assert_eq!(start(&mut playlist.preroll, false, start_script(STATUS_STOP)).await, vec!["status"]);
        assert!(!playlist.preroll.is_active());
    }
}

#[cfg(test)]
mod log_hygiene_tests {
    // Pairing identities and session tokens must never reach the logs. The
    // needles are split with concat! so this test does not match itself.
    #[test]
    fn secrets_are_not_logged() {
        let src = include_str!("main.rs");
        for needle in [
            concat!("ctt received", ": {}"),
            concat!("Generated screen_id (cl)", ": {}"),
            concat!("Generated screen_id (m)", ": {}"),
        ] {
            assert!(!src.contains(needle), "secret-logging format string found: {needle}");
        }
    }
}

#[cfg(test)]
mod no_stored_credentials_tests {
    // The receiver never stores or uses credentials of its own: the only
    // authentication is the per-cast ctt the casting client hands over. Needles
    // are split with concat! so these tests do not match themselves.

    /// Non-test source of every `.rs` file under `src/`, by path.
    fn production_sources() -> Vec<(std::path::PathBuf, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let src = std::fs::read_to_string(&path).unwrap();
                    let prod = src.split("#[cfg(test)]").next().unwrap().to_string();
                    out.push((path, prod));
                }
            }
        }
        let mut out = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        assert!(out.len() > 10, "source walk found too few files: {}", out.len());
        out
    }

    #[test]
    fn no_oauth_or_cookie_modules_or_credentials_files() {
        for (path, src) in production_sources() {
            let file = path.file_name().unwrap();
            assert_ne!(file, "oauth.rs");
            assert_ne!(file, concat!("cook", "ies.rs"));
            for needle in [concat!("mod ", "oauth"), concat!("ytcast-", "oauth"), concat!("OAuth", "State")] {
                assert!(!src.contains(needle), "{} contains {needle}", path.display());
            }
        }
    }

    fn is_ident_byte(b: Option<&u8>) -> bool {
        b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    }

    /// The one permitted Cookie header: the fixed consent cookie on the
    /// anonymous music.youtube.com page fetch (music_session.rs only).
    const CONSENT_COOKIE_SITE: &str = concat!("\"Coo", "kie\", CONSENT_", "COOKIE)");

    /// Stored-credential violations in `src`: bearer or basic auth, any
    /// Authorization or Cookie request header (string literal in any case, or
    /// the reqwest constant), a cookie jar, and the old cookie-file plumbing.
    /// With `consent_cookie_allowed`, exactly `CONSENT_COOKIE_SITE` is exempt.
    fn credential_violations(src: &str, consent_cookie_allowed: bool) -> Vec<String> {
        let mut found = Vec::new();
        // ASCII lowercasing keeps byte offsets aligned with `src`.
        let lower = src.to_ascii_lowercase();
        for needle in [
            concat!("bear", "er"),
            concat!("basic", "_auth("),
            concat!("ytm_", "cookies"),
            concat!("ytm-", "cookies"),
            concat!("load_cookie", "_file"),
            concat!("sapi", "sid"),
            concat!("cookie", "_store("),
            concat!("cookie", "_provider("),
        ] {
            if lower.contains(needle) {
                found.push(format!("uses {needle}"));
            }
        }
        let bytes = src.as_bytes();
        for name in [concat!("authori", "zation"), concat!("coo", "kie")] {
            for (i, _) in lower.match_indices(name) {
                let end = i + name.len();
                let constant = src[i..end] == name.to_ascii_uppercase()
                    && !is_ident_byte(i.checked_sub(1).and_then(|p| bytes.get(p)))
                    && !is_ident_byte(bytes.get(end));
                let literal = i > 0
                    && bytes[i - 1] == b'"'
                    && matches!(bytes.get(end), Some(b'"') | Some(b':'));
                let consent_site = consent_cookie_allowed
                    && literal
                    && src[i - 1..].starts_with(CONSENT_COOKIE_SITE);
                if (constant || literal) && !consent_site {
                    found.push(format!("{name} header: {}", line_at(src, i)));
                }
            }
        }
        found
    }

    /// The line containing byte offset `i` (a char boundary).
    fn line_at(src: &str, i: usize) -> &str {
        let start = src[..i].rfind('\n').map_or(0, |p| p + 1);
        let end = src[i..].find('\n').map_or(src.len(), |p| i + p);
        &src[start..end]
    }

    fn is_music_session(path: &std::path::Path) -> bool {
        path.file_name().unwrap() == "music_session.rs"
    }

    #[test]
    fn no_stored_credential_headers_or_cookie_files() {
        for (path, src) in production_sources() {
            let found = credential_violations(&src, is_music_session(&path));
            assert!(found.is_empty(), "{}: {found:?}", path.display());
        }
    }

    #[test]
    fn credential_guard_catches_stored_credentials() {
        for bad in [
            "req.bearer_auth(t);",
            "req.basic_auth(u, Some(p));",
            "req.header(reqwest::header::AUTHORIZATION, format!(\"tok {t}\"));",
            "req.header(\"Authorization\", x);",
            "req.header(\"authorization\", x);",
            "req.header(\"Authorization\", x); // \u{2014}\u{2014}\u{2014}",
            "req.header(header::COOKIE, c);",
            "req.header(\"Cookie\", c);",
            "req.header(\"COOKIE\", c);",
            "let raw = \"Cookie: a=b\";",
            "std::env::var(\"YTM_COOKIES\")",
            "home.join(\"ytm-cookies.txt\")",
            "cookies::load_cookie_file(p)",
            "let sapisid = x; // \u{2014}",
            "reqwest::Client::builder().cookie_store(true)",
            // Only the exact consent site is exempt, even where it is allowed.
            "req.header(\"Cookie\", other)",
            "req.header(\"cookie\", CONSENT_COOKIE)",
            "req.header(header::COOKIE, CONSENT_COOKIE)",
            "req.header(\"Cookie\", CONSENT_COOKIES)",
        ] {
            assert!(!credential_violations(bad, true).is_empty(), "not caught: {bad}");
        }
    }

    #[test]
    fn consent_cookie_is_exempt_only_where_allowed() {
        let site = concat!(".header(\"Coo", "kie\", CONSENT_", "COOKIE)");
        assert!(credential_violations(site, true).is_empty());
        assert!(!credential_violations(site, false).is_empty());
    }

    #[test]
    fn consent_cookie_is_a_constant_used_once_in_music_session() {
        let name = concat!("CONSENT_", "COOKIE");
        for (path, src) in production_sources() {
            let uses: usize = src
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .map(|l| l.matches(name).count())
                .sum();
            if is_music_session(&path) {
                // In code: the `const` definition plus the single page-fetch header.
                assert_eq!(uses, 2, "{}", path.display());
                assert!(src.contains(&format!("const {name}: &str = \"SOCS=")), "{}", path.display());
                assert_eq!(src.matches(CONSENT_COOKIE_SITE).count(), 1);
            } else {
                assert_eq!(uses, 0, "{} uses {name}", path.display());
            }
        }
    }

    #[test]
    fn reqwest_cookie_feature_is_off() {
        let manifest = include_str!("../Cargo.toml");
        let reqwest = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("reqwest"))
            .expect("reqwest dependency");
        assert!(reqwest.contains("features"), "{reqwest}");
        assert!(!reqwest.contains(concat!("\"coo", "kies\"")), "{reqwest}");
    }

    #[test]
    fn credential_guard_accepts_ctt_and_multibyte_text() {
        for good in [
            "state.playback_cookie = Some(c.encode_to_vec());",
            "let PLAYBACK_COOKIE_LEN = 3;",
            "req.header(\"X-Goog-Visitor-Id\", vd);",
            "req.header(reqwest::header::CONTENT_TYPE, \"application/json\");",
            // Prose mentions are fine; only header sites count.
            "/// Never sends an Authorization header \u{2014} only the ctt.",
            "// no Cookie header \u{2014}\u{2014}\nlet x = \"\u{2014}\";",
        ] {
            let found = credential_violations(good, false);
            assert!(found.is_empty(), "{good}: {found:?}");
        }
    }

    #[test]
    fn login_and_test_flags_are_not_parsed() {
        let main = include_str!("main.rs").split("#[cfg(test)]").next().unwrap();
        for needle in [concat!("\"--", "login\""), concat!("\"--", "test\"")] {
            assert!(!main.contains(needle), "main.rs still parses {needle}");
        }
    }
}
