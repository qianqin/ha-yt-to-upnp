//! The active Lounge theme's senders: who is connected now, and which
//! device ids took part in the running cast. Ids are never logged.
//!
//! Known residual: an id-less newcomer joining while a ghost (a sender whose
//! remoteDisconnected was lost, with no rebind or list since) still counts
//! as connected is taken as co-present and sees the cast's tokens; without
//! an id there is nothing to tell the two apart.

/// One connected sender: its device id, else its name, else neither (a
/// sender the Lounge did not identify).
#[derive(Debug, PartialEq)]
enum Sender {
    Id(String),
    Named(String),
    Unidentified,
}

#[derive(Default)]
pub struct Senders {
    /// One entry per connected sender: two phones of the same name without
    /// an id are two entries.
    connected: Vec<Sender>,
    /// Every device id seen since the cast began, connected or departed:
    /// /next's MDX context, and how a returning sender is recognised.
    cast_ids: Vec<String>,
    /// The receiver's rebind listed nobody (`forget_connected`), and no
    /// sender event has come since: a phone may still be connected.
    unlisted: bool,
    /// The device ids connected when the running cast's setPlaylist or
    /// setVideo was accepted: who cast it (the Lounge does not say which
    /// sender sent a cast, so all connected then count). A later cast
    /// replaces them.
    casters: Vec<String>,
}

impl Senders {
    pub fn any_connected(&self) -> bool {
        !self.connected.is_empty()
    }

    /// A sender connected (`ids` from remoteConnected or loungeStatus, `name`
    /// when it gave no id).
    pub fn connect(&mut self, ids: &[String], name: Option<&str>) {
        self.unlisted = false;
        if ids.is_empty() {
            let sender = name.filter(|n| !n.is_empty()).map_or(Sender::Unidentified, |n| Sender::Named(n.to_owned()));
            self.connected.push(sender);
        }
        for id in ids {
            let sender = Sender::Id(id.clone());
            if !self.connected.contains(&sender) {
                self.connected.push(sender);
            }
        }
        self.note_ids(ids);
    }

    /// The Lounge's list of who is connected now: `ids`, and `unidentified`
    /// senders without a readable id. It replaces the known senders, so one
    /// whose leave was missed is gone. The listed senders without an id are
    /// the known ones without an id (named or not), first-connected first,
    /// padded with unidentified ones.
    pub fn replace_connected(&mut self, ids: &[String], unidentified: usize) {
        self.unlisted = false;
        let mut without_id: Vec<Sender> =
            self.connected.drain(..).filter(|sender| !matches!(sender, Sender::Id(_))).collect();
        // In connect order, cut or padded to the listed number.
        without_id.resize_with(unidentified, || Sender::Unidentified);
        self.connected = ids.iter().cloned().map(Sender::Id).chain(without_id).collect();
        self.note_ids(ids);
    }

    fn note_ids(&mut self, ids: &[String]) {
        for id in ids {
            if !self.cast_ids.contains(id) {
                self.cast_ids.push(id.clone());
            }
        }
    }

    /// A sender left, found by its id, else by its name. One that matches
    /// no connected sender leaves none connected: better to forget a cast
    /// than to keep it for a sender who is gone.
    pub fn disconnect(&mut self, id: Option<&str>, name: Option<&str>) {
        self.unlisted = false;
        let by_id = id.map(|id| Sender::Id(id.to_owned()));
        let by_name = name.filter(|n| !n.is_empty()).map(|n| Sender::Named(n.to_owned()));
        let gone = [by_id, by_name]
            .into_iter()
            .flatten()
            .find_map(|sender| self.connected.iter().position(|known| *known == sender));
        match gone {
            Some(index) => {
                self.connected.remove(index);
            }
            None => self.connected.clear(),
        }
    }

    pub fn cast_ids(&self) -> &[String] {
        &self.cast_ids
    }

    /// Whether any of `ids` took part in the running cast.
    pub fn took_part(&self, ids: &[String]) -> bool {
        ids.iter().any(|id| self.cast_ids.contains(id))
    }

    /// The cast ended: the departed senders' ids go, the connected stay, and
    /// an unlisted sender is no longer awaited.
    pub fn end_cast(&mut self) {
        self.unlisted = false;
        self.casters.clear();
        let connected = &self.connected;
        self.cast_ids.retain(|id| {
            connected.contains(&Sender::Id(id.clone()))
        });
    }

