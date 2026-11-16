//! The active Lounge theme's senders: who is connected now, and which
//! device ids took part in the running cast. Ids are never logged.

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
}

impl Senders {
    pub fn any_connected(&self) -> bool {
        !self.connected.is_empty()
    }

    /// A sender connected (`ids` from remoteConnected or loungeStatus, `name`
    /// when it gave no id).
    pub fn connect(&mut self, ids: &[String], name: Option<&str>) {
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

    /// The Lounge's list of who is connected now (`ids`; `listed`: anyone at
    /// all). It replaces the known senders, so one whose leave was missed is
    /// gone. A list of senders without readable ids keeps the known ones.
    pub fn replace_connected(&mut self, ids: &[String], listed: bool) {
        if !ids.is_empty() {
            self.connected = ids.iter().cloned().map(Sender::Id).collect();
        } else if !listed {
            self.connected.clear();
        } else if self.connected.is_empty() {
            self.connected.push(Sender::Unidentified);
        }
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

    /// The cast ended: the departed senders' ids go, the connected stay.
    pub fn end_cast(&mut self) {
        let connected = &self.connected;
        self.cast_ids.retain(|id| {
            connected.contains(&Sender::Id(id.clone()))
        });
    }

    /// The senders were dropped, or another theme took over.
    pub fn clear(&mut self) {
        self.connected.clear();
        self.cast_ids.clear();
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
        senders.replace_connected(&ids(&["phone"]), true);
        senders.disconnect(Some("phone"), Some("Phone"));
        assert!(!senders.any_connected(), "the iPad is not left behind as a ghost");
        assert_eq!(senders.cast_ids(), ids(&["phone", "ipad"]).as_slice(), "the cast's ids stay");

        senders.connect(&ids(&["phone"]), None);
        senders.replace_connected(&[], false);
        assert!(!senders.any_connected(), "nobody listed");

        senders.replace_connected(&ids(&["tv"]), true);
        assert!(senders.cast_ids().contains(&"tv".to_owned()));
    }

    #[test]
    fn a_lounge_status_listing_senders_without_readable_ids_keeps_someone_connected() {
        let mut senders = Senders::default();
        senders.replace_connected(&[], true);
        assert!(senders.any_connected());
        let mut senders = Senders::default();
        senders.connect(&[], Some("Phone"));
        senders.replace_connected(&[], true);
        senders.disconnect(None, Some("Phone"));
        assert!(!senders.any_connected(), "the known sender is kept, not doubled");
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
    fn clearing_forgets_everyone() {
        let mut senders = Senders::default();
        senders.connect(&ids(&["phone"]), Some("Phone"));
        senders.clear();
        assert!(!senders.any_connected());
        assert!(senders.cast_ids().is_empty());
    }
}
