use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use slskr_types::{
    config::RetentionConfig,
    transfers::{Batch, Transfer, TransferDirection, TransferState},
};

use crate::db::Database;

pub(crate) const MIGRATIONS: &[&str] = &[
    r#"
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
"#,
    // Folder downloads: the remote folder the file is saved relative to.
    "ALTER TABLE transfers ADD COLUMN root TEXT;",
    "CREATE INDEX ix_transfers_recent ON transfers(direction, ended_at DESC) WHERE (state & 16) != 0;
     CREATE INDEX ix_transfers_active ON transfers(direction) WHERE (state & 16) = 0;",
    "CREATE INDEX ix_transfers_report_state_end ON transfers(state, ended_at) WHERE ended_at IS NOT NULL;
     CREATE INDEX ix_transfers_report_errors_end ON transfers(direction, ended_at)
         WHERE (state & ~48) != 0 AND ended_at IS NOT NULL;",
    include_str!("migrations/chart.sql"),
    // Lets `MIN(requested_at)` (the reports' earliest date) read one index entry.
    "CREATE INDEX ix_transfers_requested_at ON transfers(requested_at);",
    // No query filters on `state` alone (they use the partial and report indexes), but
    // every state change had to update it.
    "DROP INDEX ix_transfers_state;",
    include_str!("migrations/integer_times.sql"),
];

// `transfers.state` keeps the Soulseek transfer state bits (Requested 1, Queued 2,
// Initializing 4, InProgress 8, Completed 16, Succeeded 32, Cancelled 64, TimedOut 128,
// Errored 256, Rejected 512, Aborted 1024, Locally 2048, Remotely 4096); the queries test
// `state & 16` for completion.

/// The API state of stored (or wire) state bits.
pub(crate) fn state_from_bits(bits: u32) -> TransferState {
    if bits & 16 != 0 {
        [
            (32, TransferState::Succeeded),
            (64, TransferState::Cancelled),
            (128, TransferState::TimedOut),
            (512, TransferState::Rejected),
            (1024, TransferState::Aborted),
        ]
        .into_iter()
        .find(|(bit, _)| bits & bit != 0)
        .map_or(TransferState::Errored, |(_, state)| state)
    } else if bits & 8 != 0 {
        TransferState::InProgress
    } else if bits & 4 != 0 {
        TransferState::Initializing
    } else if bits & 2 != 0 {
        if bits & 4096 != 0 {
            TransferState::QueuedRemotely
        } else {
            TransferState::QueuedLocally
        }
    } else {
        TransferState::Requested
    }
}

/// The stored bits of a state.
pub(crate) fn state_to_bits(state: TransferState) -> u32 {
    match state {
        TransferState::Requested => 1,
        TransferState::QueuedLocally => 2 | 2048,
        TransferState::QueuedRemotely => 2 | 4096,
        TransferState::Initializing => 4,
        TransferState::InProgress => 8,
        TransferState::Succeeded => 16 | 32,
        TransferState::Cancelled => 16 | 64,
        TransferState::TimedOut => 16 | 128,
        TransferState::Errored => 16 | 256,
        TransferState::Rejected => 16 | 512,
        TransferState::Aborted => 16 | 1024,
    }
}

/// `SELECT <the columns [`transfer`] reads> FROM transfers <rest>`, as a constant for
/// `prepare_cached`.
macro_rules! select {
    ($($rest:literal),+) => {
        concat!(
            "SELECT id, batch_id, username, direction, filename, size, state, requested_at, \
             enqueued_at, started_at, ended_at, bytes_transferred, average_speed, place_in_queue, \
             exception, attempts, next_attempt_at, removed, root FROM transfers ",
            $($rest),+
        )
    };
}

// List queries per `(recent_completed, username)`. Separate statements, rather than
// `(?n IS NULL OR username = ?n)`, let SQLite use the username and partial state indexes.
const LIST: &str = select!("WHERE direction = ?1 AND (?2 OR NOT removed) ORDER BY rowid");
const LIST_USER: &str =
    select!("WHERE direction = ?1 AND (?2 OR NOT removed) AND username = ?3 ORDER BY rowid");
