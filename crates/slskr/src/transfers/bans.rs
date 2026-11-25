//! Bans peers whose uploads keep looping, and deletes banned users' failed uploads.
//!
//! The [backoff](super::backoff) limits what a looping peer costs. The ban is for a peer
//! that keeps doing it with no other outcome: `blacklist.ban_upload_loops` adds a user to
//! `blacklist.users` once [`LOOP_FILES`] of their files are looping, that is, the peer
//! accepted and then aborted each of them [`super::backoff::LOOP_FAILURES`] times while
//! not one upload to them succeeded (about half a day, given the backoff). Peers that are
//! offline, firewalled, slow to answer or refusing are never counted, nor are failures
//! reading the file here, and one success clears the counts. The own account and
//! privileged users are never banned this way. The ban is logged, shows in the settings,
//! and is undone by removing the name.
//!
//! Whenever the blacklist changes, and at startup, the failed and cancelled uploads of
//! blacklisted users are deleted from the store. Their successful uploads stay, for the
//! reports.

use rusqlite::params;
use slskr_types::{config::Config, transfers::TransferDirection};

use super::{Runtime, TransferService};

/// Looping files after which a user is banned.
const LOOP_FILES: usize = 5;
/// Rows deleted per statement, so other writers wait for one batch at most.
const DELETE_BATCH: i64 = 5_000;

impl TransferService {
    /// Whether `username` is due a ban for looping uploads; if so, forgets their backoff
    /// records so the ban is raised once.
    pub(super) fn loop_ban_due(&self, runtime: &mut Runtime, cfg: &Config, username: &str) -> bool {
        let due = cfg.blacklist.ban_upload_loops
            && runtime.backoff.looping_files(username) >= LOOP_FILES
            && !username.eq_ignore_ascii_case(&cfg.soulseek.username)
            && !self.0.users.is_blacklisted(username, None)
            && &*self.0.users.group(username) != "privileged";
        if due {
            runtime.backoff.forget(username);
        }
        due
    }

    /// Adds `username` to `blacklist.users`. Applying the change cancels their queued
    /// uploads and prunes their failed ones (see [`Self::prune_banned`]).
    pub(super) fn ban_looping_user(&self, username: String) {
        let service = self.clone();
        tokio::spawn(async move {
            let options = &service.0.core.options;
            let mut users = options.get().blacklist.users.clone();
            if users
                .iter()
                .any(|user| user.eq_ignore_ascii_case(&username))
            {
                return;
            }
            users.push(username.clone());
            match options
                .patch(serde_json::json!({ "blacklist": { "users": users } }))
                .await
            {
                Ok(_) => tracing::warn!(
                    %username,
                    "banned a user whose uploads kept failing in a loop; remove them from the blacklist to undo"
                ),
                Err(error) => tracing::warn!(
                    %username,
                    %error,
                    "could not ban a user whose uploads keep failing in a loop"
                ),
            }
        });
    }

    /// Deletes the failed and cancelled uploads of blacklisted users from the store.
    pub(super) async fn prune_banned(&self) {
        if let Err(error) = self.try_prune_banned().await {
            tracing::warn!(%error, "could not delete the uploads of blacklisted users");
        }
    }

