//! Persistent application events and `/events` HTTP routes.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use slskr_api::events::{Event, EventHeader, EventRecord, EventType};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    db::Database,
    http::{ApiError, ApiResult},
};
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE events (id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, type TEXT NOT NULL, data TEXT)",
];
const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct EventService(Arc<Inner>);

struct Inner {
    database: Database,
    subscribers: broadcast::Sender<Event>,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    offset: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
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

    pub fn raise(&self, event: Event) {
        let record = event.to_record();
        let db = self.0.database.clone();
        tokio::spawn(async move {
            if let Err(error) = db.call(move |conn| {
                conn.execute("INSERT OR REPLACE INTO events (id, timestamp, type, data) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![record.id, record.timestamp.to_rfc3339(), record.kind.name(), record.data])?;
                Ok(())
            }).await {
                tracing::error!(%error, "failed to persist event");
            }
        });
        let _ = self.0.subscribers.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribers.subscribe()
    }

    async fn page(&self, offset: i64, limit: i64) -> anyhow::Result<(Vec<EventRecord>, i64)> {
        let database = self.0.database.clone();
        database.call(move |conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            let mut stmt = conn.prepare("SELECT timestamp, type, data, id FROM events ORDER BY timestamp DESC LIMIT ?1 OFFSET ?2")?;
            let rows = stmt.query_map(rusqlite::params![limit, offset], |row| {
                let timestamp: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let data: Option<String> = row.get(2)?;
                let id: String = row.get(3)?;
                Ok((timestamp, kind, data, id))
            })?;
            let mut output = Vec::new();
            for row in rows {
                let (timestamp, kind, data, id) = row?;
                let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp)
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
                    .with_timezone(&Utc);
                let kind = serde_json::from_str(&format!("\"{kind}\""))
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
                output.push(EventRecord { timestamp, kind, data, id });
            }
            Ok((output, count))
        }).await
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/events", get(list))
        .route("/events/{kind}", post(raise))
}

async fn list(
    _auth: Authenticated,
    State(state): State<AppState>,
    Query(page): Query<Page>,
) -> ApiResult<(
    [(axum::http::HeaderName, String); 1],
    Json<Vec<EventRecord>>,
)> {
    if page.offset < 0 {
        return Err(ApiError::BadRequest(
            "Offset must be greater than or equal to zero".into(),
        ));
    }
    if page.limit <= 0 {
        return Err(ApiError::BadRequest(
            "Limit must be greater than zero".into(),
        ));
    }
    let (records, count) = state
        .services
        .events
        .page(page.offset, page.limit)
        .await
        .map_err(ApiError::Internal)?;
    Ok((
        [(
            axum::http::header::HeaderName::from_static("x-total-count"),
            count.to_string(),
        )],
        Json(records),
    ))
}

async fn raise(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    axum::extract::Path(kind): axum::extract::Path<String>,
    body: Option<Json<String>>,
) -> ApiResult<(StatusCode, Json<Event>)> {
    let kind = EventType::parse(&kind)
        .ok_or_else(|| ApiError::BadRequest(format!("Unknown event type '{kind}'")))?;
    if matches!(kind, EventType::None | EventType::Any) {
        return Err(ApiError::BadRequest(format!(
            "Event type '{}' cannot be raised",
            kind.name()
        )));
    }
    let disambiguator = body.map(|Json(value)| value).unwrap_or_default();
    let header = EventHeader {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        kind,
        version: 1,
    };
    let event = match kind {
        EventType::SoulseekClientConnected => {
            Event::SoulseekClientConnected(slskr_api::events::SoulseekClientConnectedEvent {
                header,
            })
        }
        EventType::SoulseekClientDisconnected => {
            Event::SoulseekClientDisconnected(slskr_api::events::SoulseekClientDisconnectedEvent {
                header,
                ..Default::default()
            })
        }
        EventType::DownloadFileComplete => {
            Event::DownloadFileComplete(slskr_api::events::DownloadFileCompleteEvent {
                header,
                local_filename: format!("{disambiguator}local.file"),
                remote_filename: format!("{disambiguator}remote.file"),
                transfer: serde_json::json!({}),
            })
        }
        EventType::DownloadDirectoryComplete => {
            Event::DownloadDirectoryComplete(slskr_api::events::DownloadDirectoryCompleteEvent {
                header,
                local_directory_name: format!("{disambiguator}local.directory"),
                remote_directory_name: format!("{disambiguator}remote.directory"),
                username: Some(format!("{disambiguator}username")),
            })
        }
        EventType::UploadFileComplete => {
            Event::UploadFileComplete(slskr_api::events::UploadFileCompleteEvent {
                header,
                local_filename: format!("{disambiguator}local.file"),
                remote_filename: format!("{disambiguator}remote.file"),
                transfer: serde_json::json!({}),
            })
        }
        EventType::PrivateMessageReceived => {
            Event::PrivateMessageReceived(slskr_api::events::PrivateMessageReceivedEvent {
                header,
                message: serde_json::json!({"id":42,"timestamp":Utc::now(),"username":format!("{disambiguator}username"),"message":format!("{disambiguator}message")}),
            })
        }
        EventType::RoomMessageReceived => {
            Event::RoomMessageReceived(slskr_api::events::RoomMessageReceivedEvent {
                header,
                message: serde_json::json!({"roomName":format!("{disambiguator}room"),"username":format!("{disambiguator}username"),"message":format!("{disambiguator}message")}),
            })
        }
        EventType::Noop => Event::Noop(slskr_api::events::NoopEvent { header }),
        EventType::None | EventType::Any => unreachable!(),
    };
    state.services.events.raise(event.clone());
    Ok((StatusCode::CREATED, Json(event)))
}
