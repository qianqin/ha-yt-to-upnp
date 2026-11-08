//! Silence pre-roll: at a DIAL launch MPD starts playing endless silence, so
//! the speaker adapter wakes the speaker and connects it to MPD's HTTP stream
//! while the phone is still loading the song. MPD's httpd output serves
//! nothing while MPD is stopped with an empty queue, so without the pre-roll
//! the speaker would only start waking once the song plays.
//!
//! The silence is no song: it is never reported to the phone, and its end is
//! no song end.

use std::time::Duration;

use tokio::time::Instant;

use crate::mpd::{self, MpdClient};

/// Without a song by then the phone gave up; MPD is stopped so the speaker
/// idles and its own auto power-off can take over.
pub const PREROLL_TIMEOUT: Duration = Duration::from_secs(60);

/// After a pre-roll was stopped without a song (timeout, or the speaker
/// released it), launches start no silence for this long, so repeated
/// launches cannot keep the speaker awake or away from its other source. A
/// cast's song still plays; it only starts without the early wake.
pub const PREROLL_COOLDOWN: Duration = Duration::from_secs(45);

#[derive(Debug, Default)]
pub struct PreRoll {
    active: Option<Active>,
    cooldown_until: Option<Instant>,
}

#[derive(Debug)]
struct Active {
    since: Instant,
    /// MPD song id of the silence. Ids, unlike queue positions, stay valid
    /// when the song is added next to it.
    silence_id: u32,
}

impl PreRoll {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Only an idle player gets the pre-roll: a song that is playing or
    /// paused (e.g. a second phone joins) is never interrupted, a running
    /// pre-roll is neither stacked nor restarted, and none starts during the
    /// cooldown.
    pub fn should_start(&self, play_started: bool, mpd_state: &str, now: Instant) -> bool {
        let cooled_down = self.cooldown_until.is_none_or(|until| now >= until);
        self.active.is_none() && cooled_down && !play_started && mpd_state == "stop"
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.active.as_ref().map(|a| a.since + PREROLL_TIMEOUT)
    }

    /// Replace MPD's queue with the silence and play it.
    pub async fn start(&mut self, mpd: &mut MpdClient, silence_url: &str) -> mpd::Result<()> {
        mpd.clear().await?;
        let silence_id = mpd.addid(silence_url).await?;
        mpd.play().await?;
        self.active = Some(Active {
            since: Instant::now(),
            silence_id,
        });
        tracing::info!("[preroll] pre-roll started");
        Ok(())
    }

    /// Play a song. A running pre-roll hands over without stopping MPD: the
    /// song is added behind the silence, played, and the silence removed, so
    /// MPD goes from play straight to play (`clear` would show the speaker
    /// adapter a stopped player in between). Returns how long the silence
    /// played, or `None` if there was no pre-roll.
    pub async fn play_song(
        &mut self,
        mpd: &mut MpdClient,
        song_url: &str,
    ) -> mpd::Result<Option<Duration>> {
        // Ended first, also if the hand-over fails: its timeout must never
        // stop a song.
        let Some(active) = self.active.take() else {
            mpd.clear().await?;
            mpd.add(song_url).await?;
            mpd.play().await?;
            return Ok(None);
        };
        if let Err(e) = hand_over(mpd, song_url).await {
            // Without this the silence would play on, with no timeout left.
            if let Err(stop_err) = mpd.stop().await {
                tracing::warn!("[preroll] failed hand-over: stopping MPD failed: {stop_err}");
            }
            if let Err(clear_err) = mpd.clear().await {
                tracing::warn!("[preroll] failed hand-over: clearing MPD failed: {clear_err}");
            }
            return Err(e);
        }
        if let Err(e) = mpd.deleteid(active.silence_id).await {
            // The song is playing; a leftover silence only sits in the queue.
            tracing::warn!("[preroll] could not remove the silence: {e}");
        }
        let waited = active.since.elapsed();
        tracing::info!(
            "[preroll] pre-roll replaced by song after {:.1} s",
            waited.as_secs_f64()
        );
        Ok(Some(waited))
    }

    /// Stop and clear MPD if the pre-roll is running, and start the
    /// cooldown. Returns how long the silence played, or `None` (MPD
    /// untouched) if there was no pre-roll.
    pub async fn stop(&mut self, mpd: &mut MpdClient) -> mpd::Result<Option<Duration>> {
        let Some(active) = self.active.take() else {
            return Ok(None);
        };
        self.cooldown_until = Some(Instant::now() + PREROLL_COOLDOWN);
        mpd.stop().await?;
        mpd.clear().await?;
        Ok(Some(active.since.elapsed()))
    }

    /// A running pre-roll for tests that don't drive MPD.
    #[cfg(test)]
    pub fn running() -> Self {
        Self {
            active: Some(Active {
                since: Instant::now(),
                silence_id: 1,
            }),
            cooldown_until: None,
        }
    }

    /// End the pre-roll when the caller clears MPD itself.
    pub fn forget(&mut self) -> Option<Duration> {
        self.active.take().map(|a| a.since.elapsed())
    }
}