const LIST_RECENT: &str = concat!(
    select!("WHERE direction = ?1 AND (?2 OR NOT removed) AND (state & 16) = 0"),
    " UNION ALL SELECT * FROM (",
    select!(
        "WHERE direction = ?1 AND (?2 OR NOT removed) AND (state & 16) != 0 ",
        "ORDER BY ended_at DESC, rowid DESC LIMIT ?3"
    ),
    ")"
);
const LIST_RECENT_USER: &str = concat!(
    select!("WHERE direction = ?1 AND (?2 OR NOT removed) AND username = ?4 AND (state & 16) = 0"),
    " UNION ALL SELECT * FROM (",
    select!(
        "WHERE direction = ?1 AND (?2 OR NOT removed) AND username = ?4 AND (state & 16) != 0 ",
        "ORDER BY ended_at DESC, rowid DESC LIMIT ?3"
    ),
    ")"
);

pub(super) fn direction_name(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => "Download",
        TransferDirection::Upload => "Upload",
    }
}

/// A transfer time as stored: nanoseconds since the Unix epoch, saturated outside the
/// range `i64` covers (years 1677 to 2262).
pub(crate) fn timestamp(time: DateTime<Utc>) -> i64 {
    time.timestamp_nanos_opt()
        .unwrap_or(if time.timestamp() < 0 {
            i64::MIN
        } else {
            i64::MAX
        })
}

/// A stored transfer time.
pub(crate) fn date(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    Ok(row
        .get::<_, Option<i64>>(i)?
        .map(DateTime::from_timestamp_nanos))
}

/// Batch times stay RFC 3339 text; nothing compares them.
fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn text_date(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
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
        state: state_from_bits(row.get(6)?),
        requested_at: date(row, 7)?.ok_or(rusqlite::Error::InvalidQuery)?,
        enqueued_at: date(row, 8)?,
        started_at: date(row, 9)?,
        ended_at: date(row, 10)?,
        bytes_transferred: row.get(11)?,
        average_speed: row.get(12)?,
        place_in_queue: row.get(13)?,
        error: row.get(14)?,
        attempts: row.get(15)?,
        next_attempt_at: date(row, 16)?,
        removed: row.get(17)?,
        root: row.get(18)?,
        bytes_remaining: 0,
        elapsed_ms: None,
        percent_complete: 0.0,
        remaining_ms: None,
    };
    transfer.refresh_derived(Utc::now());
    Ok(transfer)
}

pub(super) async fn initialize(db: &Database) -> anyhow::Result<(Vec<Transfer>, Vec<Transfer>)> {
    db.call(|db| {
        let tx = db.transaction()?;
        let now = Utc::now();
        tx.execute(
            "UPDATE transfers SET state = 1040, state_description = 'aborted', ended_at = ?1,
                next_attempt_at = NULL, exception = 'OperationCanceledException: Interrupted by daemon shutdown'
                WHERE (state & 16) = 0 AND (direction = 'Upload' OR removed)",
            [timestamp(now)],
        )?;
        // An interrupted negotiation is not a failed attempt. Move it back to the
        // queue once, while preserving delays for already scheduled retries.
        tx.execute(
            "UPDATE transfers SET state = 2050, state_description = 'queued_locally', ended_at = NULL,
                attempts = CASE WHEN next_attempt_at IS NULL THEN MAX(0, attempts - 1) ELSE attempts END,
                next_attempt_at = COALESCE(next_attempt_at, ?1), place_in_queue = NULL
                WHERE direction = 'Download' AND (state & 16) = 0",
            [timestamp(now)],
        )?;
        let history = {
            let mut stmt = tx.prepare(select!("WHERE direction = 'Upload' AND started_at >= ?1"))?;
            stmt.query_map([timestamp(now - chrono::TimeDelta::days(7))], transfer)?.collect::<rusqlite::Result<_>>()?
        };
        let downloads = {
            let mut stmt = tx.prepare(select!("WHERE direction = 'Download' AND (state & 16) = 0 ORDER BY rowid"))?;
            stmt.query_map([], transfer)?.collect::<rusqlite::Result<_>>()?
        };
        tx.commit()?;
        Ok((history, downloads))
    }).await
}

