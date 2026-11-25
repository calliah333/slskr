//! The persistent event history (`events.db`) and `GET /api/events/history`.
//!
//! Events are written in order by one task, several per transaction when they arrive
//! together. The history keeps the newest [`MAX_EVENTS`] events of the last [`MAX_AGE`].

use std::{sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::{HeaderName, header::HeaderValue},
};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use slskr_types::events::{Event, EventData, EventType};
use tokio::{sync::mpsc, time::MissedTickBehavior};
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    auth::Authenticated,
    db::Database,
    http::{ApiError, ApiResult, Json, Query, Routes},
};

const MIGRATIONS: &[&str] = &[
    "CREATE TABLE events (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, type TEXT NOT NULL, data TEXT)",
    // Event types and payloads use the API's snake_case names; older test events are dropped.
    "DELETE FROM events WHERE type IN ('None', 'Any', 'Noop');
     UPDATE events SET type = CASE type
         WHEN 'DownloadFileComplete' THEN 'download_file_complete'
         WHEN 'DownloadDirectoryComplete' THEN 'download_directory_complete'
         WHEN 'UploadFileComplete' THEN 'upload_file_complete'
         WHEN 'PrivateMessageReceived' THEN 'private_message_received'
         WHEN 'RoomMessageReceived' THEN 'room_message_received'
         WHEN 'SoulseekClientConnected' THEN 'server_connected'
         WHEN 'SoulseekClientDisconnected' THEN 'server_disconnected'
         ELSE type END;",
    // History pages and retention order by time.
    "CREATE INDEX events_timestamp ON events(timestamp);",
];
/// Header of `GET /api/events/history` carrying the number of recorded events.
const TOTAL_COUNT: HeaderName = HeaderName::from_static("x-total-count");
/// Events older than this are removed from the history.
const MAX_AGE: Duration = Duration::from_secs(30 * 86_400);
/// The history keeps at most this many (the newest) events.
const MAX_EVENTS: u32 = 10_000;
/// How often the history is pruned; the first time at startup.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3_600);
/// Most queued events written in one transaction.
const WRITE_BATCH: usize = 256;

#[derive(Clone)]
pub struct EventService(Arc<Inner>);

struct Inner {
    database: Database,
    writer: mpsc::UnboundedSender<Event>,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    offset: u32,
    #[serde(default = "default_limit")]
    limit: u32,
}
fn default_limit() -> u32 {
    100
}

impl EventService {
    /// Opens `events.db` and starts its writer, which stops when the service is dropped.
    pub fn new(core: &Core) -> anyhow::Result<Self> {
        let path = core.paths.db_dir.join("events.db");
        let database = Database::open(&path, MIGRATIONS)?;
        let (writer, queue) = mpsc::unbounded_channel();
        tokio::spawn(run_writer(database.clone(), queue));
        Ok(Self(Arc::new(Inner { database, writer })))
    }

    /// Records an event in the history.
    pub fn raise(&self, data: EventData) {
        let event = Event::new(Uuid::new_v4().to_string(), Utc::now(), data);
        let _ = self.0.writer.send(event);
    }

    /// Newest first; unreadable rows are skipped.
    async fn page(&self, offset: u32, limit: u32) -> anyhow::Result<(Vec<Event>, i64)> {
        self.0
            .database
            .call(move |conn| page(conn, offset, limit))
            .await
    }
}

/// Persists queued events in order and prunes the history periodically.
async fn run_writer(database: Database, mut queue: mpsc::UnboundedReceiver<Event>) {
    let mut prune_timer = tokio::time::interval(PRUNE_INTERVAL);
    prune_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = queue.recv() => {
                let Some(event) = event else { break };
                let mut batch = vec![event];
                while batch.len() < WRITE_BATCH
                    && let Ok(event) = queue.try_recv()
                {
                    batch.push(event);
                }
                if let Err(error) = database.call(move |conn| insert(conn, &batch)).await {
                    tracing::error!(%error, "failed to persist events");
                }
            }
            _ = prune_timer.tick() => {
                if let Err(error) = database.call(|conn| prune(conn, Utc::now())).await {
                    tracing::warn!(%error, "could not prune the event history");
                }
            }
        }
    }
}

