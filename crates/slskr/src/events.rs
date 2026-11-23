//! The persistent event history (`events.db`) and `GET /api/events/history`.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderName, header::HeaderValue},
};
use chrono::Utc;
use serde::Deserialize;
use slskr_types::events::{Event, EventData, EventType};
use tokio::sync::broadcast;
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
];
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// Header of `GET /api/events/history` carrying the number of recorded events.
const TOTAL_COUNT: HeaderName = HeaderName::from_static("x-total-count");

#[derive(Clone)]
pub struct EventService(Arc<Inner>);

struct Inner {
    database: Database,
    subscribers: broadcast::Sender<Event>,
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
    pub fn new(core: &Core) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&core.paths.db_dir)?;
        let path = core.paths.db_dir.join("events.db");
        let database = Database::open(&path, MIGRATIONS)?;
        let (subscribers, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(Self(Arc::new(Inner {
            database,
            subscribers,
        })))
    }

    /// Records an event and notifies subscribers.
    pub fn raise(&self, data: EventData) {
        let event = Event::new(Uuid::new_v4().to_string(), Utc::now(), data);
        let record = event.clone();
        let db = self.0.database.clone();
        tokio::spawn(async move {
            if let Err(error) = db
                .call(move |conn| {
                    conn.execute(
                        "INSERT OR REPLACE INTO events (id, timestamp, type, data) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            record.id,
                            record.timestamp.to_rfc3339(),
                            record.kind.as_str(),
                            record.data.to_string()
                        ],
                    )?;
                    Ok(())
                })
                .await
            {
                tracing::error!(%error, "failed to persist event");
            }
        });
        let _ = self.0.subscribers.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribers.subscribe()
    }

    /// Newest first; unreadable rows are skipped.
    async fn page(&self, offset: u32, limit: u32) -> anyhow::Result<(Vec<Event>, i64)> {
        self.0
            .database
            .call(move |conn| {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
                let mut stmt = conn.prepare(
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
            })
            .await
    }
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
