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

use std::sync::Arc;

// ---------------------------------------------------------------------------
// Playlist tracking state (local to main)
// ---------------------------------------------------------------------------

struct PlaylistState {
    video_ids: Vec<String>,
    current_index: usize,
    list_id: String,
    /// Credential transfer token from the cast session.
    ctt: Option<String>,
    /// Opaque params token from the cast session (passed through to nowPlaying).
    params: Option<String>,
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
}

impl Default for PlaylistState {
    fn default() -> Self {
        Self {
            video_ids: Vec::new(),
            current_index: 0,
            list_id: String::new(),
            ctt: None,
            params: None,
            current_title: String::new(),
            current_duration: 0,
            cpn: messages::generate_cpn(),
            last_state: messages::PlayerState::Idle,
            preroll: preroll::PreRoll::default(),
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
    // localhost like the /stream URLs, so the DIAL server's loopback check passes.
    let silence_url = format!("http://localhost:{dial_port}{}", dial::SILENCE_PATH);
    let mut position_timer = tokio::time::interval(std::time::Duration::from_secs(3));

    loop {
        tokio::select! {
            // Player commands from the Lounge protocol
            cmd = player_cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    tracing::info!("[player] Command channel closed, shutting down");
                    break;
                };

                // Track remote connection state + active session
                match &cmd {
                    lounge::PlayerCommand::RemoteConnected { theme } => {
                        tracing::info!("[player] Remote connected on theme={}, ready for commands", theme);
                        has_remote = true;
                        *active_theme.write().unwrap() = theme.clone();
                        continue;
                    }
                    lounge::PlayerCommand::RemoteDisconnected { theme } => {
                        let current = active_theme.read().unwrap().clone();
                        if *theme != current {
                            tracing::debug!("[player] Ignoring disconnect from non-active theme={}", theme);
                            continue;
                        }
                        has_remote = false;
                        // Fall through to handle_command for stop/cleanup
                    }
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

                if plan.release {
                    match release_action(has_remote, &playlist) {
                        ReleaseAction::Ignore => {
                            tracing::debug!("[player] release from adapter ignored: no active cast");
                            continue;
                        }
                        ReleaseAction::StopPreRoll => {
                            match playlist.preroll.stop(&mut mpd_client).await {
                                Ok(_) => tracing::info!("[preroll] pre-roll stopped: released by speaker (adapter)"),
                                Err(e) => tracing::warn!("[preroll] release: stopping the pre-roll failed: {e}"),
                            }
                            continue;
                        }
                        ReleaseAction::EndCast => {}
                    }
                    if playlist.preroll.is_active() {
                        tracing::info!("[preroll] pre-roll ended: released by speaker (adapter)");
                    }
                    tracing::info!("[player] released by speaker (adapter) — ending cast session");
                    has_remote = false;
                    let end_msgs = release_cast(&mut playlist, &mut play_started);
                    *dial_state.ctt.write().unwrap() = None;
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
                    continue;
                }

                if plan.check_song_end {
                    if let Ok(status) = mpd_client.status().await {
                        if status.state == "stop" {
                            play_started = false;

                            let failure = dial_state.take_stream_failure(&current_video_id(&playlist));
                            let action = stop_action(failure, &playlist);
                            if let StopAction::StreamFailed(reason) = action {
                                tracing::error!(
                                    "[player] Stream failed for {} (index {}): {}; not advancing",
                                    current_video_id(&playlist), playlist.current_index, reason
                                );
                                playlist.last_state = messages::PlayerState::Idle;
                                state_tx.send(stream_failed_message(&playlist)).await.ok();
                                continue;
                            }

                            // Tell YouTube the current track has ended BEFORE
                            // attempting auto-advance. If InnerTube fails
                            // (LOGIN_REQUIRED), YouTube already has the ended
                            // signal and will send a recovery setPlaylist fast.
                            let duration = playlist.current_duration as f64;
                            let ended_vid = current_video_id(&playlist);
                            tracing::info!(
                                "[player] Song ended: {} (index {})",
                                ended_vid, playlist.current_index
                            );
                            // Song ended: send OnStateChange only (PLAYING→STOPPED).
                            // Reference does NOT send NowPlaying when prev was PLAYING.
                            let dur_str = messages::fmt_time(duration);
                            playlist.last_state = messages::PlayerState::Stopped;
                            state_tx.send(
                                messages::OutgoingMessage::new("onStateChange")
                                    .with_arg("state", "0")
                                    .with_arg("currentTime", &dur_str)
                                    .with_arg("duration", &dur_str)
                                    .with_arg("loadedTime", &dur_str)
                                    .with_arg("seekableStartTime", "0")
                                    .with_arg("seekableEndTime", &dur_str)
                                    .with_arg("cpn", &playlist.cpn),
                            ).await.ok();

                            if let StopAction::Advance(next) = action {
                                playlist.current_index = next;
                                let video_id = playlist.video_ids[playlist.current_index].clone();
                                tracing::info!(
                                    "[player] Advancing to index {} ({})",
                                    playlist.current_index, video_id
                                );

                                // Track change — ensure NowPlaying is sent
                                playlist.last_state = messages::PlayerState::Idle;

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
                            }
                        }
                    }
                }
            }

            // DIAL launch: play silence so the speaker wakes while the phone loads
            Some(()) = preroll_rx.recv() => {
                start_preroll(&mut mpd_client, &mut playlist.preroll, play_started, &silence_url).await;
            }

            // No song came in time: stop the silence so the speaker can idle
            _ = preroll_expired(playlist.preroll.deadline()) => {
                match playlist.preroll.stop(&mut mpd_client).await {
                    Ok(_) => tracing::info!(
                        "[preroll] pre-roll timed out after {} s without a song; MPD stopped",
                        preroll::PREROLL_TIMEOUT.as_secs()
                    ),
                    Err(e) => tracing::warn!("[preroll] pre-roll timed out; stopping MPD failed: {e}"),
                }
            }

            // Position reporting + keepalive (every 3s)
            //
            // Always queries MPD status to keep the command connection alive
            // (MPD closes idle connections after 60s). Only reports to YouTube
            // when actively playing.
            _ = position_timer.tick() => {
                match mpd_client.status().await {
                    Ok(status) => {
                        if play_started && !playlist.video_ids.is_empty() {
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

            playlist.video_ids = video_ids;
            playlist.current_index = index;
            playlist.list_id = list_id;
            playlist.ctt = ctt.clone();
            playlist.params = params;
            playlist.current_duration = 0; // Reset — will be set by play_video after InnerTube resolve
            playlist.current_title.clear();
            // New track — reset last_state so NowPlaying is sent (nowPlayingChanged).
            // Without this, Playing→Loading would suppress NowPlaying since prev==Playing.
            playlist.last_state = messages::PlayerState::Idle;

            // Store ctt in shared DIAL state so the SABR handler can use it.
            if ctt.is_some() {
                tracing::info!("[player] ctt received from cast session");
            }
            *dial_state.ctt.write().unwrap() = ctt;

            *play_started = false;
            *play_started = play_video(mpd, state_tx, playlist, &video_id, current_time, yt_session).await?;
        }

        PlayerCommand::SetVideo {
            video_id,
            ctt,
            current_time,
        } => {
            tracing::info!("[player] SetVideo: {}", video_id);

            playlist.video_ids = vec![video_id.clone()];
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.ctt = ctt.clone();
            playlist.current_duration = 0;
            playlist.current_title.clear();
            playlist.last_state = messages::PlayerState::Idle; // New track — ensure NowPlaying is sent

            if ctt.is_some() {
                tracing::info!("[player] ctt received from cast session");
            }
            *dial_state.ctt.write().unwrap() = ctt;

            *play_started = false;
            *play_started = play_video(mpd, state_tx, playlist, &video_id, current_time, yt_session).await?;
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
            *play_started = false;
            end_preroll(&mut playlist.preroll, "stop from the phone");
            mpd.stop().await?;
            mpd.clear().await?;
            playlist.video_ids.clear();
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.current_title.clear();
            playlist.current_duration = 0;
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
                playlist.current_index -= 1;
                let video_id = playlist.video_ids[playlist.current_index].clone();
                tracing::info!("[player] Previous -> index {} ({})", playlist.current_index, video_id);
                playlist.last_state = messages::PlayerState::Idle; // Track change
                *play_started = false;
                *play_started = play_video(mpd, state_tx, playlist, &video_id, 0.0, yt_session).await?;
            } else {
                tracing::info!("[player] Previous: already at start");
            }
        }

        PlayerCommand::Next => {
            if playlist.current_index + 1 < playlist.video_ids.len() {
                playlist.current_index += 1;
                let video_id = playlist.video_ids[playlist.current_index].clone();
                tracing::info!("[player] Next -> index {} ({})", playlist.current_index, video_id);
                playlist.last_state = messages::PlayerState::Idle; // Track change
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

        PlayerCommand::RemoteDisconnected { .. } => {
            tracing::info!("[player] Remote disconnected, stopping playback");
            *play_started = false;
            end_preroll(&mut playlist.preroll, "remote disconnected");
            mpd.stop().await?;
            mpd.clear().await?;
            playlist.video_ids.clear();
            playlist.current_index = 0;
            playlist.list_id.clear();
            playlist.current_title.clear();
            playlist.current_duration = 0;
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
    /// The receiver could not serve the stream. Not an end of track: the
    /// app would skip ahead, so stay on this track and report the failure.
    StreamFailed(String),
    /// The track ended; play this playlist index next.
    Advance(usize),
    /// The last track ended.
    Finished,
}

fn stop_action(stream_failure: Option<String>, playlist: &PlaylistState) -> StopAction {
    match stream_failure {
        Some(reason) => StopAction::StreamFailed(reason),
        None if playlist.current_index + 1 < playlist.video_ids.len() => {
            StopAction::Advance(playlist.current_index + 1)
        }
        None => StopAction::Finished,
    }
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
fn release_action(has_remote: bool, playlist: &PlaylistState) -> ReleaseAction {
    if has_remote || !playlist.video_ids.is_empty() {
        ReleaseAction::EndCast
    } else if playlist.preroll.is_active() {
        ReleaseAction::StopPreRoll
    } else {
        ReleaseAction::Ignore
    }
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
    *playlist = PlaylistState::default();
    vec![
        messages::OutgoingMessage::new("nowPlaying"),
        messages::OutgoingMessage::new("loungeScreenDisconnected"),
    ]
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
    // Tell YouTube we're loading — app shows spinner immediately
    send_state(mpd, state_tx, playlist, messages::PlayerState::Loading).await?;

    let stream = innertube::resolve_stream(
        video_id,
        playlist.ctt.as_deref(),
        if playlist.list_id.is_empty() {
            None
        } else {
            Some(playlist.list_id.as_str())
        },
        yt_session,
    )
    .await?;

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
        if let Some(ref ctt) = playlist.ctt {
            np = np.with_arg("ctt", ctt);
        }
        if let Some(ref params) = playlist.params {
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
        let both = mpd::IdleEvent { player: true, release: true };
        assert_eq!(idle_plan(both, true, false), IdlePlan { release: true, check_song_end: false });
    }

    #[test]
    fn player_change_checks_song_end_only_while_playing() {
        let player = mpd::IdleEvent { player: true, release: false };
        assert_eq!(idle_plan(player, true, false), IdlePlan { release: false, check_song_end: true });
        assert_eq!(idle_plan(player, false, false), IdlePlan { release: false, check_song_end: false });
    }

    #[test]
    fn the_pre_roll_silence_never_ends_a_song() {
        let player = mpd::IdleEvent { player: true, release: false };
        // Even if a stray resume from the phone set play_started.
        assert!(!idle_plan(player, true, true).check_song_end);
        assert!(!idle_plan(player, false, true).check_song_end);
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
        assert!(!idle_plan(mpd::IdleEvent { player: true, release: false }, play_started, false).check_song_end);
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
    fn release_during_a_pre_roll_alone_stops_only_the_silence() {
        // The phone may still be connecting: ending its cast would break that.
        let pre_rolling = PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() };
        assert_eq!(release_action(false, &pre_rolling), ReleaseAction::StopPreRoll);
        let connected = PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() };
        assert_eq!(release_action(true, &connected), ReleaseAction::EndCast);
    }

    #[test]
    fn ending_the_cast_ends_its_pre_roll() {
        let mut pl = PlaylistState { preroll: preroll::PreRoll::running(), ..Default::default() };
        release_cast(&mut pl, &mut false);
        assert!(!pl.preroll.is_active());
    }

    #[test]
    fn natural_end_advances_or_finishes() {
        assert_eq!(stop_action(None, &playlist(3, 0)), StopAction::Advance(1));
        assert_eq!(stop_action(None, &playlist(3, 2)), StopAction::Finished);
    }

    #[test]
    fn stream_failure_never_advances() {
        assert_eq!(
            stop_action(Some("nsig decode failed".into()), &playlist(3, 0)),
            StopAction::StreamFailed("nsig decode failed".into())
        );
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
