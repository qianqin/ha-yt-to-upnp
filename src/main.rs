mod config;
mod cookies;
mod dial;
mod innertube;
mod lounge;
mod messages;
mod mpd;
mod nsig;
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

    // 4. Load YouTube cookies (for Premium auth — bypasses PoToken requirement)
    let cookie_path = storage::cookie_path(
        std::env::var("YTM_COOKIES").ok().as_deref(),
        storage::data_dir_env().as_deref(),
    );

    let yt_cookies = if std::path::Path::new(&cookie_path).exists() {
        match cookies::load_cookie_file(&cookie_path) {
            Ok(Some(c)) => {
                tracing::info!("[init] Loaded cookies from {}", cookie_path);
                Some(Arc::new(c))
            }
            Ok(None) => {
                tracing::warn!("[init] Cookie file {} has no SAPISID", cookie_path);
                None
            }
            Err(e) => {
                tracing::warn!("[init] Failed to load cookies from {}: {}", cookie_path, e);
                None
            }
        }
    } else {
        tracing::info!("[init] No cookie file at {}, running without Premium auth", cookie_path);
        None
    };

    // 5. Create reqwest client (shared across innertube + lounge)
    // NOTE: cookies are NOT loaded into the client — they're sent manually
    // only on WEB_REMIX /player calls to avoid poisoning IOS/other requests.
    let http_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .build()?;

    // 5b. Fetch live YouTube Music session (INNERTUBE_CONTEXT from page)
    // This is critical: without the full 22+ field context from ytcfg,
    // WEB_REMIX /player returns UNPLAYABLE. yt-dlp does the same thing.
    let yt_session = if let Some(ref cookies) = yt_cookies {
        match cookies::fetch_music_session(&http_client, cookies).await {
            Some(session) => {
                tracing::info!("[init] YouTube Music session extracted (logged_in={})", session.logged_in);
                Some(Arc::new(session))
            }
            None => {
                tracing::warn!("[init] Failed to extract YouTube Music session — WEB_REMIX may fail");
                None
            }
        }
    } else {
        None
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
        device_name: device_name.clone(),
        uuid: config.uuid.clone(),
        local_ip,
        port: dial_port,
        ctt: std::sync::RwLock::new(None),
        yt_cookies: yt_cookies.clone(),
        yt_session: yt_session.clone(),
        screen_id: config.screen_id.clone(),
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
    // Uses MPD's `idle player` command for instant notification when a song
    // ends, just like the Node.js mpd2 library's event system. No polling.
    let (idle_tx, mut idle_rx) = tokio::sync::mpsc::channel::<()>(8);
    let idle_mpd_host = mpd_host.clone();
    let idle_mpd_port = mpd_port;
    let idle_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            if *idle_shutdown.borrow() {
                break;
            }
            match mpd_idle.wait_player_change().await {
                Ok(()) => {
                    if idle_tx.send(()).await.is_err() {
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
    let innertube_client = http_client;
    let innertube_cookies = yt_cookies;
    let innertube_session = yt_session;
    let mut playlist = PlaylistState::default();
    let mut play_started = false;
    let mut has_remote = false;
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
                    &innertube_client,
                    &state_tx,
                    &mut playlist,
                    &mut play_started,
                    &dial_state,
                    innertube_cookies.as_ref().map(|c| c.as_ref()),
                    innertube_session.as_ref().map(|s| s.as_ref()),
                ).await {
                    tracing::error!("[player] Error handling command: {}", e);
                }
            }

            // MPD player state changed — instant song-end detection
            Some(()) = idle_rx.recv() => {
                // Drain any extra queued events (e.g. from clear+add+play sequence)
                while idle_rx.try_recv().is_ok() {}

                if play_started {
                    if let Ok(status) = mpd_client.status().await {
                        if status.state == "stop" {
                            play_started = false;

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

                            if playlist.current_index + 1 < playlist.video_ids.len() {
                                playlist.current_index += 1;
                                let video_id = playlist.video_ids[playlist.current_index].clone();
                                tracing::info!(
                                    "[player] Advancing to index {} ({})",
                                    playlist.current_index, video_id
                                );

                                // Track change — ensure NowPlaying is sent
                                playlist.last_state = messages::PlayerState::Idle;

                                match play_video(
                                    &mut mpd_client, &innertube_client, &state_tx,
                                    &mut playlist, &video_id, 0.0, innertube_cookies.as_ref().map(|c| c.as_ref()),
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
    http: &reqwest::Client,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    play_started: &mut bool,
    dial_state: &Arc<dial::DialState>,
    yt_cookies: Option<&cookies::YtCookies>,
    yt_session: Option<&cookies::YtMusicSession>,
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
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time, yt_cookies, yt_session).await?;
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
            *play_started = play_video(mpd, http, state_tx, playlist, &video_id, current_time, yt_cookies, yt_session).await?;
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
            let state = mpd_state_to_player_state(&status.state);
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
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0, yt_cookies, yt_session).await?;
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
                *play_started = play_video(mpd, http, state_tx, playlist, &video_id, 0.0, yt_cookies, yt_session).await?;
            } else {
                tracing::info!("[player] Next: end of playlist");
                *play_started = false;
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

fn current_video_id(playlist: &PlaylistState) -> String {
    playlist
        .video_ids
        .get(playlist.current_index)
        .cloned()
        .unwrap_or_default()
}

/// Resolve an audio stream via InnerTube and play it through MPD.
///
/// Returns `true` if playback started, `false` if no stream was available.
/// If InnerTube can't resolve the track, reports error state to YouTube
/// and returns `false`. Does NOT auto-skip — let YouTube handle navigation.
async fn play_video(
    mpd: &mut mpd::MpdClient,
    http: &reqwest::Client,
    state_tx: &tokio::sync::mpsc::Sender<messages::OutgoingMessage>,
    playlist: &mut PlaylistState,
    video_id: &str,
    start_position: f64,
    yt_cookies: Option<&cookies::YtCookies>,
    yt_session: Option<&cookies::YtMusicSession>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Tell YouTube we're loading — app shows spinner immediately
    send_state(mpd, state_tx, playlist, messages::PlayerState::Loading).await?;

    let stream = innertube::resolve_stream(
        http,
        video_id,
        playlist.ctt.as_deref(),
        if playlist.list_id.is_empty() {
            None
        } else {
            Some(playlist.list_id.as_str())
        },
        yt_cookies,
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

            mpd.clear().await?;
            mpd.add(&info.stream_url).await?;
            mpd.play().await?;

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
    let elapsed = status.elapsed.unwrap_or(0.0);
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
    // The receiver never stores or uses its own credentials: it only uses what
    // the cast client hands over (ctt) and the optional cookie file. Needles
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
    fn no_oauth_module_or_credentials_file() {
        for (path, src) in production_sources() {
            assert_ne!(path.file_name().unwrap(), "oauth.rs");
            for needle in [concat!("mod ", "oauth"), concat!("ytcast-", "oauth"), concat!("OAuth", "State")] {
                assert!(!src.contains(needle), "{} contains {needle}", path.display());
            }
        }
    }

    #[test]
    fn no_bearer_authorization() {
        for (path, src) in production_sources() {
            assert!(!src.contains(concat!("Bear", "er")), "{} mentions a bearer token", path.display());
            // The only Authorization header allowed is the cookie SAPISIDHASH.
            for (i, _) in src.match_indices(concat!("\"Authori", "zation\"")) {
                let value = &src[i..(i + 200).min(src.len())];
                assert!(
                    value.contains("full_sapisidhash("),
                    "{}: Authorization header that is not SAPISIDHASH: {value}",
                    path.display()
                );
            }
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