    /// The senders were dropped, or another theme took over.
    pub fn clear(&mut self) {
        self.connected.clear();
        self.cast_ids.clear();
        self.unlisted = false;
        self.casters.clear();
    }

    /// A cast was accepted: the senders connected now cast it.
    pub fn record_casters(&mut self) {
        self.casters = self
            .connected
            .iter()
            .filter_map(|sender| match sender {
                Sender::Id(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
    }

    /// Whether `id` cast the running cast (see `record_casters`).
    pub fn is_caster(&self, id: &str) -> bool {
        self.casters.iter().any(|caster| caster == id)
    }

    /// The receiver's own rebind listed nobody: a sender whose leave was
    /// lost is no longer counted, but one still connected may not be listed
    /// yet (see `unlisted_since_rebind`). The cast's ids stay.
    pub fn forget_connected(&mut self) {
        self.connected.clear();
        self.unlisted = true;
    }

    /// Since a rebind listed nobody, no sender event has come: a cast from
    /// the Lounge is a connected sender speaking.
    pub fn unlisted_since_rebind(&self) -> bool {
        self.unlisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_sender_leaving_keeps_the_others_connected() {
        let mut senders = Senders::default();
        assert!(!senders.any_connected());
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        senders.disconnect(Some("ipad"), Some("iPad"));
        assert!(senders.any_connected(), "the phone is still there");
        senders.disconnect(Some("phone"), Some("Phone"));
        assert!(!senders.any_connected());
    }

    #[test]
    fn senders_without_an_id_are_told_apart_by_name() {
        let mut senders = Senders::default();
        senders.connect(&[], Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        senders.disconnect(None, Some("Phone"));
        assert!(senders.any_connected());
        senders.disconnect(Some("ipad"), None);
        assert!(!senders.any_connected());
    }

    #[test]
    fn a_disconnect_that_names_no_known_sender_drops_them_all() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        senders.disconnect(None, None);
        assert!(!senders.any_connected());
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.disconnect(Some("someone-else"), None);
        assert!(!senders.any_connected());
    }

    #[test]
    fn a_loungestatus_lists_several_senders() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone", "ipad"]), None);
        senders.disconnect(Some("phone"), None);
        assert!(senders.any_connected());
    }

    #[test]
    fn the_casts_ids_are_the_union_of_all_senders_seen() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.disconnect(Some("phone"), None);
        senders.disconnect(Some("ipad"), None);
        assert_eq!(senders.cast_ids(), ids(&["phone", "ipad"]).as_slice(), "departed senders stay for /next");
        assert!(senders.took_part(&ids(&["phone"])));
        assert!(senders.took_part(&ids(&["ipad", "tv"])));
        assert!(!senders.took_part(&ids(&["tv"])));
        assert!(!senders.took_part(&[]), "a sender without an id is not known");
    }

    #[test]
    fn ending_the_cast_keeps_only_the_connected_senders_ids() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        senders.disconnect(Some("ipad"), None);
        senders.end_cast();
        assert_eq!(senders.cast_ids(), ids(&["phone"]).as_slice());
        assert!(senders.any_connected());
    }

    #[test]
    fn a_lounge_status_replaces_the_connected_senders() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.connect(&ids(&["ipad"]), Some("iPad"));
        // The iPad's remoteDisconnected was missed.
        senders.replace_connected(&ids(&["phone"]), 0);
        senders.disconnect(Some("phone"), Some("Phone"));
        assert!(!senders.any_connected(), "the iPad is not left behind as a ghost");
        assert_eq!(senders.cast_ids(), ids(&["phone", "ipad"]).as_slice(), "the cast's ids stay");

        senders.connect(&ids(&["phone"]), None);
        senders.replace_connected(&[], 0);
        assert!(!senders.any_connected(), "nobody listed");

        senders.replace_connected(&ids(&["tv"]), 0);
        assert!(senders.cast_ids().contains(&"tv".to_owned()));
    }

    #[test]
    fn a_lounge_status_counts_each_sender_without_a_readable_id() {
        let mut senders = Senders::default();
        senders.replace_connected(&[], 1);
        assert!(senders.any_connected());
        senders.replace_connected(&[], 1);
        senders.replace_connected(&[], 1);
        senders.disconnect(None, Some("someone"));
        assert!(!senders.any_connected(), "repeated lists add no ghosts");

        // A named sender without an id stays known by its name.
        let mut senders = Senders::default();
        senders.connect(&[], Some("Phone"));
        senders.replace_connected(&[], 1);
        senders.disconnect(None, Some("Phone"));
        assert!(!senders.any_connected(), "the known sender is kept, not doubled");

        // Two known without an id, one listed: one is gone.
        let mut senders = Senders::default();
        senders.connect(&[], Some("Phone"));
        senders.connect(&[], Some("iPad"));
        senders.replace_connected(&[], 1);
        senders.disconnect(None, Some("Phone"));
        assert!(!senders.any_connected());
    }

