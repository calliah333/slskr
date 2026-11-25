//! Backs off uploads of a file that keep failing for the same user.
//!
//! Some peers re-queue a file the moment an upload of it fails, and then fail it again
//! the same way: they close the file connection before sending the resume offset, reject
//! the request, or drop the transfer after a few seconds and ask for it again from byte
//! 0. Left alone that loops every few seconds for days, and each round costs a peer
//! connection, a stored transfer row and often a slot that another user could have used.
//! (One such peer accounted for over a third of a long-running database.)
//!
//! After [`FREE_FAILURES`] failures in a row, the next attempt waits in the queue: one
//! minute, doubling up to an hour. The peer sees the file stay queued, so it stops
//! re-queuing. A success, or a failure that resumed past where the last one ended, clears
//! the record.

use std::{collections::HashMap, time::Duration};

use slskr_types::transfers::{Transfer, TransferState};
use tokio::time::Instant;

/// Failures in a row that are retried at once, as transient errors usually are.
const FREE_FAILURES: u32 = 2;
const BASE_DELAY: Duration = Duration::from_secs(60);
const MAX_DELAY: Duration = Duration::from_secs(3600);
/// A record with no failure for this long is forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(6 * 3600);

struct Strikes {
    failures: u32,
    last: Instant,
    /// The furthest byte any failed attempt reached.
    reached: u64,
}

#[derive(Default)]
pub(super) struct UploadBackoff(HashMap<(String, String), Strikes>);

impl UploadBackoff {
    /// How long the next upload of `filename` to `username` should wait, if at all.
    pub(super) fn delay(&self, username: &str, filename: &str) -> Option<Duration> {
        let strikes = self.0.get(&(username.to_owned(), filename.to_owned()))?;
        let wait = backoff(strikes.failures)?;
        wait.checked_sub(strikes.last.elapsed())
            .filter(|wait| !wait.is_zero())
    }

    /// Records how an upload ended; `offset` is where the peer asked it to start.
    pub(super) fn record(&mut self, t: &Transfer, offset: u64) {
        let key = (t.username.clone(), t.filename.clone());
        match t.state {
            TransferState::Succeeded => {
                self.0.remove(&key);
            }
            // Cancelled here: not the peer failing.
            TransferState::Cancelled => {}
            _ => {
                // `bytes_transferred` counts from the start of the file.
                let reached = t.bytes_transferred.max(offset);
                let strikes = self.0.entry(key).or_insert(Strikes {
                    failures: 0,
                    last: Instant::now(),
                    reached: 0,
                });
                // A peer that resumes past where it stopped is making progress, however
                // often it fails. One that starts over from 0 is not.
                if offset > 0 && reached > strikes.reached {
                    strikes.failures = 0;
                }
                strikes.failures = strikes.failures.saturating_add(1);
                strikes.reached = strikes.reached.max(reached);
                strikes.last = Instant::now();
            }
        }
    }

    pub(super) fn prune(&mut self) {
        self.0
            .retain(|_, strikes| strikes.last.elapsed() < FORGET_AFTER);
    }
}

fn backoff(failures: u32) -> Option<Duration> {
    let over = failures.checked_sub(FREE_FAILURES)?;
    Some(
        BASE_DELAY
            .saturating_mul(1_u32 << over.min(16))
            .min(MAX_DELAY),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use slskr_types::transfers::TransferDirection;

    const USER: &str = "peer";
    const FILE: &str = "music\\a.flac";

    fn end(backoff: &mut UploadBackoff, state: TransferState, offset: u64, bytes: u64) {
        let mut t =
            super::super::new_transfer(TransferDirection::Upload, USER, FILE.into(), 100, None);
        t.state = state;
        t.bytes_transferred = offset + bytes;
        backoff.record(&t, offset);
    }

    #[test]
    fn backoff_starts_after_the_free_failures_and_doubles_to_a_cap() {
        assert_eq!(backoff(0), None);
        assert_eq!(backoff(1), None);
        assert_eq!(backoff(2), Some(Duration::from_secs(60)));
        assert_eq!(backoff(3), Some(Duration::from_secs(120)));
        assert_eq!(backoff(8), Some(Duration::from_secs(3600)));
        assert_eq!(backoff(u32::MAX), Some(Duration::from_secs(3600)));
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_failures_delay_the_file_until_a_success() {
        let mut backoff = UploadBackoff::default();
        end(&mut backoff, TransferState::Errored, 0, 0);
        assert_eq!(backoff.delay(USER, FILE), None);
        end(&mut backoff, TransferState::Rejected, 0, 0);
        assert_eq!(backoff.delay(USER, FILE), Some(Duration::from_secs(60)));
        assert_eq!(backoff.delay(USER, "music\\b.flac"), None);
        assert_eq!(backoff.delay("other", FILE), None);

        tokio::time::advance(Duration::from_secs(45)).await;
        assert_eq!(backoff.delay(USER, FILE), Some(Duration::from_secs(15)));
        tokio::time::advance(Duration::from_secs(15)).await;
        assert_eq!(backoff.delay(USER, FILE), None);

        end(&mut backoff, TransferState::TimedOut, 0, 0);
        assert_eq!(backoff.delay(USER, FILE), Some(Duration::from_secs(120)));
        end(&mut backoff, TransferState::Cancelled, 0, 0);
        assert_eq!(backoff.delay(USER, FILE), Some(Duration::from_secs(120)));
        end(&mut backoff, TransferState::Succeeded, 0, 100);
        assert_eq!(backoff.delay(USER, FILE), None);
        assert!(backoff.0.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn restarting_from_zero_is_not_progress_but_resuming_further_is() {
        let mut backoff = UploadBackoff::default();
        for bytes in [30, 40, 35] {
            end(&mut backoff, TransferState::Errored, 0, bytes);
        }
        assert!(backoff.delay(USER, FILE).is_some());
        end(&mut backoff, TransferState::Errored, 40, 20);
        assert_eq!(backoff.delay(USER, FILE), None);
        end(&mut backoff, TransferState::Errored, 40, 10);
        assert!(backoff.delay(USER, FILE).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_records_are_forgotten() {
        let mut backoff = UploadBackoff::default();
        end(&mut backoff, TransferState::Errored, 0, 0);
        tokio::time::advance(FORGET_AFTER).await;
        backoff.prune();
        assert!(backoff.0.is_empty());
    }
}