/// Stores new transfers. Returns the ids of completed transfers of the same files that
/// they superseded (now removed from the list).
pub(super) async fn insert(db: &Database, transfers: Vec<Transfer>) -> anyhow::Result<Vec<String>> {
    db.call(move |db| {
        let tx = db.transaction()?;
        let mut superseded = Vec::new();
        {
            let mut supersede = tx.prepare_cached("UPDATE transfers SET removed = 1 WHERE direction = ?1 AND username = ?2
                AND filename = ?3 AND removed = 0 AND (state & 16) != 0 RETURNING id")?;
            let mut insert = tx.prepare_cached("INSERT INTO transfers (id, batch_id, username, direction, filename, size,
                state, state_description, requested_at, enqueued_at, removed, root) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)")?;
            for t in transfers {
                let direction = direction_name(t.direction);
                for id in supersede.query_map(params![direction, t.username, t.filename], |row| row.get(0))? {
                    superseded.push(id?);
                }
                insert.execute(params![t.id, t.batch_id, t.username, direction, t.filename, t.size,
                    state_to_bits(t.state), t.state.as_str(), timestamp(t.requested_at), t.enqueued_at.map(timestamp), t.removed, t.root])?;
            }
        }
        tx.commit()?;
        Ok(superseded)
    }).await
}

