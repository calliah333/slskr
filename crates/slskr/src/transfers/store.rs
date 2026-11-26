use std::{borrow::Cow, sync::LazyLock, time::Duration};

use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{
    Connection, OptionalExtension, Row, params,
    types::{Value, ValueRef},
};
use slskr_types::{
    config::RetentionConfig,
    transfers::{
        Batch, CompletedFilter, Transfer, TransferDirection, TransferState, TransferStateCount,
    },
};
use uuid::Uuid;

use crate::db::Database;

/// Rows removed from the list are deleted this long after they ended. Their totals stay
/// in the report rollups.
pub(super) const PURGE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);

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
    // The only lookup by file (superseding a listed, completed transfer when the file is
    // requested again) wants listed rows only. The full index also held every removed
    // row, so a peer that re-requested one file thousands of times made each request scan
    // thousands of entries; it was also a fifth of a large database.
    "DROP INDEX ix_transfers_file;
     CREATE INDEX ix_transfers_listed_file ON transfers(direction, username, filename)
         WHERE removed = 0 AND (state & 16) != 0;",
    include_str!("migrations/report_hours.sql"),
    include_str!("migrations/compact.sql"),
    include_str!("migrations/error_messages.sql"),
    include_str!("migrations/offline_errors.sql"),
    include_str!("migrations/list_counts.sql"),
];

/// `error` with the IPv4 peer addresses in it (`a.b.c.d` or `a.b.c.d:port`) replaced by
/// `<address>`, so the same failure from different peers is stored once and counted
/// together in the reports.
pub(crate) fn mask_addresses(error: &str) -> Cow<'_, str> {
    static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?\b").expect("valid address pattern")
    });
    ADDRESS.replace_all(error, "<address>")
}

/// A transfer id as stored: the 16 bytes of a canonical (lowercase, hyphenated) UUID,
/// else the text as given.
pub(crate) fn key(id: &str) -> Value {
    match Uuid::try_parse(id) {
        Ok(uuid) if uuid.hyphenated().to_string() == id => Value::Blob(uuid.as_bytes().to_vec()),
        _ => Value::Text(id.to_owned()),
    }
}

/// A stored transfer id (see [`key`]).
pub(crate) fn id(row: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    match row.get_ref(i)? {
        ValueRef::Blob(bytes) => Uuid::from_slice(bytes)
            .map(|uuid| uuid.hyphenated().to_string())
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    i,
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            }),
        _ => row.get(i),
    }
}

