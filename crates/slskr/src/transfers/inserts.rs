//! Stores queued uploads in groups.
//!
//! Peers queue a whole folder at once: one request per file, each of which used to be its
//! own transaction. A burst of a few hundred files then queued on the writer connection
//! for seconds (28 s of waiting in one 15-minute window on the live instance). Instead,
//! each new upload joins a queue, and one writer task stores everything queued so far in
//! a single transaction, then whatever arrived meanwhile, until the queue is empty.

use std::mem;

use slskr_types::transfers::{Transfer, TransferDirection};
use tokio::sync::oneshot;

use super::{TransferService, store};

/// Rows per transaction, so one burst does not hold the writer for long.
const MAX_BATCH: usize = 500;

type Done = oneshot::Sender<Result<(), String>>;

#[derive(Default)]
pub(super) struct UploadInserts {
    queued: Vec<(Transfer, Done)>,
    /// A writer task is draining `queued`.
    writing: bool,
    /// Transactions written, for tests.
    #[cfg(test)]
    pub(super) batches: usize,
}

impl TransferService {
    /// Stores a new upload, together with any others queued at the same time.
    pub(super) async fn store_upload(&self, transfer: Transfer) -> anyhow::Result<()> {
        let (done, stored) = oneshot::channel();
        let start = {
            let mut inserts = self.upload_inserts();
            inserts.queued.push((transfer, done));
            !mem::replace(&mut inserts.writing, true)
        };
        if start {
            // A task of its own, so a caller that goes away cannot strand the queue.
            tokio::spawn(self.clone().write_queued_uploads());
        }
        stored
            .await
            .map_err(|_| anyhow::anyhow!("the upload was not stored"))?
            .map_err(anyhow::Error::msg)
    }

    async fn write_queued_uploads(self) {
        loop {
            let batch = {
                let mut inserts = self.upload_inserts();
                if inserts.queued.is_empty() {
                    inserts.writing = false;
                    return;
                }
                let take = inserts.queued.len().min(MAX_BATCH);
                let rest = inserts.queued.split_off(take);
                #[cfg(test)]
                {
                    inserts.batches += 1;
                }
                mem::replace(&mut inserts.queued, rest)
            };
            let (transfers, done): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
            let result = store::insert(self.database(), transfers).await;
            self.wrote(TransferDirection::Upload);
            let result = match result {
                Ok(superseded) => {
                    self.publish_removed(superseded);
                    Ok(())
                }
                Err(error) => Err(format!("{error:#}")),
            };
            for done in done {
                let _ = done.send(result.clone());
            }
        }
    }

    pub(super) fn upload_inserts(&self) -> std::sync::MutexGuard<'_, UploadInserts> {
        self.0
            .upload_inserts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use crate::transfers::new_transfer;
    use slskr_types::transfers::TransferDirection;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_are_stored_in_few_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::bootstrap::test_app(dir.path(), "[soulseek]\nconnect = false\n", &[])
            .await
            .unwrap();
        let service = app.services.transfers.clone();
        let stores = (0..300).map(|n| {
            let service = service.clone();
            tokio::spawn(async move {
                let t = new_transfer(TransferDirection::Upload, "peer", format!("f{n}"), 1, None);
                service.store_upload(t).await
            })
        });
        for store in futures::future::join_all(stores).await {
            store.unwrap().unwrap();
        }
        let rows: i64 = service
            .database()
            .call(|db| db.query_row("SELECT count(*) FROM transfers", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(rows, 300);
        let inserts = service.upload_inserts();
        assert!(!inserts.writing && inserts.queued.is_empty());
        assert!(inserts.batches < 300, "{} transactions", inserts.batches);
    }
}