pub(super) struct Snapshot {
    id: String,
    size: u64,
    offset: u64,
    state: TransferState,
    enqueued: Option<DateTime<Utc>>,
    started: Option<DateTime<Utc>>,
    ended: Option<DateTime<Utc>>,
    bytes: u64,
    speed: f64,
    error: Option<String>,
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
            error: t.error.clone(),
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
            let mut stmt = tx.prepare_cached("UPDATE transfers SET size = ?2, start_offset = ?3, state = ?4, enqueued_at = ?5,
                started_at = ?6, ended_at = ?7, bytes_transferred = ?8, average_speed = ?9, exception = ?10,
                attempts = ?11, next_attempt_at = ?12, removed = MAX(removed, ?13), state_description = ?14, place_in_queue = ?15
                WHERE id = ?1 AND (state & 16) = 0 AND attempts <= ?11")?;
            for t in snapshots {
                stmt.execute(params![t.id, t.size, t.offset, state_to_bits(t.state), t.enqueued.map(timestamp),
                    t.started.map(timestamp), t.ended.map(timestamp), t.bytes, t.speed,
                    t.error, t.attempts, t.next_attempt.map(timestamp), t.removed, t.state.as_str(), t.position])?;
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
    recent_completed: Option<u32>,
) -> anyhow::Result<Vec<Transfer>> {
    db.call(move |db| {
        let direction = direction_name(direction);
        match (recent_completed, username) {
            (None, None) => db
                .prepare_cached(LIST)?
                .query_map(params![direction, include_removed], transfer)?
                .collect(),
            (None, Some(username)) => db
                .prepare_cached(LIST_USER)?
                .query_map(params![direction, include_removed, username], transfer)?
                .collect(),
            (Some(limit), None) => db
                .prepare_cached(LIST_RECENT)?
                .query_map(params![direction, include_removed, limit], transfer)?
                .collect(),
            (Some(limit), Some(username)) => db
                .prepare_cached(LIST_RECENT_USER)?
                .query_map(
                    params![direction, include_removed, limit, username],
                    transfer,
                )?
                .collect(),
        }
    })
    .await
}

pub(super) async fn find(
    db: &Database,
    direction: TransferDirection,
    id: String,
) -> anyhow::Result<Option<Transfer>> {
    db.call(move |db| {
        db.prepare_cached(select!("WHERE id = ?1 AND direction = ?2"))?
            .query_row(params![id, direction_name(direction)], transfer)
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

/// Removes one transfer, or every completed one, from the list. Returns the ids removed.
pub(super) async fn remove(
    db: &Database,
    dir: TransferDirection,
    id: Option<String>,
) -> anyhow::Result<Vec<String>> {
    db.call(move |db| {
        let id_of = |row: &Row<'_>| row.get(0);
        match id {
            Some(id) => db
                .prepare_cached("UPDATE transfers SET removed = 1 WHERE id = ?1 AND direction = ?2 RETURNING id")?
                .query_map(params![id, direction_name(dir)], id_of)?
                .collect(),
            None => db
                .prepare_cached("UPDATE transfers SET removed = 1 WHERE direction = ?1 AND NOT removed AND (state & 16) != 0 RETURNING id")?
                .query_map([direction_name(dir)], id_of)?
                .collect(),
        }
    })
    .await
}

pub(super) async fn create_batch(db: &Database, batch: Batch) -> anyhow::Result<bool> {
    db.call(move |db| {
        Ok(db.execute("INSERT INTO transfer_batches (id, search_id, username, created_at, destination, external_id)\n            VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO NOTHING",
            params![batch.id, batch.search_id, batch.username, rfc3339(batch.created_at), batch.destination, batch.external_id])? != 0)
    }).await
}

pub(super) async fn batch(db: &Database, id: String) -> anyhow::Result<Option<Batch>> {
    db.call(move |db| {
        let Some(mut batch) = batch_metadata(db, &id)? else {
            return Ok(None);
        };
        let mut stmt = db.prepare_cached(select!("WHERE batch_id = ?1 ORDER BY rowid"))?;
        batch.transfers = stmt
            .query_map([id], transfer)?
            .collect::<rusqlite::Result<_>>()?;
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
        created_at: text_date(row, 3)?.ok_or(rusqlite::Error::InvalidQuery)?, transfers: Vec::new(),
        destination: row.get(4)?, external_id: row.get(5)?,
    })).optional()
}

/// Removes completed transfers older than the retention rules allow from the list.
/// Returns the ids removed.
pub(super) async fn prune(db: &Database, r: &RetentionConfig) -> anyhow::Result<Vec<String>> {
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
        let mut removed = Vec::new();
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE transfers SET removed = 1 WHERE direction = ?1 AND state = ?2
                AND NOT removed AND ended_at <= ?3 RETURNING id",
            )?;
            for (direction, [succeeded, cancelled, errored, failed]) in rules {
                for (state, age) in [
                    (48, succeeded),
                    (80, cancelled),
                    (272, errored),
                    (144, failed),
                    (528, failed),
                    (1040, failed),
                ] {
                    let Some(cutoff) = age
                        .and_then(|age| chrono::TimeDelta::from_std(age).ok())
                        .and_then(|age| now.checked_sub_signed(age))
                    else {
                        continue;
                    };
                    for id in stmt
                        .query_map(params![direction, state, timestamp(cutoff)], |row| {
                            row.get(0)
                        })?
                    {
                        removed.push(id?);
                    }
                }
            }
        }
        tx.commit()?;
        Ok(removed)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::transfers::new_transfer;

    #[tokio::test]
    async fn text_times_become_nanoseconds_keeping_rows_order_and_chart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.db");
        let integer_times = MIGRATIONS
            .iter()
            .position(|sql| *sql == include_str!("migrations/integer_times.sql"))
            .unwrap();
        drop(Database::open(&path, &MIGRATIONS[..integer_times]).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "INSERT INTO transfers (rowid, id, username, direction, filename, size, state,
                     requested_at, enqueued_at, started_at, ended_at, next_attempt_at) VALUES
                 (7, 'b', 'peer', 'Download', 'b', 5, 48, '2026-01-01T00:00:00Z',
                  '2026-01-01T00:00:01.5Z', '2026-01-01T01:00:02.000000001+01:00',
                  '2026-01-01T00:59:59.999999999Z', NULL),
                 (3, 'a', 'peer', 'Download', 'a', 9, 2050, '2026-01-01T00:00:00.123Z',
                  NULL, NULL, NULL, '2026-01-02T00:00:00.000000000Z');",
            )
            .unwrap();
        let chart = |connection: &Connection| {
            connection
                .query_row(
                    "SELECT group_concat(hour || ':' || bytes || ':' || files) FROM transfer_chart_hours",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        let before = chart(&connection);
        drop(connection);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let at = |text: &str| timestamp(text.parse().unwrap());
        // rowid, id, then requested, enqueued, started, ended and next attempt.
        type Row = (
            i64,
            String,
            i64,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        );
        let rows: Vec<Row> = db
            .call(|db| {
                db.prepare("SELECT rowid, id, requested_at, enqueued_at, started_at, ended_at, next_attempt_at FROM transfers ORDER BY rowid")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?
                    .collect()
            })
            .await
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    3,
                    "a".into(),
                    at("2026-01-01T00:00:00.123Z"),
                    None,
                    None,
                    None,
                    Some(at("2026-01-02T00:00:00Z"))
                ),
                (
                    7,
                    "b".into(),
                    at("2026-01-01T00:00:00Z"),
                    Some(at("2026-01-01T00:00:01.5Z")),
                    Some(at("2026-01-01T00:00:02.000000001Z")),
                    Some(at("2026-01-01T00:59:59.999999999Z")),
                    None,
                ),
            ]
        );
        let after = db.call(move |db| Ok(chart(db))).await.unwrap();
        assert_eq!(after, before, "chart hours are kept");