/// The id of a stored text, inserting it first if it is new.
fn intern(
    db: &Connection,
    select: &str,
    insert: &str,
    values: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<i64> {
    if let Some(id) = db
        .prepare_cached(select)?
        .query_row(values, |row| row.get(0))
        .optional()?
    {
        return Ok(id);
    }
    db.prepare_cached(insert)?.execute(values)?;
    Ok(db.last_insert_rowid())
}

/// The `transfer_files` id of `filename`.
fn file_id(db: &Connection, filename: &str) -> rusqlite::Result<i64> {
    // Split after the last separator, as the migration does.
    let (path, name) = filename.split_at(filename.rfind(['/', '\\']).map_or(0, |i| i + 1));
    let directory = intern(
        db,
        "SELECT directory_id FROM transfer_directories WHERE path = ?1",
        "INSERT INTO transfer_directories (path) VALUES (?1)",
        &[&path],
    )?;
    intern(
        db,
        "SELECT file_id FROM transfer_files WHERE directory_id = ?1 AND name = ?2",
        "INSERT INTO transfer_files (directory_id, name) VALUES (?1, ?2)",
        &[&directory, &name],
    )
}

/// The `transfer_errors` id of an error, masked.
fn error_id(db: &Connection, error: Option<&str>) -> rusqlite::Result<Option<i64>> {
    error
        .map(|error| {
            intern(
                db,
                "SELECT error_id FROM transfer_errors WHERE exception = ?1",
                "INSERT INTO transfer_errors (exception) VALUES (?1)",
                &[&mask_addresses(error)],
            )
        })
        .transpose()
}

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

/// `SELECT <the columns [`transfer`] reads> FROM transfer_details <rest>`, as a constant
/// for `prepare_cached`. `seq` is the row's `rowid`.
macro_rules! select {
    ($($rest:literal),+) => {
        concat!(
            "SELECT id, batch_id, username, direction, filename, size, state, requested_at, \
             enqueued_at, started_at, ended_at, bytes_transferred, average_speed, place_in_queue, \
             exception, attempts, next_attempt_at, removed, root FROM transfer_details ",
            $($rest),+
        )
    };
}

/// Which transfers a list returns: every active one, and the completed ones `completed`
/// selects (all of them in list order, or a page of the newest with `recent_completed`).
#[derive(Clone, Debug, Default)]
pub(crate) struct ListFilter {
    pub(crate) username: Option<String>,
    pub(crate) include_removed: bool,
    pub(crate) recent_completed: Option<u32>,
    pub(crate) completed_offset: u32,
    pub(crate) completed: CompletedFilter,
    /// Lower-case text the user name or the file name contains.
    pub(crate) query: Option<String>,
}

impl ListFilter {
    /// The `WHERE` clause every part of the list shares, with its parameters.
    fn base(&self, direction: TransferDirection) -> (String, Vec<(&'static str, Value)>) {
        let mut sql = String::from("WHERE direction = :direction AND (:removed OR NOT removed)");
        let mut params = vec![
            (":direction", Value::Text(direction_name(direction).into())),
            (":removed", Value::Integer(self.include_removed.into())),
        ];
        if let Some(username) = &self.username {
            sql.push_str(" AND username = :username");
            params.push((":username", Value::Text(username.clone())));
        }
        if let Some(query) = &self.query {
            sql.push_str(
                " AND (instr(lower(username), :query) > 0 OR instr(lower(filename), :query) > 0)",
            );
            params.push((":query", Value::Text(query.clone())));
        }
        (sql, params)
    }

    /// The list query. The completed part keeps `(state & 16) != 0` so that SQLite can
    /// page through the partial `ix_transfers_recent` index.
    fn sql(&self, direction: TransferDirection) -> (String, Vec<(&'static str, Value)>) {
        let (base, mut params) = self.base(direction);
        let columns = select!("");
        let completed = match self.completed {
            CompletedFilter::All => Some("(state & 16) != 0"),
            CompletedFilter::Succeeded => Some("(state & 16) != 0 AND (state & 32) != 0"),
            CompletedFilter::Failed => Some("(state & 16) != 0 AND (state & 32) = 0"),
            CompletedFilter::None => None,
        };
        let sql = match (self.recent_completed, completed) {
            (_, None) => format!("{columns}{base} AND (state & 16) = 0 ORDER BY seq"),
            (None, Some(completed)) => {
                format!("{columns}{base} AND ((state & 16) = 0 OR ({completed})) ORDER BY seq")
            }
            (Some(limit), Some(completed)) => {
                params.push((":limit", Value::Integer(limit.into())));
                params.push((":offset", Value::Integer(self.completed_offset.into())));
                format!(
                    "{columns}{base} AND (state & 16) = 0 UNION ALL SELECT * FROM \
                     ({columns}{base} AND {completed} \
                     ORDER BY ended_at DESC, seq DESC LIMIT :limit OFFSET :offset)"
                )
            }
        };
        (sql, params)
    }
}

/// Binds named parameters given as owned values.
fn named<'a>(params: &'a [(&'static str, Value)]) -> Vec<(&'static str, &'a dyn rusqlite::ToSql)> {
    params
        .iter()
        .map(|(name, value)| (*name, value as &dyn rusqlite::ToSql))
        .collect()
}

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
        id: id(row, 0)?,
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

/// An upload started in the last week, for the upload limits.
pub(super) struct Started {
    pub username: String,
    pub started_at: DateTime<Utc>,
    pub size: u64,
    pub succeeded: bool,
}

/// Unfinished transfers found in the store at startup, already moved back to the queue.
pub(super) struct Recovered {
    /// Uploads started in the last week, for the upload limits.
    pub history: Vec<Started>,
    pub downloads: Vec<Transfer>,
    /// Uploads that were queued or running, in the order they were queued.
    pub uploads: Vec<Transfer>,
}

pub(super) async fn initialize(db: &Database) -> anyhow::Result<Recovered> {
    db.call(|db| {
        let tx = db.transaction()?;
        let now = Utc::now();
        tx.execute(
            "UPDATE transfers SET state = 1040, ended_at = ?1,
                next_attempt_at = NULL, error_id = ?2
                WHERE (state & 16) = 0 AND removed",
            params![
                timestamp(now),
                error_id(&tx, Some("Interrupted by daemon shutdown"))?
            ],
        )?;
        // The peer still wants an upload that a restart interrupted: queue it again in
        // its old place. Sending resumes from whatever offset the peer then asks for.
        tx.execute(
            "UPDATE transfers SET state = 2050, ended_at = NULL,
                started_at = NULL, next_attempt_at = NULL, place_in_queue = NULL, error_id = NULL
                WHERE direction = 'Upload' AND (state & 16) = 0",
            [],
        )?;
        // An interrupted negotiation is not a failed attempt. Move it back to the
        // queue once, while preserving delays for already scheduled retries.
        tx.execute(
            "UPDATE transfers SET state = 2050, ended_at = NULL,
                attempts = CASE WHEN next_attempt_at IS NULL THEN MAX(0, attempts - 1) ELSE attempts END,
                next_attempt_at = COALESCE(next_attempt_at, ?1), place_in_queue = NULL
                WHERE direction = 'Download' AND (state & 16) = 0",
            [timestamp(now)],
        )?;
        let history = {
            let mut stmt = tx.prepare("SELECT username, started_at, size, state FROM transfers
                WHERE direction = 'Upload' AND started_at >= ?1")?;
            stmt.query_map([timestamp(now - chrono::TimeDelta::days(7))], |row| Ok(Started {
                username: row.get(0)?,
                started_at: date(row, 1)?.ok_or(rusqlite::Error::InvalidQuery)?,
                size: row.get(2)?,
                succeeded: state_from_bits(row.get(3)?) == TransferState::Succeeded,
            }))?.collect::<rusqlite::Result<_>>()?
        };
        let unfinished = |direction: &str| -> rusqlite::Result<Vec<Transfer>> {
            let mut stmt = tx.prepare(select!("WHERE direction = ?1 AND (state & 16) = 0 ORDER BY seq"))?;
            stmt.query_map([direction], transfer)?.collect()
        };
        let downloads = unfinished("Download")?;
        let uploads = unfinished("Upload")?;
        tx.commit()?;
        Ok(Recovered { history, downloads, uploads })
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
                AND file_id = ?3 AND removed = 0 AND (state & 16) != 0 RETURNING id")?;
            let mut insert = tx.prepare_cached("INSERT INTO transfers (id, batch_id, username, direction, file_id, size,
                state, requested_at, enqueued_at, removed, root) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)")?;
            for t in transfers {
                let direction = direction_name(t.direction);
                let file = file_id(&tx, &t.filename)?;
                for id in supersede.query_map(params![direction, t.username, file], |row| id(row, 0))? {
                    superseded.push(id?);
                }
                insert.execute(params![key(&t.id), t.batch_id, t.username, direction, file, t.size,
                    state_to_bits(t.state), timestamp(t.requested_at), t.enqueued_at.map(timestamp), t.removed, t.root])?;
            }
        }
        tx.commit()?;
        Ok(superseded)
    }).await
}

pub(super) struct Snapshot {
    id: String,
    size: u64,
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
    pub(super) fn new(t: &Transfer) -> Self {
        Self {
            id: t.id.clone(),
            size: t.size,
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
            let mut stmt = tx.prepare_cached("UPDATE transfers SET size = ?2, state = ?3, enqueued_at = ?4,
                started_at = ?5, ended_at = ?6, bytes_transferred = ?7, average_speed = ?8, error_id = ?9,
                attempts = ?10, next_attempt_at = ?11, removed = MAX(removed, ?12), place_in_queue = ?13
                WHERE id = ?1 AND (state & 16) = 0 AND attempts <= ?10")?;
            for t in snapshots {
                let error = error_id(&tx, t.error.as_deref())?;
                stmt.execute(params![key(&t.id), t.size, state_to_bits(t.state), t.enqueued.map(timestamp),
                    t.started.map(timestamp), t.ended.map(timestamp), t.bytes, t.speed.max(0.0),
                    error, t.attempts, t.next_attempt.map(timestamp), t.removed, t.position])?;
            }
        }
        tx.commit()
    }).await
}

pub(super) async fn list(
    db: &Database,
    direction: TransferDirection,
    filter: ListFilter,
) -> anyhow::Result<Vec<Transfer>> {
    db.call(move |db| {
        let (sql, params) = filter.sql(direction);
        db.prepare_cached(&sql)?
            .query_map(named(&params).as_slice(), transfer)?
            .collect()
    })
    .await
}

/// How many transfers the list holds per state, and their size. Without a user or text
/// filter this reads the counts the triggers keep (`migrations/list_counts.sql`).
pub(super) async fn summary(
    db: &Database,
    direction: TransferDirection,
    filter: ListFilter,
) -> anyhow::Result<Vec<TransferStateCount>> {
    db.call(move |db| {
        let rows: Vec<(u32, u64, u64)> = if filter.username.is_none() && filter.query.is_none() {
            db.prepare_cached(
                "SELECT state, SUM(files), SUM(bytes) FROM transfer_list_counts
                 WHERE direction = ?1 AND (?2 OR removed = 0) GROUP BY state",
            )?
            .query_map(
                params![direction_name(direction), filter.include_removed],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<rusqlite::Result<_>>()?
        } else {
            let (base, params) = filter.base(direction);
            db.prepare_cached(&format!(
                "SELECT state, COUNT(*), SUM(size) FROM transfer_details {base} GROUP BY state"
            ))?
            .query_map(named(&params).as_slice(), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        // Several stored bit patterns can mean one state.
        let mut states = std::collections::BTreeMap::<TransferState, (u64, u64)>::new();
        for (bits, files, bytes) in rows.into_iter().filter(|(_, files, _)| *files > 0) {
            let total = states.entry(state_from_bits(bits)).or_default();
            total.0 += files;
            total.1 += bytes;
        }
        Ok(states
            .into_iter()
            .map(|(state, (files, bytes))| TransferStateCount {
                state,
                files,
                bytes,
            })
            .collect())
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
            .query_row(params![key(&id), direction_name(direction)], transfer)
            .optional()
    })
    .await
}

pub(super) async fn set_position(db: &Database, id: String, position: u32) -> anyhow::Result<()> {
    db.call(move |db| {
        db.execute(
            "UPDATE transfers SET place_in_queue = ?2 WHERE id = ?1",
            params![key(&id), position],
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
        let id_of = |row: &Row<'_>| self::id(row, 0);
        match id {
            Some(id) => db
                .prepare_cached("UPDATE transfers SET removed = 1 WHERE id = ?1 AND direction = ?2 RETURNING id")?
                .query_map(params![key(&id), direction_name(dir)], id_of)?
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
        let mut stmt = db.prepare_cached(select!("WHERE batch_id = ?1 ORDER BY seq"))?;
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
                            self::id(row, 0)
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

/// Deletes rows removed from the list more than [`PURGE_AFTER`] ago, keeping their totals
/// in the report rollups (the delete triggers skip rows marked `removed = 2`). Returns
/// how many were deleted.
pub(super) async fn purge(db: &Database) -> anyhow::Result<usize> {
    let cutoff = Utc::now() - PURGE_AFTER;
    db.call(move |db| {
        let tx = db.transaction()?;
        tx.prepare_cached("UPDATE transfers SET removed = 2 WHERE removed AND ended_at < ?1")?
            .execute([timestamp(cutoff)])?;
        // `removed AND` lets the planner use `ix_transfers_removed` (it does not infer
        // `removed` from `removed = 2`); without it, every minute scans the whole table.
        let deleted = tx
            .prepare_cached("DELETE FROM transfers WHERE removed AND removed = 2")?
            .execute([])?;
        tx.commit()?;
        Ok(deleted)
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
                .map(Snapshot::new)
                .collect(),
        )
        .await
        .unwrap();
        drop(db);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let Recovered {
            history,
            downloads: pending,
            uploads,
        } = initialize(&db).await.unwrap();
        // The interrupted upload is queued again rather than counted as a failure.
        assert!(history.is_empty());
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].id, upload.id);
        assert_eq!(uploads[0].state, TransferState::QueuedLocally);
        assert_eq!(uploads[0].started_at, None);
        assert_eq!(uploads[0].ended_at, None);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].next_attempt_at, Some(scheduled));
        assert_eq!(pending[0].attempts, 2);
        assert_eq!(pending[0].bytes_transferred, 2);
        assert_eq!(pending[1].state, TransferState::QueuedLocally);
        assert_eq!(pending[1].attempts, 1);
        assert!(pending[1].next_attempt_at.is_some());
        assert_eq!(initialize(&db).await.unwrap().downloads[1].attempts, 1);
    }

    #[tokio::test]
    async fn requeues_uploads_in_order_and_aborts_removed_ones() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        let upload = |name: &str| {
            new_transfer(
                TransferDirection::Upload,
                "peer",
                format!("album\\{name}"),
                9,
                None,
            )
        };
        let (first, second, removed, done) =
            (upload("one"), upload("two"), upload("gone"), upload("done"));
        insert(
            &db,
            vec![first.clone(), second.clone(), removed.clone(), done.clone()],
        )
        .await
        .unwrap();
        db.call({
            let (removed, done) = (removed.id.clone(), done.id.clone());
            move |db| {
                db.execute(
                    "UPDATE transfers SET removed = 1 WHERE id = ?1",
                    [key(&removed)],
                )?;
                db.execute(
                    "UPDATE transfers SET state = 48, started_at = ?2 WHERE id = ?1",
                    params![key(&done), timestamp(Utc::now())],
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

        let recovered = initialize(&db).await.unwrap();
        let ids: Vec<_> = recovered.uploads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, [first.id.as_str(), second.id.as_str()]);
        assert_eq!(recovered.history.len(), 1);
        assert_eq!(
            (
                recovered.history[0].username.as_str(),
                recovered.history[0].size
            ),
            ("peer", 9)
        );
        assert!(recovered.history[0].succeeded);
        let gone = find(&db, TransferDirection::Upload, removed.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gone.state, TransferState::Aborted);
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
        save(&db, vec![Snapshot::new(&t)]).await.unwrap();
        remove(&db, TransferDirection::Download, None)
            .await
            .unwrap();
        assert!(
            list(&db, TransferDirection::Download, ListFilter::default())
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
        save(&db, vec![Snapshot::new(&t)]).await.unwrap();
        drop(db);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let pending = initialize(&db).await.unwrap().downloads;
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
    async fn completed_transfers_page_and_filter_and_the_summary_counts_them_all() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        let start = Utc::now();
        let mut rows = Vec::new();
        for i in 0..6_i64 {
            let mut t = new_transfer(
                TransferDirection::Upload,
                if i % 2 == 0 { "alice" } else { "Bob" },
                format!("music\\Album {i}\\track.flac"),
                10,
                None,
            );
            insert(&db, vec![t.clone()]).await.unwrap();
            if i < 5 {
                t.state = if i == 4 {
                    TransferState::Errored
                } else {
                    TransferState::Succeeded
                };
                t.ended_at = Some(start + chrono::Duration::seconds(i));
                save(&db, vec![Snapshot::new(&t)]).await.unwrap();
            }
            rows.push(t);
        }
        let names = |list: Vec<Transfer>| {
            list.into_iter()
                .map(|t| t.filename[12..13].to_owned())
                .collect::<String>()
        };
        let page = |offset, completed, query: Option<&str>| {
            list(
                &db,
                TransferDirection::Upload,
                ListFilter {
                    recent_completed: Some(2),
                    completed_offset: offset,
                    completed,
                    query: query.map(Into::into),
                    ..ListFilter::default()
                },
            )
        };
        // The active one, then two completed ones per page, newest first.
        assert_eq!(
            names(page(0, CompletedFilter::All, None).await.unwrap()),
            "543"
        );
        assert_eq!(
            names(page(2, CompletedFilter::All, None).await.unwrap()),
            "521"
        );
        assert_eq!(
            names(page(4, CompletedFilter::All, None).await.unwrap()),
            "50"
        );
        assert_eq!(
            names(page(0, CompletedFilter::Succeeded, None).await.unwrap()),
            "532"
        );
        assert_eq!(
            names(page(0, CompletedFilter::Failed, None).await.unwrap()),
            "54"
        );
        assert_eq!(
            names(page(0, CompletedFilter::None, None).await.unwrap()),
            "5"
        );
        // Text matches the user or the file name, either case (the API lowers it).
        assert_eq!(
            names(page(0, CompletedFilter::All, Some("bob")).await.unwrap()),
            "531"
        );
        assert_eq!(
            names(
                page(0, CompletedFilter::All, Some("album 2"))
                    .await
                    .unwrap()
            ),
            "2"
        );

        let summary = |query: Option<&str>, include_removed| {
            summary(
                &db,
                TransferDirection::Upload,
                ListFilter {
                    include_removed,
                    query: query.map(Into::into),
                    ..ListFilter::default()
                },
            )
        };
        let count = |state, files| TransferStateCount {
            state,
            files,
            bytes: files * 10,
        };
        assert_eq!(
            summary(None, false).await.unwrap(),
            [
                count(TransferState::QueuedLocally, 1),
                count(TransferState::Succeeded, 4),
                count(TransferState::Errored, 1)
            ]
        );
        assert_eq!(
            summary(Some("bob"), false).await.unwrap(),
            [
                count(TransferState::QueuedLocally, 1),
                count(TransferState::Succeeded, 2)
            ]
        );
        // Removing and deleting rows keeps the counts in step.
        remove(&db, TransferDirection::Upload, None).await.unwrap();
        assert_eq!(
            summary(None, false).await.unwrap(),
            [count(TransferState::QueuedLocally, 1)]
        );
        assert_eq!(summary(None, true).await.unwrap().len(), 3);
        db.call(|db| db.execute("DELETE FROM transfers", []))
            .await
            .unwrap();
        assert!(summary(None, true).await.unwrap().is_empty());
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
        save(&db, vec![Snapshot::new(&done)]).await.unwrap();

        let ids = |list: Vec<Transfer>| list.into_iter().map(|t| t.id).collect::<Vec<_>>();
        let list = |user: Option<&str>, recent: Option<u32>| {
            list(
                &db,
                TransferDirection::Upload,
                ListFilter {
                    username: user.map(Into::into),
                    recent_completed: recent,
                    ..ListFilter::default()
                },
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
    fn addresses_are_masked_and_ids_round_trip() {
        assert_eq!(
            mask_addresses("Failed to read 8 bytes from 83.21.240.32:50300: closed"),
            "Failed to read 8 bytes from <address>: closed"
        );
        assert_eq!(mask_addresses("to peer (10.0.0.1)"), "to peer (<address>)");
        assert_eq!(
            mask_addresses("Inactivity timeout of 60000 ms"),
            "Inactivity timeout of 60000 ms"
        );

        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE ids (id BLOB)").unwrap();
        let uuid = Uuid::new_v4().hyphenated().to_string();
        for (given, stored_type) in [
            (uuid.as_str(), "blob"),
            ("legacy-id", "text"),
            // Not the canonical spelling, so kept as given.
            (&*uuid.to_uppercase(), "text"),
        ] {
            db.execute("DELETE FROM ids", []).unwrap();
            db.execute("INSERT INTO ids VALUES (?1)", [key(given)])
                .unwrap();
            let (read, kind): (String, String) = db
                .query_row("SELECT id, typeof(id) FROM ids", [], |row| {
                    Ok((id(row, 0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!((read.as_str(), kind.as_str()), (given, stored_type));
        }
    }

    #[tokio::test]
    async fn purge_deletes_old_removed_rows_but_keeps_their_report_hours() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        let rows = ["old-removed", "old-listed", "new-removed"].map(|file| {
            new_transfer(
                TransferDirection::Upload,
                "peer",
                format!("music\\{file}"),
                10,
                None,
            )
        });
        insert(&db, rows.to_vec()).await.unwrap();
        let ended = rows.clone().map(|mut t| {
            let age_days = if t.filename.contains("old") { 40 } else { 1 };
            t.state = TransferState::Errored;
            t.error = Some("refused by 1.2.3.4:5".into());
            t.ended_at = Some(Utc::now() - chrono::TimeDelta::days(age_days));
            t.removed = t.filename.contains("removed");
            t
        });
        save(&db, ended.iter().map(Snapshot::new).collect())
            .await
            .unwrap();
        let rollups = |db: &Database| {
            db.call(|db| {
                db.query_row(
                    "SELECT (SELECT SUM(files) FROM transfer_user_hours),
                        (SELECT SUM(files) FROM transfer_error_hours),
                        (SELECT group_concat(DISTINCT error) FROM transfer_error_hours)",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
            })
        };
        let before = rollups(&db).await.unwrap();
        assert_eq!(before, (3, 3, "refused by <address>".into()));

        assert_eq!(purge(&db).await.unwrap(), 1);
        assert_eq!(rollups(&db).await.unwrap(), before);
        assert!(
            find(&db, TransferDirection::Upload, rows[0].id.clone())
                .await
                .unwrap()
                .is_none()
        );
        let kept = find(&db, TransferDirection::Upload, rows[1].id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.filename, "music\\old-listed");
        assert_eq!(kept.error.as_deref(), Some("refused by <address>"));
        assert_eq!(purge(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn offline_errors_no_longer_name_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.db");
        let before = MIGRATIONS
            .iter()
            .position(|sql| *sql == include_str!("migrations/offline_errors.sql"))
            .unwrap();
        let failures = [
            ("alice", "User alice appears to be offline"),
            ("alice", "User alice appears to be offline"),
            ("bob", "User bob appears to be offline"),
            ("bob", "refused"),
        ];
        let rows: Vec<_> = failures
            .iter()
            .enumerate()
            .map(|(i, (user, _))| {
                new_transfer(
                    TransferDirection::Upload,
                    user,
                    format!("music\\{i}.flac"),
                    10,
                    None,
                )
            })
            .collect();
        let failed: Vec<_> = rows
            .iter()
            .zip(failures)
            .map(|(t, (_, error))| {
                let mut t = t.clone();
                t.state = TransferState::Errored;
                t.error = Some(error.into());
                t.ended_at = Some(Utc::now());
                t
            })
            .collect();
        let rollups = |db: &Database| {
            db.call(|db| {
                let mut stmt = db.prepare(
                    "SELECT error, username, SUM(files) FROM transfer_error_hours
                     GROUP BY error, username ORDER BY error, username",
                )?;
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<(String, String, i64)>>>()
            })
        };
        let old = Database::open(&path, &MIGRATIONS[..before]).unwrap();
        insert(&old, rows.clone()).await.unwrap();
        save(&old, failed.iter().map(Snapshot::new).collect())
            .await
            .unwrap();
        // A purged transfer's count, which has no row left to move.
        old.call(|db| {
            db.execute(
                "INSERT INTO transfer_error_hours VALUES (0, 'Upload', 'User carol appears to be offline', 'carol', 3)",
                [],
            )
        })
        .await
        .unwrap();
        drop(old);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let stored = db
            .call(|db| {
                let mut stmt = db.prepare("SELECT exception FROM transfer_errors ORDER BY 1")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .unwrap();
        assert_eq!(stored, ["User was offline", "refused"]);
        for (row, error) in rows.iter().zip([
            "User was offline",
            "User was offline",
            "User was offline",
            "refused",
        ]) {
            let found = find(&db, TransferDirection::Upload, row.id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.error.as_deref(), Some(error));
        }
        let offline = |user: &str, files| ("User was offline".to_owned(), user.to_owned(), files);
        assert_eq!(
            rollups(&db).await.unwrap(),
            [
                offline("alice", 2),
                offline("bob", 1),
                offline("carol", 3),
                ("refused".to_owned(), "bob".to_owned(), 1)
            ]
        );
        // The delete trigger finds the moved rows under the new message.
        db.call(|db| db.execute("DELETE FROM transfers", []))
            .await
            .unwrap();
        assert_eq!(rollups(&db).await.unwrap(), [offline("carol", 3)]);
    }

    #[tokio::test]
    async fn exception_names_are_dropped_and_their_errors_merged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transfers.db");
        let before = MIGRATIONS
            .iter()
            .position(|sql| *sql == include_str!("migrations/error_messages.sql"))
            .unwrap();
        let errors = [
            "TimeoutException: no answer",
            "System.TimeoutException: no answer",
            "ConnectionException: A: b",
            "A: b",
            "no name",
        ];
        let rows: Vec<_> = (0..errors.len())
            .map(|i| {
                new_transfer(
                    TransferDirection::Upload,
                    "peer",
                    format!("music\\{i}.flac"),
                    10,
                    None,
                )
            })
            .collect();
        let failed: Vec<_> = rows
            .iter()
            .zip(errors)
            .map(|(t, error)| {
                let mut t = t.clone();
                t.state = TransferState::Errored;
                t.error = Some(error.into());
                t.ended_at = Some(Utc::now());
                t
            })
            .collect();
        let rollups = |db: &Database| {
            db.call(|db| {
                let mut stmt = db.prepare(
                    "SELECT error, SUM(files) FROM transfer_error_hours GROUP BY error ORDER BY error",
                )?;
                stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
        };
        let old = Database::open(&path, &MIGRATIONS[..before]).unwrap();
        insert(&old, rows.clone()).await.unwrap();
        save(&old, failed.iter().map(Snapshot::new).collect())
            .await
            .unwrap();
        let rolled_up = rollups(&old).await.unwrap();
        assert_eq!(
            rolled_up,
            [
                ("A: b".to_owned(), 1),
                ("b".to_owned(), 1),
                ("no answer".to_owned(), 2),
                ("no name".to_owned(), 1)
            ]
        );
        drop(old);

        let db = Database::open(&path, MIGRATIONS).unwrap();
        let stored = db
            .call(|db| {
                let mut stmt = db.prepare("SELECT exception FROM transfer_errors ORDER BY 1")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .unwrap();
        assert_eq!(stored, ["A: b", "b", "no answer", "no name"]);
        for (row, error) in rows
            .iter()
            .zip(["no answer", "no answer", "A: b", "b", "no name"])
        {
            let found = find(&db, TransferDirection::Upload, row.id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.error.as_deref(), Some(error));
        }
        // The rollups are unchanged, and the recreated delete trigger finds each row's key.
        assert_eq!(rollups(&db).await.unwrap(), rolled_up);
        db.call(|db| db.execute("DELETE FROM transfers", []))
            .await
            .unwrap();
        assert!(rollups(&db).await.unwrap().is_empty());
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
