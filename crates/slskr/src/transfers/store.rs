use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use slskr_api::{
    config::RetentionConfig,
    transfers::{Batch, BatchOptions, Transfer, TransferDirection, TransferStates},
};

use crate::db::Database;

pub(super) const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE transfer_batches (
    id TEXT PRIMARY KEY, search_id TEXT, username TEXT NOT NULL,
    created_at TEXT NOT NULL, destination TEXT, external_id TEXT
);
CREATE TABLE transfers (
    id TEXT PRIMARY KEY, batch_id TEXT, username TEXT NOT NULL, direction TEXT NOT NULL,
    filename TEXT NOT NULL, size INTEGER NOT NULL, start_offset INTEGER NOT NULL DEFAULT 0,
    state INTEGER NOT NULL, state_description TEXT, requested_at TEXT NOT NULL, enqueued_at TEXT,
    started_at TEXT, ended_at TEXT, bytes_transferred INTEGER NOT NULL DEFAULT 0,
    average_speed REAL NOT NULL DEFAULT 0, exception TEXT, attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT, removed INTEGER NOT NULL DEFAULT 0, place_in_queue INTEGER
);
CREATE INDEX ix_transfers_username ON transfers(username);
CREATE INDEX ix_transfers_state ON transfers(state);
CREATE INDEX ix_transfers_batch_id ON transfers(batch_id);
CREATE INDEX ix_transfers_file ON transfers(direction, username, filename);
"#];

const FIELDS: &str = "id, batch_id, username, direction, filename, size, state, requested_at, enqueued_at,\n    started_at, ended_at, bytes_transferred, average_speed, place_in_queue, exception, attempts, next_attempt_at, removed";