        // The recreated triggers keep the chart in step with later changes.
        let found = find(&db, TransferDirection::Download, "b".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.ended_at,
            Some("2026-01-01T00:59:59.999999999Z".parse().unwrap())
        );
        db.call(|db| db.execute("DELETE FROM transfers WHERE id = 'b'", []))
            .await
            .unwrap();
        assert_eq!(
            db.call(
                |db| db.query_row("SELECT count(*) FROM transfer_chart_hours", [], |row| row
                    .get::<_, i64>(
                    0
                ))
            )
            .await
            .unwrap(),
            0
        );
    }

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
        live.state = TransferState::InProgress;
        upload.started_at = Some(Utc::now());
        upload.state = TransferState::InProgress;
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
        assert_eq!(history[0].state, TransferState::Aborted);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].next_attempt_at, Some(scheduled));
        assert_eq!(pending[0].attempts, 2);
        assert_eq!(pending[0].bytes_transferred, 2);
        assert_eq!(pending[1].state, TransferState::QueuedLocally);
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
            transfers: Vec::new(),
            destination: Some("folder".into()),
            external_id: Some("record".into()),
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
        t.state = TransferState::Succeeded;
        t.bytes_transferred = t.size;
        save(&db, vec![Snapshot::new(&t, 0)]).await.unwrap();
        remove(&db, TransferDirection::Download, None)
            .await
            .unwrap();
        assert!(
            list(&db, TransferDirection::Download, None, false, None)
                .await
                .unwrap()
                .is_empty()
        );
        let batch = batch(&db, b.id).await.unwrap().unwrap();
        assert_eq!(batch.destination, b.destination);
        assert_eq!(batch.external_id, b.external_id);
        let transfers = batch.transfers;
        assert_eq!(transfers[0].id, t.id);
        assert!(transfers[0].removed);
    }

    #[tokio::test]
    async fn folder_root_survives_restart_and_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.db");
        // A database written before folder downloads gains the column empty.
        let db = Database::open(&path, &MIGRATIONS[..1]).unwrap();
        db.call(|db| {
            db.execute(
                "INSERT INTO transfers (id, username, direction, filename, size, state, requested_at)
                    VALUES ('old', 'peer', 'Download', 'a\\b', 1, 2050, '2026-01-01T00:00:00Z')",
                [],
            )
        })
        .await
        .unwrap();
        drop(db);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let mut t = new_transfer(
            TransferDirection::Download,
            "peer",
            r"@@music\Album\CD1\01.flac".into(),
            5,
            None,
        );
        t.root = Some(r"@@music\Album".into());
        insert(&db, vec![t.clone()]).await.unwrap();
        t.attempts = 1;
        t.state = TransferState::InProgress;
        save(&db, vec![Snapshot::new(&t, 0)]).await.unwrap();
        drop(db);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let (_, pending) = initialize(&db).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, "old");
        assert_eq!(pending[0].root, None);
        let recovered = &pending[1];
        assert_eq!(recovered.root, t.root);
        let cfg = slskr_types::config::Config::default();
        let layout = super::super::destination::derive(&cfg, &t, None, None);
        assert_eq!(layout, Path::new("Album/CD1"));
        assert_eq!(
            super::super::destination::derive(&cfg, recovered, None, None),
            layout
        );
        let found = find(&db, TransferDirection::Download, t.id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.root, t.root);
    }

    #[tokio::test]
    async fn lists_filter_by_user_and_removals_report_their_ids() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        let upload = |user: &str, file: &str| {
            new_transfer(TransferDirection::Upload, user, file.into(), 1, None)
        };
        let mut done = upload("alice", "a\\1");
        let live = upload("alice", "a\\2");
        let other = upload("bob", "b\\1");
        assert!(
            insert(&db, vec![done.clone(), live.clone(), other.clone()])
                .await
                .unwrap()
                .is_empty()
        );
        done.state = TransferState::Succeeded;
        done.ended_at = Some(Utc::now());
        save(&db, vec![Snapshot::new(&done, 0)]).await.unwrap();

        let ids = |list: Vec<Transfer>| list.into_iter().map(|t| t.id).collect::<Vec<_>>();
        let list = |user: Option<&str>, recent: Option<u32>| {
            list(
                &db,
                TransferDirection::Upload,
                user.map(Into::into),
                false,
                recent,
            )
        };
        assert_eq!(ids(list(None, None).await.unwrap()).len(), 3);
        assert_eq!(
            ids(list(Some("alice"), None).await.unwrap()),
            vec![done.id.clone(), live.id.clone()]
        );
        assert_eq!(
            ids(list(Some("alice"), Some(0)).await.unwrap()),
            vec![live.id.clone()]
        );
        assert_eq!(
            ids(list(Some("alice"), Some(5)).await.unwrap()),
            vec![live.id.clone(), done.id.clone()]
        );
        assert_eq!(ids(list(None, Some(0)).await.unwrap()).len(), 2);
        assert!(list(Some("carol"), None).await.unwrap().is_empty());

        // Requesting the completed file again supersedes the finished transfer.
        let again = upload("alice", "a\\1");
        assert_eq!(
            insert(&db, vec![again.clone()]).await.unwrap(),
            vec![done.id.clone()]
        );
        assert_eq!(
            remove(&db, TransferDirection::Upload, Some(other.id.clone()))
                .await
                .unwrap(),
            vec![other.id.clone()]
        );
        assert!(
            remove(&db, TransferDirection::Upload, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(list(None, None).await.unwrap()),
            vec![live.id.clone(), again.id]
        );
    }

    #[test]
    fn state_bits_round_trip() {
        for state in TransferState::ALL {
            assert_eq!(state_from_bits(state_to_bits(state)), state);
        }
        assert_eq!(state_from_bits(16), TransferState::Errored);
        assert_eq!(state_from_bits(2 | 4096), TransferState::QueuedRemotely);
    }
}
