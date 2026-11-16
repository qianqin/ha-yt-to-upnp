//! Per-video authorization of the running cast, in memory only.
//!
//! A ctt is valid for one video. The phone sends one with setPlaylist /
//! setVideo for the video it starts; /next (see `watch_next`) gives the next
//! video's own. Each video is played and echoed in nowPlaying with its own
//! ctt and params, never with another video's.

/// One video's ctt and params.
#[derive(Clone, Default, PartialEq)]
pub struct VideoAuth {
    pub ctt: Option<String>,
    pub params: Option<String>,
}

/// Tokens are reported by length only.
impl std::fmt::Debug for VideoAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoAuth")
            .field("ctt_len", &self.ctt.as_deref().map(str::len))
            .field("params_len", &self.params.as_deref().map(str::len))
            .finish()
    }
}

/// The current, next and a few earlier videos (for "previous").
const MAX_VIDEOS: usize = 4;

/// The cast's known per-video tokens, keyed by video id.
#[derive(Default)]
pub struct CastTokens {
    /// Oldest first.
    videos: Vec<(String, VideoAuth)>,
    /// Bumped by `clear` and `next_song`, so a /next answer for an earlier
    /// cast or song is dropped.
    generation: u64,
    /// The (generation, current video, expected next video) /next was asked
    /// about: at most one request per song.
    prefetched: Option<(u64, String, String)>,
    /// The video /next named after the current song, when it is a later
    /// queue entry than the queue's next (YouTube skipped entries itself).
    /// By id: the queue may be edited (UpdatePlaylist) before the song ends.
    next_choice: Option<String>,
}

impl CastTokens {
    /// Forget everything: the cast changed or ended.
    pub fn clear(&mut self) {
        self.videos.clear();
        self.prefetched = None;
        self.next_choice = None;
        self.generation += 1;
    }

    /// The queue moved to another song: a /next answer still on its way,
    /// and YouTube's choice after the earlier song, no longer apply. The
    /// songs' own tokens stay.
    pub fn next_song(&mut self) {
        self.next_choice = None;
        self.generation += 1;
    }

    /// /next names `video_id` to follow the current song.
    pub fn choose_next(&mut self, video_id: &str) {
        self.next_choice = Some(video_id.to_owned());
    }

    pub fn forget_next_choice(&mut self) {
        self.next_choice = None;
    }

    pub fn next_choice(&self) -> Option<&str> {
        self.next_choice.as_deref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Store `video_id`'s own tokens, replacing older ones for it; when full,
    /// the oldest video goes. Nothing is stored for a video without either.
    pub fn remember(&mut self, video_id: &str, auth: VideoAuth) {
        self.videos.retain(|(id, _)| id != video_id);
        if auth == VideoAuth::default() {
            return;
        }
        if self.videos.len() >= MAX_VIDEOS {
            self.videos.remove(0);
        }
        self.videos.push((video_id.to_owned(), auth));
    }

    /// `video_id`'s own tokens; none if they are unknown.
    pub fn auth_for(&self, video_id: &str) -> VideoAuth {
        self.videos
            .iter()
            .find(|(id, _)| id == video_id)
            .map(|(_, auth)| auth.clone())
            .unwrap_or_default()
    }

    /// Note that /next is being asked which video follows `current`
    /// (expected: `next`). False if it already was, in this cast.
    pub fn start_prefetch(&mut self, current: &str, next: &str) -> bool {
        let key = (self.generation, current.to_owned(), next.to_owned());
        if self.prefetched.as_ref() == Some(&key) {
            return false;
        }
        self.prefetched = Some(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(ctt: &str) -> VideoAuth {
        VideoAuth { ctt: Some(ctt.into()), params: Some(format!("{ctt}-params")) }
    }

    #[test]
    fn each_video_gets_only_its_own_tokens() {
        let mut tokens = CastTokens::default();
        tokens.remember("aaaaaaaaaaa", auth("ctt-a"));
        tokens.remember("bbbbbbbbbbb", auth("ctt-b"));
        assert_eq!(tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"));
        assert_eq!(tokens.auth_for("bbbbbbbbbbb"), auth("ctt-b"));
        assert_eq!(tokens.auth_for("ccccccccccc"), VideoAuth::default());
    }

    #[test]
    fn newer_tokens_replace_older_ones_and_old_videos_are_dropped() {
        let mut tokens = CastTokens::default();
        tokens.remember("aaaaaaaaaaa", auth("ctt-a"));
        tokens.remember("aaaaaaaaaaa", auth("ctt-a2"));
        assert_eq!(tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a2"));
        for (i, id) in ["v1", "v2", "v3", "v4"].iter().enumerate() {
            tokens.remember(id, auth(&format!("ctt-{i}")));
        }
        assert_eq!(tokens.auth_for("aaaaaaaaaaa"), VideoAuth::default());
        // Four are kept: current, next and two for "previous".
        for (i, id) in ["v1", "v2", "v3", "v4"].iter().enumerate() {
            assert_eq!(tokens.auth_for(id), auth(&format!("ctt-{i}")), "{id}");
        }
        tokens.remember("v4", VideoAuth::default());
        assert_eq!(tokens.auth_for("v4"), VideoAuth::default());
    }

    #[test]
    fn clearing_forgets_all_and_starts_a_new_generation() {
        let mut tokens = CastTokens::default();
        tokens.remember("aaaaaaaaaaa", auth("ctt-a"));
        assert!(tokens.start_prefetch("aaaaaaaaaaa", "bbbbbbbbbbb"));
        let before = tokens.generation();
        tokens.clear();
        assert_eq!(tokens.auth_for("aaaaaaaaaaa"), VideoAuth::default());
        assert_ne!(tokens.generation(), before);
        assert!(tokens.start_prefetch("aaaaaaaaaaa", "bbbbbbbbbbb"), "a new cast asks again");
    }

    #[test]
    fn next_is_asked_once_per_song() {
        let mut tokens = CastTokens::default();
        assert!(tokens.start_prefetch("aaaaaaaaaaa", "bbbbbbbbbbb"));
        assert!(!tokens.start_prefetch("aaaaaaaaaaa", "bbbbbbbbbbb"));
        assert!(tokens.start_prefetch("bbbbbbbbbbb", "ccccccccccc"));
        // A changed queue (another next video) is asked about again.
        assert!(tokens.start_prefetch("bbbbbbbbbbb", "ddddddddddd"));
    }

    #[test]
    fn youtubes_next_choice_holds_for_the_current_song_only() {
        let mut tokens = CastTokens::default();
        assert_eq!(tokens.next_choice(), None);
        tokens.choose_next("ccccccccccc");
        assert_eq!(tokens.next_choice(), Some("ccccccccccc"));
        let before = tokens.generation();
        tokens.next_song();
        assert_eq!(tokens.next_choice(), None, "another song plays");
        assert_ne!(tokens.generation(), before, "an answer for the earlier song is stale");
        tokens.remember("aaaaaaaaaaa", auth("ctt-a"));
        tokens.next_song();
        assert_eq!(tokens.auth_for("aaaaaaaaaaa"), auth("ctt-a"), "the songs' tokens stay");
        tokens.forget_next_choice();
        assert_eq!(tokens.next_choice(), None);
        tokens.choose_next("ccccccccccc");
        tokens.clear();
        assert_eq!(tokens.next_choice(), None, "a new cast has no choice");
    }

    #[test]
    fn debug_output_shows_lengths_only() {
        let shown = format!("{:?}", auth("ctt-secret"));
        assert!(!shown.contains("ctt-secret"), "{shown}");
    }
}