/// Add the song behind the silence and play it.
async fn hand_over(mpd: &mut MpdClient, song_url: &str) -> mpd::Result<()> {
    let song_id = mpd.addid(song_url).await?;
    mpd.playid(song_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_only_on_an_idle_player() {
        let idle = PreRoll::default();
        assert!(idle.should_start(false, "stop", Instant::now()));
        // A song playing or paused (e.g. a second phone joins): don't interrupt.
        assert!(!idle.should_start(true, "play", Instant::now()));
        assert!(!idle.should_start(true, "pause", Instant::now()));
        assert!(!idle.should_start(true, "stop", Instant::now()));
        assert!(!idle.should_start(false, "play", Instant::now()));
        assert!(!idle.should_start(false, "pause", Instant::now()));
    }

    #[test]
    fn timeout_and_cooldown_are_pinned() {
        assert_eq!(PREROLL_TIMEOUT, Duration::from_secs(60));
        assert_eq!(PREROLL_COOLDOWN, Duration::from_secs(45));
    }

    #[test]
    fn an_idle_pre_roll_has_no_deadline() {
        let idle = PreRoll::default();
        assert!(!idle.is_active());
        assert_eq!(idle.deadline(), None);
    }

    #[cfg(not(feature = "mock-mpd"))]
    mod with_mpd {
        use super::super::*;
        use crate::mpd::fake::fake_mpd;

        const SILENCE: &str = "http://localhost:8008/silence";
        const SONG: &str = "http://localhost:8008/stream/aaaaaaaaaaa";

        async fn client(
            script: Vec<(&'static str, &'static str)>,
        ) -> (MpdClient, tokio::task::JoinHandle<Vec<String>>) {
            let (port, server) = fake_mpd(script).await;
            (MpdClient::connect("127.0.0.1", port).await.unwrap(), server)
        }

        /// A pre-roll whose silence has song id 5.
        async fn started() -> PreRoll {
            let (mut mpd, _server) = client(vec![
                ("clear", "OK\n"),
                ("addid", "Id: 5\nOK\n"),
                ("play", "OK\n"),
            ])
            .await;
            let mut preroll = PreRoll::default();
            preroll.start(&mut mpd, SILENCE).await.unwrap();
            preroll
        }

        #[tokio::test]
        async fn start_plays_silence_alone_with_a_deadline() {
            let (mut mpd, server) = client(vec![
                ("clear", "OK\n"),
                ("addid", "Id: 5\nOK\n"),
                ("play", "OK\n"),
            ])
            .await;
            let mut preroll = PreRoll::default();
            let before = Instant::now();
            preroll.start(&mut mpd, SILENCE).await.unwrap();

            assert!(preroll.is_active());
            let deadline = preroll.deadline().unwrap();
            assert!(
                deadline >= before + PREROLL_TIMEOUT
                    && deadline <= Instant::now() + PREROLL_TIMEOUT
            );
            // Busy now: a further launch must not stack another silence.
            assert!(!preroll.should_start(false, "stop", Instant::now()));
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec!["clear", "addid \"http://localhost:8008/silence\"", "play"]
            );
        }

        #[tokio::test]
        async fn failed_start_leaves_no_pre_roll() {
            let (mut mpd, _server) = client(vec![
                ("clear", "OK\n"),
                ("addid", "ACK [50@0] {addid} No such directory\n"),
            ])
            .await;
            let mut preroll = PreRoll::default();
            assert!(preroll.start(&mut mpd, SILENCE).await.is_err());
            assert!(!preroll.is_active());
        }

        #[tokio::test]
        async fn song_replaces_the_silence_without_stopping_mpd() {
            let mut preroll = started().await;
            let (mut mpd, server) = client(vec![
                ("addid", "Id: 6\nOK\n"),
                ("playid", "OK\n"),
                ("deleteid", "OK\n"),
            ])
            .await;

            assert!(preroll.play_song(&mut mpd, SONG).await.unwrap().is_some());
            assert!(!preroll.is_active());
            drop(mpd);
            // No clear and no stop: MPD goes from playing silence straight to
            // playing the song, so the adapter never sees a stopped player.
            assert_eq!(
                server.await.unwrap(),
                vec![
                    "addid \"http://localhost:8008/stream/aaaaaaaaaaa\"",
                    "playid 6",
                    "deleteid 5"
                ]
            );
        }

        #[tokio::test]
        async fn song_plays_even_if_the_silence_is_already_gone() {
            let mut preroll = started().await;
            let (mut mpd, _server) = client(vec![
                ("addid", "Id: 6\nOK\n"),
                ("playid", "OK\n"),
                ("deleteid", "ACK [50@0] {deleteid} No such song\n"),
            ])
            .await;
            assert!(preroll.play_song(&mut mpd, SONG).await.unwrap().is_some());
            assert!(!preroll.is_active());
        }

        #[tokio::test]
        async fn failed_hand_over_ends_the_pre_roll_and_its_silence() {
            // Ended, or its timeout could later stop a song that did start;
            // MPD stopped, or the silence would play on with no timeout.
            let mut preroll = started().await;
            let (mut mpd, server) = client(vec![
                ("addid", "ACK [50@0] {addid} No such directory\n"),
                ("stop", "OK\n"),
                ("clear", "OK\n"),
            ])
            .await;
            let err = preroll.play_song(&mut mpd, SONG).await.unwrap_err();
            assert!(err.to_string().contains("No such directory"), "{err}");
            assert!(!preroll.is_active());
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec!["addid \"http://localhost:8008/stream/aaaaaaaaaaa\"", "stop", "clear"]
            );
        }

        #[tokio::test]
        async fn failed_play_of_the_song_ends_the_silence_too() {
            let mut preroll = started().await;
            let (mut mpd, server) = client(vec![
                ("addid", "Id: 6\nOK\n"),
                ("playid", "ACK [50@0] {playid} boom\n"),
                ("stop", "ACK [50@0] {stop} also broken\n"),
                ("clear", "OK\n"),
            ])
            .await;
            // The hand-over's own error is returned, not the clean-up's.
            let err = preroll.play_song(&mut mpd, SONG).await.unwrap_err();
            assert!(err.to_string().contains("{playid} boom"), "{err}");
            assert!(!preroll.is_active());
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec!["addid \"http://localhost:8008/stream/aaaaaaaaaaa\"", "playid 6", "stop", "clear"]
            );
        }

        #[tokio::test]
        async fn a_stopped_pre_roll_does_not_restart_during_the_cooldown() {
            // A peer repeating launches must not keep the speaker awake or
            // away from its other source.
            let mut preroll = started().await;
            let (mut mpd, _server) = client(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
            let stopped = Instant::now();
            preroll.stop(&mut mpd).await.unwrap();
            assert!(!preroll.should_start(false, "stop", Instant::now()));
            assert!(!preroll.should_start(false, "stop", stopped + PREROLL_COOLDOWN - Duration::from_secs(1)));
            assert!(preroll.should_start(false, "stop", Instant::now() + PREROLL_COOLDOWN));
        }

        #[tokio::test]
        async fn a_failed_stop_still_starts_the_cooldown() {
            let mut preroll = started().await;
            let (mut mpd, _server) = client(vec![("stop", "ACK [50@0] {stop} boom\n")]).await;
            assert!(preroll.stop(&mut mpd).await.is_err());
            assert!(!preroll.should_start(false, "stop", Instant::now()));
        }

        #[tokio::test]
        async fn a_pre_roll_ended_by_a_song_or_the_phone_has_no_cooldown() {
            let mut preroll = started().await;
            preroll.forget();
            assert!(preroll.should_start(false, "stop", Instant::now()));

            let mut preroll = started().await;
            let (mut mpd, _server) = client(vec![
                ("addid", "Id: 6\nOK\n"),
                ("playid", "OK\n"),
                ("deleteid", "OK\n"),
            ])
            .await;
            preroll.play_song(&mut mpd, SONG).await.unwrap();
            assert!(preroll.should_start(false, "stop", Instant::now()));
        }

        #[tokio::test]
        async fn song_without_pre_roll_replaces_the_queue_as_before() {
            let (mut mpd, server) =
                client(vec![("clear", "OK\n"), ("add", "OK\n"), ("play", "OK\n")]).await;
            let mut preroll = PreRoll::default();
            assert_eq!(preroll.play_song(&mut mpd, SONG).await.unwrap(), None);
            drop(mpd);
            assert_eq!(
                server.await.unwrap(),
                vec![
                    "clear",
                    "add \"http://localhost:8008/stream/aaaaaaaaaaa\"",
                    "play"
                ]
            );
        }

        #[tokio::test]
        async fn stop_ends_the_silence() {
            let mut preroll = started().await;
            let (mut mpd, server) = client(vec![("stop", "OK\n"), ("clear", "OK\n")]).await;
            assert!(preroll.stop(&mut mpd).await.unwrap().is_some());
            assert!(!preroll.is_active());
            assert_eq!(preroll.deadline(), None);
            drop(mpd);
            assert_eq!(server.await.unwrap(), vec!["stop", "clear"]);
        }

        #[tokio::test]
        async fn stop_without_pre_roll_leaves_mpd_alone() {
            let (mut mpd, server) = client(vec![]).await;
            assert_eq!(PreRoll::default().stop(&mut mpd).await.unwrap(), None);
            drop(mpd);
            assert!(server.await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn failed_stop_still_ends_the_pre_roll() {
            // Otherwise the expired deadline would fire again at once.
            let mut preroll = started().await;
            let (mut mpd, _server) = client(vec![("stop", "ACK [50@0] {stop} boom\n")]).await;
            assert!(preroll.stop(&mut mpd).await.is_err());
            assert!(!preroll.is_active());
        }

        #[tokio::test]
        async fn forget_ends_the_pre_roll_without_mpd() {
            let mut preroll = started().await;
            assert!(preroll.forget().is_some());
            assert!(!preroll.is_active());
            assert_eq!(preroll.forget(), None);
        }
    }
}