fn insert(conn: &mut Connection, events: &[Event]) -> rusqlite::Result<()> {
    let transaction = conn.transaction()?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT OR REPLACE INTO events (id, timestamp, type, data) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for event in events {
            statement.execute(rusqlite::params![
                event.id,
                event.timestamp.to_rfc3339(),
                event.kind.as_str(),
                event.data.to_string()
            ])?;
        }
    }
    transaction.commit()
}

/// Removes events older than [`MAX_AGE`] and all but the newest [`MAX_EVENTS`].
fn prune(conn: &Connection, now: DateTime<Utc>) -> rusqlite::Result<usize> {
    let cutoff = now - chrono::TimeDelta::from_std(MAX_AGE).unwrap_or(chrono::TimeDelta::MAX);
    let expired = conn
        .prepare_cached("DELETE FROM events WHERE timestamp < ?1")?
        .execute([cutoff.to_rfc3339()])?;
    // The subquery is NULL, and nothing is deleted, while there are fewer events.
    let excess = conn
        .prepare_cached(
            "DELETE FROM events WHERE timestamp <
                (SELECT timestamp FROM events ORDER BY timestamp DESC LIMIT 1 OFFSET ?1 - 1)",
        )?
        .execute([MAX_EVENTS])?;
    Ok(expired + excess)
}

fn page(conn: &Connection, offset: u32, limit: u32) -> rusqlite::Result<(Vec<Event>, i64)> {
    let count: i64 = conn
        .prepare_cached("SELECT COUNT(*) FROM events")?
        .query_row([], |row| row.get(0))?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, timestamp, type, data FROM events ORDER BY timestamp DESC LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit, offset], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (id, timestamp, kind, data) = row?;
        let (Ok(timestamp), Some(kind)) = (
            chrono::DateTime::parse_from_rfc3339(&timestamp),
            EventType::parse(&kind),
        ) else {
            continue;
        };
        let data = data
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default();
        events.push(Event {
            id,
            timestamp: timestamp.with_timezone(&Utc),
            kind,
            data,
        });
    }
    Ok((events, count))
}

pub fn routes() -> Routes {
    Routes::new().get("/api/events/history", history)
}

async fn history(
    _auth: Authenticated,
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> ApiResult<([(HeaderName, HeaderValue); 1], Json<Vec<Event>>)> {
    if page.limit == 0 {
        return Err(ApiError::BadRequest(
            "limit must be greater than zero".into(),
        ));
    }
    let (events, count) = state.services.events.page(page.offset, page.limit).await?;
    Ok(([(TOTAL_COUNT, HeaderValue::from(count))], Json(events)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slskr_types::events::EventData;

    fn event(at: DateTime<Utc>) -> Event {
        Event::new(Uuid::new_v4().to_string(), at, EventData::ServerConnected)
    }

    #[tokio::test]
    async fn history_is_newest_first_and_pruned_by_age_and_count() {
        let db = Database::open(std::path::Path::new(":memory:"), MIGRATIONS).unwrap();
        let now = Utc::now();
        let old = event(now - chrono::TimeDelta::days(31));
        // More than the cap, one second apart, the newest last.
        let recent: Vec<_> = (0..MAX_EVENTS + 5)
            .map(|i| event(now - chrono::TimeDelta::seconds(i64::from(MAX_EVENTS + 5 - i))))
            .collect();
        let newest = recent.last().unwrap().id.clone();
        let oldest_kept = recent[5].id.clone();
        let mut all = vec![old];
        all.extend(recent);
        db.call(move |conn| insert(conn, &all)).await.unwrap();

        let (events, count) = db.call(|conn| page(conn, 0, 2)).await.unwrap();
        assert_eq!(count, i64::from(MAX_EVENTS) + 6);
        assert_eq!(events[0].id, newest);
        assert!(events[0].timestamp > events[1].timestamp);

        let removed = db.call(move |conn| prune(conn, now)).await.unwrap();
        assert_eq!(removed, 6);
        let (events, count) = db
            .call(|conn| page(conn, MAX_EVENTS - 1, 10))
            .await
            .unwrap();
        assert_eq!(count, i64::from(MAX_EVENTS));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, oldest_kept);
        assert_eq!(db.call(move |conn| prune(conn, now)).await.unwrap(), 0);
    }
}