    #[test]
    fn a_shorter_list_keeps_the_first_connected_senders_without_an_id() {
        let mut senders = Senders::default();
        for name in ["Phone", "iPad", "TV"] {
            senders.connect(&[], Some(name));
        }
        senders.replace_connected(&[], 2);
        senders.disconnect(None, Some("Phone"));
        assert!(senders.any_connected(), "the iPad is still known by name");
        senders.disconnect(None, Some("iPad"));
        assert!(!senders.any_connected(), "the TV, connected last, was dropped");
    }

    #[test]
    fn a_mixed_lounge_status_keeps_the_sender_without_an_id() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), None);
        senders.connect(&[], Some("iPad"));
        senders.replace_connected(&ids(&["phone"]), 1);
        senders.disconnect(Some("phone"), None);
        assert!(senders.any_connected(), "the iPad is still listed");
        senders.disconnect(None, Some("iPad"));
        assert!(!senders.any_connected());

        let mut senders = Senders::default();
        senders.replace_connected(&ids(&["phone"]), 1);
        senders.disconnect(Some("phone"), None);
        assert!(senders.any_connected(), "an unidentified sender was listed too");
    }

    #[test]
    fn two_senders_without_an_id_by_the_same_name_count_twice() {
        let mut senders = Senders::default();
        senders.connect(&[], Some("Phone"));
        senders.connect(&[], Some("Phone"));
        senders.disconnect(None, Some("Phone"));
        assert!(senders.any_connected(), "the second phone is still there");
        senders.disconnect(None, Some("Phone"));
        assert!(!senders.any_connected());
    }

    #[test]
    fn a_sender_without_id_or_name_is_never_matched_by_an_empty_name() {
        let mut senders = Senders::default();
        senders.connect(&[], Some(""));
        assert!(senders.any_connected());
        senders.connect(&[], Some("Phone"));
        // A leave with an empty name names nobody: everyone is dropped.
        senders.disconnect(None, Some(""));
        assert!(!senders.any_connected());
    }

    #[test]
    fn a_rebind_listing_nobody_leaves_the_senders_unknown_until_one_speaks() {
        let mut senders = Senders::default();
        assert!(!senders.unlisted_since_rebind(), "a fresh start knows nobody spoke");
        senders.connect(&ids(&["phone"]), None);
        senders.forget_connected();
        assert!(!senders.any_connected());
        assert!(senders.unlisted_since_rebind());
        assert_eq!(senders.cast_ids(), ids(&["phone"]).as_slice());
        type Event = fn(&mut Senders);
        let events: [Event; 5] = [
            |s| s.connect(&[], None),
            |s| s.replace_connected(&[], 0),
            |s| s.disconnect(None, None),
            |s| s.clear(),
            |s| s.end_cast(),
        ];
        for event in events {
            let mut senders = Senders::default();
            senders.forget_connected();
            event(&mut senders);
            assert!(!senders.unlisted_since_rebind());
        }
    }

    #[test]
    fn the_senders_connected_when_a_cast_starts_are_its_casters() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), None);
        senders.record_casters();
        senders.connect(&ids(&["ipad"]), None);
        assert!(senders.is_caster("phone"));
        assert!(!senders.is_caster("ipad"), "joined later: a co-listener");
        assert!(senders.took_part(&ids(&["ipad"])), "though it took part");
        // A later cast (the iPad's setPlaylist, say): those connected then.
        senders.record_casters();
        assert!(senders.is_caster("ipad") && senders.is_caster("phone"));
        senders.disconnect(Some("ipad"), None);
        assert!(senders.is_caster("ipad"), "a caster that left stays one");
    }

    #[test]
    fn the_casters_go_with_the_cast() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), None);
        senders.record_casters();
        senders.end_cast();
        assert!(!senders.is_caster("phone"));
        senders.record_casters();
        senders.clear();
        assert!(!senders.is_caster("phone"));
    }

    #[test]
    fn clearing_forgets_everyone() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.clear();
        assert!(!senders.any_connected());
        assert!(senders.cast_ids().is_empty());
    }
}