    async fn try_prune_banned(&self) -> anyhow::Result<()> {
        let db = self.database();
        let names: Vec<String> = db
            .call(|db| {
                db.prepare("SELECT DISTINCT username FROM transfers WHERE direction = 'Upload'")?
                    .query_map([], |row| row.get(0))?
                    .collect()
            })
            .await?;
        for username in names
            .into_iter()
            .filter(|name| self.0.users.is_blacklisted(name, None))
        {
            let mut deleted = 0;
            loop {
                let name = username.clone();
                let batch: Vec<(String, bool)> = db
                    .call(move |db| {
                        db.prepare_cached(
                            "DELETE FROM transfers WHERE rowid IN (SELECT rowid FROM transfers
                                WHERE username = ?1 AND direction = 'Upload' AND (state & 16) != 0
                                AND state != 48 LIMIT ?2)
                            RETURNING id, removed",
                        )?
                        .query_map(params![name, DELETE_BATCH], |row| {
                            Ok((super::store::id(row, 0)?, row.get(1)?))
                        })?
                        .collect()
                    })
                    .await?;
                if batch.is_empty() {
                    break;
                }
                deleted += batch.len();
                self.wrote(TransferDirection::Upload);
                self.publish_removed(
                    batch
                        .into_iter()
                        .filter(|(_, removed)| !removed)
                        .map(|(id, _)| id)
                        .collect(),
                );
            }
            if deleted > 0 {
                tracing::info!(%username, deleted, "deleted the failed uploads of a blacklisted user");
                db.release_free_pages().await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfers::{new_transfer, store};
    use slskr_types::transfers::TransferState;

    async fn service(config: &str) -> (TransferService, crate::app::AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::bootstrap::test_app(dir.path(), config, &[])
            .await
            .unwrap();
        let service = app.services.transfers.clone();
        service.0.ready.send_replace(Some(Ok(())));
        (service, app, dir)
    }

    fn ended(username: &str, file: &str, state: TransferState) -> slskr_types::transfers::Transfer {
        let mut t = new_transfer(TransferDirection::Upload, username, file.into(), 100, None);
        t.state = state;
        t
    }

    #[tokio::test]
    async fn prunes_only_the_failed_uploads_of_blacklisted_users() {
        let (service, _app, _dir) =
            service("[soulseek]\nconnect = false\n[blacklist]\nusers = [\"Looper\"]\n").await;
        let db = service.database();
        let rows = [
            ("looper", "a", TransferState::Errored),
            ("looper", "a", TransferState::Rejected),
            ("looper", "b", TransferState::Succeeded),
            ("looper", "c", TransferState::QueuedLocally),
            ("fine", "a", TransferState::Errored),
        ];
        for (user, file, state) in rows {
            let t = ended(user, file, state);
            store::insert(db, vec![t.clone()]).await.unwrap();
            let bits = store::state_to_bits(state);
            db.call(move |db| {
                db.execute(
                    "UPDATE transfers SET state = ?1 WHERE id = ?2",
                    params![bits, store::key(&t.id)],
                )
            })
            .await
            .unwrap();
        }
        service.prune_banned().await;
        let left: Vec<(String, u32)> = db
            .call(|db| {
                db.prepare("SELECT username, state FROM transfers ORDER BY rowid")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .await
            .unwrap();
        let bits = store::state_to_bits;
        assert_eq!(
            left,
            [
                ("looper".into(), bits(TransferState::Succeeded)),
                ("looper".into(), bits(TransferState::QueuedLocally)),
                ("fine".into(), bits(TransferState::Errored)),
            ]
        );
    }

    #[tokio::test]
    async fn bans_a_user_once_several_files_loop_but_never_the_own_account() {
        let (service, app, _dir) =
            service("[soulseek]\nconnect = false\nusername = \"me\"\npassword = \"pw\"\n").await;
        let cfg = app.options.get();
        let abort = |user: &str, files: &[&str]| {
            let mut runtime = service.runtime();
            for file in files {
                for _ in 0..crate::transfers::backoff::LOOP_FAILURES {
                    let t = ended(user, file, TransferState::Errored);
                    runtime.backoff.record(&t, 0, true);
                }
            }
        };
        let age = |user: &str| {
            let duration = crate::transfers::backoff::LOOP_DURATION;
            service.runtime().backoff.backdate(user, duration);
        };
        let due = |user: &str| service.loop_ban_due(&mut service.runtime(), &cfg, user);

        abort("Me", &["a", "b", "c", "d", "e"]);
        age("Me");
        assert!(!due("Me"));

        abort("looper", &["a", "b", "c", "d"]);
        age("looper");
        assert!(!due("looper"), "four files");
        abort("looper", &["e"]);
        assert!(!due("looper"), "the fifth has not looped for a day");
        age("looper");
        assert!(due("looper"));
        // Raised once.
        assert!(!due("looper"));

        let mut changes = app.options.subscribe();
        service.ban_looping_user("looper".into());
        changes.changed().await.unwrap();
        assert_eq!(app.options.get().blacklist.users, ["looper"]);
        assert!(service.0.users.is_blacklisted("Looper", None));
    }

    #[test]
    fn only_accepted_then_dropped_uploads_are_aborts() {
        use soulseek::Error;
        use std::path::PathBuf;
        let aborted =
            |prior, result: soulseek::Result<PathBuf>| super::super::peer_aborted(prior, &result);
        let closed = || {
            Err(Error::Connection(format!(
                "{}: Remote connection closed",
                super::super::UPLOAD_OFFSET_FAILURE
            )))
        };
        assert!(aborted(TransferState::Initializing, closed()));
        // A connection dropped mid-file can be a bad link on either side.
        assert!(!aborted(
            TransferState::InProgress,
            Err(Error::Connection("reset".into()))
        ));
        assert!(!aborted(
            TransferState::InProgress,
            Err(Error::Timeout("stalled".into()))
        ));
        // Unreachable, unanswered, refused, or this side's problem.
        assert!(!aborted(
            TransferState::Initializing,
            Err(Error::Connection(
                "Failed to establish a F connection".into()
            ))
        ));
        assert!(!aborted(TransferState::Requested, closed()));
        assert!(!aborted(
            TransferState::Requested,
            Err(Error::Timeout("no answer".into()))
        ));
        assert!(!aborted(
            TransferState::Requested,
            Err(Error::TransferRejected("Cancelled".into()))
        ));
        assert!(!aborted(
            TransferState::InProgress,
            Err(Error::TransferStream("read".into()))
        ));
        assert!(!aborted(TransferState::InProgress, Err(Error::Cancelled)));
        assert!(!aborted(TransferState::InProgress, Ok(PathBuf::new())));
    }
}
