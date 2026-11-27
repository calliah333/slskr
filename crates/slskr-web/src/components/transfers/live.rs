//! The transfer list of one page, fetched from the list route and kept current by the
//! `transfers` topic in between.
//!
//! A fetch and the events race: the list can be served before or after an event that
//! arrives while the request runs. Changes received after a fetch started are therefore
//! kept and applied again on top of its result; the last event about a transfer is always
//! its newest state, because the daemon publishes every change.

use std::collections::HashMap;

use slskr_types::transfers::{Transfer, TransferDirection};

pub(super) struct LiveList {
    direction: TransferDirection,
    /// Whether the newest completed transfers are shown: only then do transfers that
    /// complete join the list (on later pages they belong to page one).
    first_page: bool,
    /// In the order of the list route: downloads as requested; uploads active first, then
    /// completed ones newest first.
    list: Vec<Transfer>,
    /// While a fetch runs: the changes received since it started, by id (`None`: removed).
    since_fetch: Option<HashMap<String, Option<Transfer>>>,
}

impl LiveList {
    pub(super) fn new(direction: TransferDirection) -> Self {
        Self {
            direction,
            first_page: true,
            list: Vec::new(),
            since_fetch: None,
        }
    }

    pub(super) fn list(&self) -> &[Transfer] {
        &self.list
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.since_fetch.is_some()
    }

    /// Whether the page shown is the first one (see `first_page`).
    pub(super) fn set_first_page(&mut self, first_page: bool) {
        self.first_page = first_page;
    }

    /// A fetch starts; changes from now on are applied again on top of its result.
    pub(super) fn begin_fetch(&mut self) {
        self.since_fetch = Some(HashMap::new());
    }

    /// The fetch failed or the list was unchanged (304): keep the current list.
    pub(super) fn end_fetch(&mut self) {
        self.since_fetch = None;
    }

    /// Replaces the list with a fetched one plus the changes received meanwhile.
    pub(super) fn finish_fetch(&mut self, mut list: Vec<Transfer>) {
        list.retain(|file| !file.removed);
        self.list = list;
        for (id, change) in self.since_fetch.take().unwrap_or_default() {
            match change {
                Some(file) => self.upsert(file),
                None => self.remove_one(&id),
            }
        }
    }

    /// Applies changed rows (`transfer_list`, `transfer_updated`) of this direction.
    pub(super) fn update(&mut self, files: &[Transfer]) {
        let direction = self.direction;
        for file in files.iter().filter(|file| file.direction == direction) {
            if let Some(changes) = &mut self.since_fetch {
                changes.insert(file.id.clone(), Some(file.clone()));
            }
            self.upsert(file.clone());
        }
    }

    /// Applies `transfer_removed`. Unknown ids are recorded too: the running fetch may
    /// still return them.
    pub(super) fn remove(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(changes) = &mut self.since_fetch {
                match changes.get_mut(id) {
                    Some(Some(file)) => file.removed = true,
                    _ => {
                        changes.insert(id.clone(), None);
                    }
                }
            }
            self.remove_one(id);
        }
    }

    fn remove_one(&mut self, id: &str) {
        let Some(position) = self.list.iter().position(|file| file.id == id) else {
            return;
        };
        self.list.remove(position);
    }

    fn upsert(&mut self, file: Transfer) {
        if file.removed {
            self.remove_one(&file.id);
            return;
        }
        let position = self.list.iter().position(|row| row.id == file.id);
        if position.is_none() && !self.first_page && file.state.is_complete() {
            return;
        }
        if self.direction == TransferDirection::Download {
            match position {
                Some(position) => self.list[position] = file,
                None => self.list.push(file),
            }
            return;
        }
        if let Some(position) = position {
            if self.list[position].state.is_complete() == file.state.is_complete() {
                self.list[position] = file;
                return;
            }
            self.list.remove(position);
        }
        // Active uploads come first; the newest completed one leads the rest.
        let boundary = self
            .list
            .iter()
            .position(|row| row.state.is_complete())
            .unwrap_or(self.list.len());
        self.list.insert(boundary, file);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use slskr_types::transfers::TransferState;

    use super::*;

    fn transfer(id: &str, direction: TransferDirection, state: TransferState) -> Transfer {
        let file = serde_json::json!({
            "id": id,
            "username": "peer",
            "direction": direction,
            "filename": format!("dir\\{id}"),
            "size": 10,
            "state": state,
            "requested_at": Utc::now(),
        });
        serde_json::from_value(file).unwrap()
    }

    fn ids(live: &LiveList) -> Vec<&str> {
        live.list().iter().map(|file| file.id.as_str()).collect()
    }

    #[test]
    fn changes_during_a_fetch_survive_its_older_result() {
        use TransferState::*;
        let down = |id, state| transfer(id, TransferDirection::Download, state);
        let mut live = LiveList::new(TransferDirection::Download);
        live.begin_fetch();
        assert!(live.is_fetching());
        // Events arrive while the request runs.
        live.update(&[down("b", InProgress)]);
        live.update(&[transfer("u", TransferDirection::Upload, InProgress)]);
        live.remove(&["a".into()]);
        live.finish_fetch(vec![down("a", Succeeded), down("b", QueuedRemotely)]);
        assert!(!live.is_fetching());
        assert_eq!(ids(&live), ["b"]);
        assert_eq!(live.list()[0].state, InProgress);

        // New transfers are appended, removed rows leave.
        live.update(&[down("c", QueuedLocally)]);
        let mut gone = down("b", Cancelled);
        gone.removed = true;
        live.update(&[gone]);
        assert_eq!(ids(&live), ["c"]);
    }

    #[test]
    fn uploads_keep_active_first_then_newest_completed() {
        use TransferState::*;
        let up = |id, state| transfer(id, TransferDirection::Upload, state);
        let mut live = LiveList::new(TransferDirection::Upload);
        live.begin_fetch();
        live.finish_fetch(vec![up("a", InProgress), up("old", Succeeded)]);
        live.update(&[up("b", QueuedLocally)]);
        assert_eq!(ids(&live), ["a", "b", "old"]);
        live.update(&[up("a", Succeeded)]);
        assert_eq!(ids(&live), ["b", "a", "old"]);
        live.update(&[up("a", Succeeded)]);
        assert_eq!(ids(&live), ["b", "a", "old"]);
    }
}