pub(super) fn direction_name(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => "Download",
        TransferDirection::Upload => "Upload",
    }
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn date(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<String>>(i)?
        .map(|text| {
            text.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    i,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn transfer(row: &Row<'_>) -> rusqlite::Result<Transfer> {
    let mut transfer = Transfer {
        id: row.get(0)?,
        batch_id: row.get(1)?,
        username: row.get(2)?,
        direction: if row.get_ref(3)?.as_str()? == "Upload" {
            TransferDirection::Upload
        } else {
            TransferDirection::Download
        },
        filename: row.get(4)?,
        size: row.get(5)?,
        state: TransferStates::from_bits(row.get(6)?),
        requested_at: date(row, 7)?.ok_or(rusqlite::Error::InvalidQuery)?,
        enqueued_at: date(row, 8)?,
        started_at: date(row, 9)?,
        ended_at: date(row, 10)?,
        bytes_transferred: row.get(11)?,
        average_speed: row.get(12)?,
        place_in_queue: row.get(13)?,
        exception: row.get(14)?,
        attempts: row.get(15)?,
        next_attempt_at: date(row, 16)?,
        removed: row.get(17)?,
        bytes_remaining: 0,
        elapsed_time: None,
        percent_complete: 0.0,
        remaining_time: None,
    };
    transfer.refresh_derived(Utc::now());
    Ok(transfer)
}

pub(super) async fn initialize(db: &Database) -> anyhow::Result<(Vec<Transfer>, Vec<Transfer>)> {
    db.call(|db| {
        let tx = db.transaction()?;
        let now = Utc::now();
        tx.execute(
            "UPDATE transfers SET state = 1040, state_description = 'Completed, Aborted', ended_at = ?1,
                next_attempt_at = NULL, exception = 'OperationCanceledException: Interrupted by daemon shutdown'
                WHERE (state & 16) = 0 AND (direction = 'Upload' OR removed)",
            [timestamp(now)],
        )?;
        // An interrupted negotiation is not a failed attempt. Move it back to the
        // queue once, while preserving delays for already scheduled retries.
        tx.execute(
            "UPDATE transfers SET state = 2050, state_description = 'Queued, Locally', ended_at = NULL,
                attempts = CASE WHEN next_attempt_at IS NULL THEN MAX(0, attempts - 1) ELSE attempts END,
                next_attempt_at = COALESCE(next_attempt_at, ?1), place_in_queue = NULL
                WHERE direction = 'Download' AND (state & 16) = 0",
            [timestamp(now)],
        )?;
        let history = {
            let mut stmt = tx.prepare(&format!("SELECT {FIELDS} FROM transfers WHERE direction = 'Upload' AND started_at >= ?1"))?;
            stmt.query_map([timestamp(now - chrono::TimeDelta::days(7))], transfer)?.collect::<rusqlite::Result<_>>()?
        };
        let downloads = {
            let mut stmt = tx.prepare(&format!("SELECT {FIELDS} FROM transfers WHERE direction = 'Download' AND (state & 16) = 0 ORDER BY rowid"))?;
            stmt.query_map([], transfer)?.collect::<rusqlite::Result<_>>()?
        };
        tx.commit()?;
        Ok((history, downloads))
    }).await
}

pub(super) async fn insert(db: &Database, transfers: Vec<Transfer>) -> anyhow::Result<()> {
    db.call(move |db| {
        let tx = db.transaction()?;
        {
            let mut supersede = tx.prepare("UPDATE transfers SET removed = 1 WHERE direction = ?1 AND username = ?2
                AND filename = ?3 AND removed = 0 AND (state & 16) != 0")?;
            let mut insert = tx.prepare("INSERT INTO transfers (id, batch_id, username, direction, filename, size,
                state, state_description, requested_at, enqueued_at, removed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)")?;
            for t in transfers {
                let direction = direction_name(t.direction);
                supersede.execute(params![direction, t.username, t.filename])?;
                insert.execute(params![t.id, t.batch_id, t.username, direction, t.filename, t.size,
                    t.state.bits(), t.state.to_string(), timestamp(t.requested_at), t.enqueued_at.map(timestamp), t.removed])?;
            }
        }
        tx.commit()
    }).await
}

pub(super) struct Snapshot {
    id: String,
    size: u64,
    offset: u64,
    state: TransferStates,
    enqueued: Option<DateTime<Utc>>,
    started: Option<DateTime<Utc>>,
    ended: Option<DateTime<Utc>>,
    bytes: u64,
    speed: f64,
    exception: Option<String>,
    attempts: u32,
    next_attempt: Option<DateTime<Utc>>,
    removed: bool,
    position: Option<u32>,
}

impl Snapshot {
    pub(super) fn new(t: &Transfer, offset: u64) -> Self {
        Self {
            id: t.id.clone(),
            size: t.size,
            offset,
            state: t.state,
            enqueued: t.enqueued_at,
            started: t.started_at,
            ended: t.ended_at,
            bytes: t.bytes_transferred,
            speed: t.average_speed,
            exception: t.exception.clone(),
            attempts: t.attempts,
            next_attempt: t.next_attempt_at,
            removed: t.removed,
            position: t.place_in_queue,
        }
    }
}

pub(super) async fn save(db: &Database, snapshots: Vec<Snapshot>) -> anyhow::Result<()> {
    db.call(move |db| {
        let tx = db.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE transfers SET size = ?2, start_offset = ?3, state = ?4, enqueued_at = ?5,
                started_at = ?6, ended_at = ?7, bytes_transferred = ?8, average_speed = ?9, exception = ?10,
                attempts = ?11, next_attempt_at = ?12, removed = MAX(removed, ?13), state_description = ?14, place_in_queue = ?15
                WHERE id = ?1 AND (state & 16) = 0 AND attempts <= ?11")?;
            for t in snapshots {
                stmt.execute(params![t.id, t.size, t.offset, t.state.bits(), t.enqueued.map(timestamp),
                    t.started.map(timestamp), t.ended.map(timestamp), t.bytes, t.speed,
                    t.exception, t.attempts, t.next_attempt.map(timestamp), t.removed, t.state.to_string(), t.position])?;
            }
        }
        tx.commit()
    }).await
}

pub(super) async fn list(
    db: &Database,
    direction: TransferDirection,
    username: Option<String>,
    include_removed: bool,
) -> anyhow::Result<Vec<Transfer>> {
    db.call(move |db| {
        let mut stmt = db.prepare(&format!("SELECT {FIELDS} FROM transfers WHERE direction = ?1\n            AND (?2 IS NULL OR username = ?2) AND (?3 OR NOT removed) ORDER BY rowid"))?;
        stmt.query_map(params![direction_name(direction), username, include_removed], transfer)?.collect()
    }).await
}

pub(super) async fn find(
    db: &Database,
    direction: TransferDirection,
    id: String,
) -> anyhow::Result<Option<Transfer>> {
    db.call(move |db| {
        db.query_row(
            &format!("SELECT {FIELDS} FROM transfers WHERE id = ?1 AND direction = ?2"),
            params![id, direction_name(direction)],
            transfer,
        )
        .optional()
    })
    .await
}

pub(super) async fn set_position(db: &Database, id: String, position: u32) -> anyhow::Result<()> {
    db.call(move |db| {
        db.execute(
            "UPDATE transfers SET place_in_queue = ?2 WHERE id = ?1",
            params![id, position],
        )
    })
    .await?;
    Ok(())
}

pub(super) async fn remove(
    db: &Database,
    dir: TransferDirection,
    id: Option<String>,
) -> anyhow::Result<()> {
    db.call(move |db| match id {
        Some(id) => db.execute("UPDATE transfers SET removed = 1 WHERE id = ?1 AND direction = ?2", params![id, direction_name(dir)]),
        None => db.execute("UPDATE transfers SET removed = 1 WHERE direction = ?1 AND NOT removed AND (state & 16) != 0", [direction_name(dir)]),
    }).await?;
    Ok(())
}

pub(super) async fn create_batch(db: &Database, batch: Batch) -> anyhow::Result<bool> {
    db.call(move |db| {
        let options = batch.options.unwrap_or_default();
        Ok(db.execute("INSERT INTO transfer_batches (id, search_id, username, created_at, destination, external_id)\n            VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO NOTHING",
            params![batch.id, batch.search_id, batch.username, timestamp(batch.created_at), options.destination, options.external_id])? != 0)
    }).await
}

pub(super) async fn batch(db: &Database, id: String) -> anyhow::Result<Option<Batch>> {
    db.call(move |db| {
        let Some(mut batch) = batch_metadata(db, &id)? else {
            return Ok(None);
        };
        let mut stmt = db.prepare(&format!(
            "SELECT {FIELDS} FROM transfers WHERE batch_id = ?1 ORDER BY rowid"
        ))?;
        batch.transfers = Some(
            stmt.query_map([id], transfer)?
                .collect::<rusqlite::Result<_>>()?,
        );
        Ok(Some(batch))
    })
    .await
}

pub(super) async fn batch_info(db: &Database, id: String) -> anyhow::Result<Option<Batch>> {
    db.call(move |db| batch_metadata(db, &id)).await
}

fn batch_metadata(db: &Connection, id: &str) -> rusqlite::Result<Option<Batch>> {
    db.query_row("SELECT id, search_id, username, created_at, destination, external_id FROM transfer_batches WHERE id = ?1", [id], |row| Ok(Batch {
        id: row.get(0)?, search_id: row.get(1)?, username: row.get(2)?,
        direction: TransferDirection::Download,
        created_at: date(row, 3)?.ok_or(rusqlite::Error::InvalidQuery)?, transfers: None,
        options: Some(BatchOptions { destination: row.get(4)?, external_id: row.get(5)? }),
    })).optional()
}

pub(super) async fn prune(db: &Database, r: &RetentionConfig) -> anyhow::Result<()> {
    let rules = [
        (
            "Download",
            [
                r.downloads_succeeded,
                r.downloads_cancelled,
                r.downloads_errored,
                r.downloads_failed,
            ],
        ),
        (
            "Upload",
            [
                r.uploads_succeeded,
                r.uploads_cancelled,
                r.uploads_errored,
                r.uploads_failed,
            ],
        ),
    ];
    db.call(move |db| {
        let tx = db.transaction()?;
        let now = Utc::now();
        for (direction, [succeeded, cancelled, errored, failed]) in rules {
            for (state, age) in [(48, succeeded), (80, cancelled), (272, errored),
                (144, failed), (528, failed), (1040, failed)] {
                let Some(cutoff) = age
                    .and_then(|age| chrono::TimeDelta::from_std(age).ok())
                    .and_then(|age| now.checked_sub_signed(age))
                else {
                    continue;
                };
                tx.execute("UPDATE transfers SET removed = 1 WHERE direction = ?1 AND state = ?2 AND NOT removed AND ended_at <= ?3",
                    params![direction, state, timestamp(cutoff)])?;
            }
        }
        tx.commit()
    }).await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::transfers::{LOCAL_QUEUE, new_transfer};

    #[tokio::test]
    async fn preserves_delayed_retries_and_recovers_interrupted_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.db");
        let db = Database::open(&path, MIGRATIONS).unwrap();
        let mut retry = new_transfer(
            TransferDirection::Download,
            "peer",
            "album\\retry".into(),
            5,
            None,
        );
        let mut live = new_transfer(
            TransferDirection::Download,
            "peer",
            "album\\live".into(),
            7,
            None,
        );
        let mut upload = new_transfer(
            TransferDirection::Upload,
            "peer",
            "album\\upload".into(),
            9,
            None,
        );
        insert(&db, vec![retry.clone(), live.clone(), upload.clone()])
            .await
            .unwrap();
        let scheduled = Utc::now() + chrono::TimeDelta::minutes(1);
        retry.attempts = 2;
        retry.next_attempt_at = Some(scheduled);
        retry.bytes_transferred = 2;
        live.attempts = 2;
        live.state = TransferStates::IN_PROGRESS;
        upload.started_at = Some(Utc::now());
        upload.state = TransferStates::IN_PROGRESS;
        save(
            &db,
            [&retry, &live, &upload]
                .into_iter()
                .map(|t| Snapshot::new(t, 0))
                .collect(),
        )
        .await
        .unwrap();
        drop(db);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let (history, pending) = initialize(&db).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].state,
            TransferStates::COMPLETED | TransferStates::ABORTED
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].next_attempt_at, Some(scheduled));
        assert_eq!(pending[0].attempts, 2);
        assert_eq!(pending[0].bytes_transferred, 2);
        assert_eq!(pending[1].state, LOCAL_QUEUE);
        assert_eq!(pending[1].attempts, 1);
        assert!(pending[1].next_attempt_at.is_some());
        assert_eq!(initialize(&db).await.unwrap().1[1].attempts, 1);
    }

    #[tokio::test]
    async fn batch_members_and_history_remain_after_soft_removal() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        let b = Batch {
            id: "batch-id".into(),
            search_id: None,
            username: "peer".into(),
            created_at: Utc::now(),
            direction: TransferDirection::Download,
            transfers: None,
            options: Some(BatchOptions {
                destination: Some("folder".into()),
                external_id: Some("record".into()),
            }),
        };
        assert!(create_batch(&db, b.clone()).await.unwrap());
        assert!(!create_batch(&db, b.clone()).await.unwrap());
        let mut t = new_transfer(
            TransferDirection::Download,
            "peer",
            "album\\file".into(),
            4,
            Some(b.id.clone()),
        );
        insert(&db, vec![t.clone()]).await.unwrap();
        t.state = TransferStates::COMPLETED | TransferStates::SUCCEEDED;
        t.bytes_transferred = t.size;
        save(&db, vec![Snapshot::new(&t, 0)]).await.unwrap();
        remove(&db, TransferDirection::Download, None)
            .await
            .unwrap();
        assert!(
            list(&db, TransferDirection::Download, None, false)
                .await
                .unwrap()
                .is_empty()
        );
        let batch = batch(&db, b.id).await.unwrap().unwrap();
        assert_eq!(batch.options, b.options);
        let transfers = batch.transfers.unwrap();
        assert_eq!(transfers[0].id, t.id);
        assert!(transfers[0].removed);
    }
}
