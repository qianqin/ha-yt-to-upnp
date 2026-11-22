//! The receiver's identity on the LAN and towards YouTube: its UPnP/DIAL
//! uuid and one Lounge screen id per theme. YouTube can come to treat an
//! identity badly (every /next answered with autoplay only); a new one heals
//! that. Ids are never logged.

use crate::config::Config;
use crate::watch_next::NextSource;

/// What the DIAL server, SSDP and the Lounge sessions present.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub uuid: String,
    pub screen_id: String,
    pub screen_id_m: String,
    /// The "m" Lounge session's device id: one of its own per identity,
    /// not stored.
    lounge_uuid_m: String,
}

/// A Lounge session's part of the identity.
#[derive(Clone, Debug, PartialEq)]
pub struct LoungeIdentity {
    pub uuid: String,
    pub screen_id: String,
}

impl Identity {
    pub fn from_config(config: &Config) -> Self {
        Identity {
            uuid: config.uuid.clone(),
            screen_id: config.screen_id.clone(),
            screen_id_m: config.screen_id_m.clone(),
            lounge_uuid_m: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// The identity of the Lounge session for `theme` ("cl" or "m").
    pub fn lounge(&self, theme: &str) -> LoungeIdentity {
        if theme == "m" {
            LoungeIdentity { uuid: self.lounge_uuid_m.clone(), screen_id: self.screen_id_m.clone() }
        } else {
            LoungeIdentity { uuid: self.uuid.clone(), screen_id: self.screen_id.clone() }
        }
    }
}

/// Casts in a row answered with autoplay only after which the identity is
/// renewed.
pub const AUTOPLAY_ONLY_CASTS: u32 = 3;

/// Whether YouTube still treats this identity well. A healthy one gets the
/// queue's next song from /next (nextVideoRenderer); a bad one only an
/// autoplay pick without a queue index (mdxAutoplayVideoRenderer), so songs
/// 2+ play without a ctt.
#[derive(Debug, Default)]
pub struct Health {
    autoplay_only_in_row: u32,
    /// A cast of a queue started and its first /next answer is still to come.
    awaiting_first_answer: bool,
    renewal_due: bool,
    /// A renewal was taken and its new identity not yet adopted.
    renewing: bool,
}

impl Health {
    /// A setPlaylist started a cast of `queue_len` songs (`has_list`: it
    /// names a list; an ad-hoc queue may get autoplay-only answers anyway).
    pub fn cast_started(&mut self, queue_len: usize, has_list: bool) {
        self.awaiting_first_answer = queue_len >= 2 && has_list;
    }

    /// A /next answer for the playing song (`queue_has_next`: the queue has
    /// a song after it). A cast's first answer with autoplay only counts; any
    /// answer naming the queue's next resets the count.
    pub fn next_answered(&mut self, source: NextSource, index: Option<u64>, queue_has_next: bool) {
        let first = std::mem::take(&mut self.awaiting_first_answer);
        match source {
            NextSource::NextVideoRenderer => {
                self.autoplay_only_in_row = 0;
                self.renewal_due = false;
                self.renewing = false;
            }
            NextSource::MdxAutoplay if first && index.is_none() && queue_has_next => {
                self.autoplay_only_in_row += 1;
                if self.autoplay_only_in_row >= AUTOPLAY_ONLY_CASTS {
                    self.renewal_due = true;
                }
            }
            NextSource::MdxAutoplay => {}
        }
    }

    /// The due renewal, taken only when `idle` (nothing plays, no queue):
    /// the casts that made it due, and the count starts over. At most one a
    /// day (`last_auto`: the last automatic renewal, `now`: unix seconds);
    /// one due sooner is dropped.
    pub fn take_renewal(&mut self, idle: bool, last_auto: Option<u64>, now: u64) -> Option<u32> {
        if !(self.renewal_due && idle) {
            return None;
        }
        self.renewal_due = false;
        let casts = std::mem::take(&mut self.autoplay_only_in_row);
        if last_auto.is_some_and(|last| now.saturating_sub(last) < AUTO_RENEWAL_INTERVAL_S) {
            tracing::warn!(
                "[identity] YouTube answers /next with autoplay only for {casts} casts in a row; the identity was renewed less than 24 h ago, so it stays"
            );
            return None;
        }
        self.renewing = true;
        Some(casts)
    }

    /// Whether a taken renewal is still wanted: an answer naming the queue's
    /// next song since makes it moot.
    pub fn renewal_wanted(&self) -> bool {
        self.renewing
    }

    /// The prepared new identity (`prepared`), to adopt now if `idle` and
    /// still wanted; one no longer wanted is dropped.
    pub fn take_prepared<T>(&mut self, prepared: &mut Option<T>, idle: bool) -> Option<T> {
        if !self.renewing {
            if prepared.take().is_some() {
                tracing::info!("[identity] YouTube names the queue's next song again; the new identity is not used");
            }
            return None;
        }
        if !idle || prepared.is_none() {
            return None;
        }
        self.renewing = false;
        prepared.take()
    }
}

/// The least time between two automatic renewals.
const AUTO_RENEWAL_INTERVAL_S: u64 = 24 * 60 * 60;

/// `config` with a new identity: a new uuid and new screen ids (from
/// `generate_screen_id`, YouTube's pairing API). The option's generation stays.
pub async fn renewed<G, Fut>(mut config: Config, generate_screen_id: G) -> Result<Config, Box<dyn std::error::Error + Send + Sync>>
where
    G: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<String, Box<dyn std::error::Error + Send + Sync>>>,
{
    config.screen_id = generate_screen_id().await?;
    config.screen_id_m = generate_screen_id().await?;
    config.uuid = uuid::Uuid::new_v4().to_string();
    Ok(config)
}

/// The generation the `identity_generation` option asks for (its env
/// value). Unset, cleared or no number: none, the identity stays.
pub fn wanted_generation(value: Option<&str>) -> Option<i64> {
    value?.trim().parse().ok()
}

/// At start: a generation other than the stored one asks for a new identity
/// (a new uuid; the screen ids are left empty for the start to make). True
/// if it did; the new generation is stored with it.
pub fn apply_identity_generation(config: &mut Config, wanted: i64) -> bool {
    if config.identity_generation == wanted {
        return false;
    }
    config.uuid = uuid::Uuid::new_v4().to_string();
    config.screen_id.clear();
    config.screen_id_m.clear();
    config.identity_generation = wanted;
    true
}

/// Store `renewed` (`save`) and present it: the DIAL server and SSDP show
/// it, both Lounge sessions register its screens. Logged once, without ids.
pub fn publish(
    mut renewed: Config,
    autoplay_only_casts: u32,
    now: u64,
    save: impl FnOnce(&Config) -> crate::config::Result<()>,
    identity: &tokio::sync::watch::Sender<Identity>,
) -> Config {
    renewed.last_auto_renewal = Some(now);
    tracing::warn!(
        "[identity] YouTube answers /next with autoplay only for {autoplay_only_casts} casts in a row; new receiver identity (the phone must pick the speaker again)"
    );
    if let Err(e) = save(&renewed) {
        tracing::error!("[identity] the new identity could not be stored ({e}); it is used until the next start");
    }
    identity.send_replace(Identity::from_config(&renewed));
    renewed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch_next::NextSource;

    fn config() -> Config {
        Config {
            uuid: "old-uuid".into(),
            screen_id: "old-screen".into(),
            screen_id_m: "old-screen-m".into(),
            identity_generation: 4,
            last_auto_renewal: None,
        }
    }

    /// Unix seconds "now" in the tests.
    const NOW: u64 = 2_000_000_000;
    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn each_lounge_theme_gets_its_own_screen_and_device_id() {
        let identity = Identity::from_config(&config());
        let cl = identity.lounge("cl");
        let m = identity.lounge("m");
        assert_eq!((cl.uuid.as_str(), cl.screen_id.as_str()), ("old-uuid", "old-screen"));
        assert_eq!(m.screen_id, "old-screen-m");
        assert_ne!(m.uuid, "old-uuid", "the m session has a device id of its own");
        assert_eq!(identity.lounge("m").uuid, m.uuid, "for this identity");
    }

    /// One cast of a two-song queue whose first /next answer is `source`.
    fn cast(health: &mut Health, source: NextSource, index: Option<u64>) {
        health.cast_started(2, true);
        health.next_answered(source, index, true);
    }

    #[test]
    fn three_casts_in_a_row_answered_with_autoplay_only_make_a_renewal_due() {
        let mut health = Health::default();
        cast(&mut health, NextSource::MdxAutoplay, None);
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(true, None, NOW), None);
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(false, None, NOW), None, "not while something plays");
        assert_eq!(health.take_renewal(true, None, NOW), Some(3));
        assert_eq!(health.take_renewal(true, None, NOW), None, "taken once");
        // The count starts over.
        cast(&mut health, NextSource::MdxAutoplay, None);
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(true, None, NOW), None);
    }

    #[test]
    fn any_answer_naming_the_queues_next_resets_the_count() {
        let mut health = Health::default();
        cast(&mut health, NextSource::MdxAutoplay, None);
        cast(&mut health, NextSource::MdxAutoplay, None);
        health.next_answered(NextSource::NextVideoRenderer, Some(2), true);
        cast(&mut health, NextSource::MdxAutoplay, None);
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(true, None, NOW), None);
    }

    #[test]
    fn only_a_casts_first_answer_for_a_queue_with_a_next_song_counts() {
        let mut health = Health::default();
        // Later answers of the same cast.
        health.cast_started(5, true);
        health.next_answered(NextSource::MdxAutoplay, None, true);
        health.next_answered(NextSource::MdxAutoplay, None, true);
        health.next_answered(NextSource::MdxAutoplay, None, true);
        // A single song (no queue), the queue's last song, an autoplay with an index.
        health.cast_started(1, true);
        health.next_answered(NextSource::MdxAutoplay, None, false);
        // A single song whose queue grew later (UpdatePlaylist) was no queue cast.
        health.cast_started(1, true);
        health.next_answered(NextSource::MdxAutoplay, None, true);
        health.cast_started(3, true);
        health.next_answered(NextSource::MdxAutoplay, None, false);
        health.cast_started(3, true);
        health.next_answered(NextSource::MdxAutoplay, Some(1), true);
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(true, None, NOW), None, "two counted casts");
        cast(&mut health, NextSource::MdxAutoplay, None);
        assert_eq!(health.take_renewal(true, None, NOW), Some(3));
    }

    #[test]
    fn a_cast_without_a_list_does_not_count() {
        let mut health = Health::default();
        for _ in 0..3 {
            health.cast_started(4, false);
            health.next_answered(NextSource::MdxAutoplay, None, true);
        }
        assert_eq!(health.take_renewal(true, None, NOW), None, "an ad-hoc queue may get autoplay only");
    }

    #[test]
    fn an_automatic_renewal_comes_at_most_once_a_day() {
        let mut health = Health::default();
        let due = |health: &mut Health| {
            for _ in 0..AUTOPLAY_ONLY_CASTS {
                cast(health, NextSource::MdxAutoplay, None);
            }
        };
        due(&mut health);
        assert_eq!(health.take_renewal(true, Some(NOW - DAY + 60), NOW), None, "renewed 23:59 ago");
        assert!(!health.renewal_wanted());
        due(&mut health);
        assert_eq!(health.take_renewal(true, Some(NOW - DAY), NOW), Some(3), "a day ago");
        assert!(health.renewal_wanted());
    }

    #[test]
    fn a_prepared_identity_is_dropped_once_youtube_answers_well_again() {
        let renewal = || (config(), 3);
        let mut health = Health::default();
        for _ in 0..3 {
            cast(&mut health, NextSource::MdxAutoplay, None);
        }
        assert_eq!(health.take_renewal(true, None, NOW), Some(3));
        let mut prepared = Some(renewal());
        assert!(health.take_prepared(&mut prepared, false).is_none(), "waits for idle");
        assert!(prepared.is_some());
        assert!(health.take_prepared(&mut prepared, true).is_some());
        assert!(!health.renewal_wanted(), "adopted");

        for _ in 0..3 {
            cast(&mut health, NextSource::MdxAutoplay, None);
        }
        health.take_renewal(true, None, NOW).unwrap();
        // The queue's next is named again before the new identity is used.
        health.next_answered(NextSource::NextVideoRenderer, Some(1), true);
        let mut prepared = Some(renewal());
        assert!(health.take_prepared(&mut prepared, true).is_none());
        assert!(prepared.is_none(), "dropped");
    }

    #[tokio::test]
    async fn a_renewed_identity_has_a_new_uuid_and_new_screen_ids() {
        let screens = std::sync::Mutex::new(vec!["new-screen-m", "new-screen"]);
        let renewed = renewed(config(), || {
            let screen = screens.lock().unwrap().pop().unwrap().to_owned();
            async move { Ok(screen) }
        })
        .await
        .unwrap();
        assert_ne!(renewed.uuid, "old-uuid");
        assert!(uuid::Uuid::parse_str(&renewed.uuid).is_ok());
        assert_eq!((renewed.screen_id.as_str(), renewed.screen_id_m.as_str()), ("new-screen", "new-screen-m"));
        assert_eq!(renewed.identity_generation, 4, "the option's generation stays");
    }

    #[tokio::test]
    async fn a_renewal_that_gets_no_screen_id_fails() {
        let result = renewed(config(), || async { Err::<String, _>("generate_screen_id returned 500".into()) }).await;
        assert!(result.is_err());
    }

    #[test]
    fn a_published_identity_is_stored_and_presented_everywhere() {
        let (tx, mut rx) = tokio::sync::watch::channel(Identity::from_config(&config()));
        rx.borrow_and_update();
        let new = Config { uuid: "new-uuid".into(), screen_id: "new-screen".into(), ..config() };
        let saved = std::cell::RefCell::new(None);
        let logs = crate::storage::test_util::LogCapture::start();

        let published = publish(new, 3, NOW, |c| {
            *saved.borrow_mut() = Some(c.uuid.clone());
            Ok(())
        }, &tx);

        assert_eq!(published.uuid, "new-uuid");
        assert_eq!(published.last_auto_renewal, Some(NOW), "stored with the identity");
        assert_eq!(saved.into_inner().as_deref(), Some("new-uuid"));
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow().uuid, "new-uuid");
        assert_eq!(rx.borrow().lounge("cl").screen_id, "new-screen");
        let text = logs.text();
        assert!(text.contains(
            "[identity] YouTube answers /next with autoplay only for 3 casts in a row; new receiver identity (the phone must pick the speaker again)"
        ), "{text}");
        assert!(text.contains("WARN"), "{text}");
        for id in ["new-uuid", "new-screen", "old-uuid"] {
            assert!(!text.contains(id), "{id}: {text}");
        }
    }

    #[test]
    fn an_identity_that_cannot_be_stored_is_still_presented() {
        let (tx, rx) = tokio::sync::watch::channel(Identity::from_config(&config()));
        let new = Config { uuid: "new-uuid".into(), ..config() };
        publish(new, 3, NOW, |_| Err(crate::config::ConfigError::NoHomeDir), &tx);
        assert_eq!(rx.borrow().uuid, "new-uuid");
    }

    #[test]
    fn a_changed_identity_generation_makes_a_new_identity_once() {
        let mut config = config();
        assert!(!apply_identity_generation(&mut config, 4), "unchanged");
        assert_eq!(config.uuid, "old-uuid");

        assert!(apply_identity_generation(&mut config, 5));
        assert_ne!(config.uuid, "old-uuid");
        assert!(config.screen_id.is_empty() && config.screen_id_m.is_empty(), "made anew at start");
        assert_eq!(config.identity_generation, 5);
        let uuid = config.uuid.clone();
        assert!(!apply_identity_generation(&mut config, 5), "bumping resets once");
        assert_eq!(config.uuid, uuid);
    }

    #[test]
    fn only_a_set_generation_other_than_the_stored_one_renews() {
        let mut config = Config { identity_generation: 0, ..config() };
        let mut renews = |option: Option<&str>| {
            wanted_generation(option).is_some_and(|wanted| apply_identity_generation(&mut config, wanted))
        };
        assert!(renews(Some("1")), "set 1");
        assert!(!renews(None), "cleared");
        assert!(!renews(Some("1")), "set 1 again");
        assert!(renews(Some("2")), "set 2");
    }

    #[test]
    fn the_wanted_generation_comes_from_the_option() {
        assert_eq!(wanted_generation(None), None, "unset or cleared: no change");
        assert_eq!(wanted_generation(Some("")), None);
        assert_eq!(wanted_generation(Some("3")), Some(3));
        assert_eq!(wanted_generation(Some(" 3\n")), Some(3), "as the run script may pass it");
        assert_eq!(wanted_generation(Some("-1")), Some(-1));
        assert_eq!(wanted_generation(Some("x")), None, "not a number: no change");
    }
}
