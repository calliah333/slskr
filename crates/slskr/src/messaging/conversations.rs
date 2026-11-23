//! Persistent private conversations and message delivery acknowledgements.

use std::path::Path;

use axum::{
    Json, Router,
    extract::{Path as RoutePath, Query, State},
    http::StatusCode,
    routing::{get, put},
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use serde::Deserialize;
use slskr_api::{
    conversations::{Conversation, MessageDirection, PrivateMessage},
    events::{Event, EventType, PrivateMessageReceivedEvent},
};
use tokio::sync::broadcast::error::RecvError;

use super::{ensure_available, event_header};
use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    db::Database,
    events::EventService,
    http::{ApiError, ApiResult},
};

const MIGRATIONS: &[&str] = &[
    "CREATE TABLE conversations (username TEXT PRIMARY KEY, is_active INTEGER NOT NULL DEFAULT 1);
     CREATE TABLE private_messages (
        username TEXT NOT NULL, id INTEGER NOT NULL, timestamp TEXT NOT NULL,
        direction TEXT NOT NULL, message TEXT NOT NULL, is_acknowledged INTEGER NOT NULL DEFAULT 0,
        was_replayed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(username, id, timestamp)
     );
     CREATE INDEX ix_private_messages_username ON private_messages(username);",
    "DROP INDEX ix_private_messages_username;
     CREATE INDEX ix_private_messages_history ON private_messages(username, timestamp);
     CREATE INDEX ix_private_messages_unread ON private_messages(username, id) WHERE is_acknowledged = 0;",
    // Bumped whenever a (replayed) message is reset to unacknowledged, so an acknowledgement
    // only commits rows that were not reset while its network ACK was in flight.
    "ALTER TABLE private_messages ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;",
];

const HISTORY_LIMIT: usize = 100;

#[derive(Clone)]
pub struct ConversationService {
    db: Database,
    client: soulseek::SoulseekClient,
    events: EventService,
    // No lock is held across network I/O. Instead, a replay that resets a message while its
    // ACK is in flight bumps `revision`, so `mark_acknowledged` skips it, and `close` only
    // deactivates a conversation that has no unacknowledged messages left.
}

impl ConversationService {
    pub fn new(core: &Core, events: &EventService) -> anyhow::Result<Self> {
        let db = if core.options.get().flags.volatile {
            Database::open(Path::new(":memory:"), MIGRATIONS)?
        } else {
            Database::open(&core.paths.data_dir.join("messaging.db"), MIGRATIONS)?
        };
        Ok(Self {
            db,
            client: core.client.clone(),
            events: events.clone(),
        })
    }

    pub fn start(&self, app: AppState) {
        let service = self.clone();
        let mut events = self.client.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = app.shutdown.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(soulseek::Event::PrivateMessageReceived(message)) => {
                            if app.services.users.is_blacklisted(&message.username, None) {
                                continue;
                            }
                            if let Err(error) = service.receive(message).await {
                                tracing::error!(%error, "failed to record private message");
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "private message event receiver lagged");
                        }
                        Err(RecvError::Closed) => break,
                        _ => {}
                    },
                }
            }
        });
    }

    async fn receive(&self, message: soulseek::PrivateMessage) -> anyhow::Result<()> {
        let message = self.db.call(move |db| {
            let record = PrivateMessage {
                timestamp: message.timestamp.into(),
                id: message.id as i32,
                username: message.username,
                direction: MessageDirection::In,
                message: message.message,
                is_acknowledged: false,
                was_replayed: message.replayed,
            };
            let tx = db.transaction()?;
            activate(&tx, &record.username)?;
            tx.execute(
                "INSERT INTO private_messages(username,id,timestamp,direction,message,is_acknowledged,was_replayed)
                 VALUES (?1,?2,?3,'In',?4,0,?5)
                 ON CONFLICT(username,id,timestamp) DO UPDATE
                 SET is_acknowledged=0, was_replayed=excluded.was_replayed, revision=revision+1",
                params![record.username, record.id, record.timestamp.to_rfc3339(), record.message, record.was_replayed],
            )?;
            tx.commit()?;
            Ok(record)
        }).await?;
        self.events
            .raise(Event::PrivateMessageReceived(PrivateMessageReceivedEvent {
                header: event_header(EventType::PrivateMessageReceived),
                message: serde_json::to_value(message)?,
            }));
        Ok(())
    }

    async fn messages(
        &self,
        username: String,
        unacknowledged: bool,
    ) -> anyhow::Result<Option<Vec<PrivateMessage>>> {
        self.db
            .call(move |db| {
                if !exists(db, &username, false)? {
                    return Ok(None);
                }
                recent_messages(db, &username, unacknowledged).map(Some)
            })
            .await
    }

    async fn conversation(
        &self,
        username: String,
        include_messages: bool,
    ) -> anyhow::Result<Option<Conversation>> {
        self.db
            .call(move |db| {
                let mut conversation = db
                    .query_row(
                        "SELECT c.username, c.is_active,
                    (SELECT count(*) FROM private_messages m
                     WHERE m.username = c.username AND m.is_acknowledged = 0)
                 FROM conversations c WHERE c.username = ?1",
                        [&username],
                        read_conversation,
                    )
                    .optional()?;
                if include_messages && let Some(conversation) = &mut conversation {
                    conversation.messages = Some(recent_messages(db, &username, false)?);
                }
                Ok(conversation)
            })
            .await
    }

    async fn list(
        &self,
        include_inactive: bool,
        unacked_only: bool,
    ) -> anyhow::Result<Vec<Conversation>> {
        self.db
            .call(move |db| {
                let mut stmt = db.prepare(
                    "SELECT c.username, c.is_active, coalesce(unread.count, 0)
                 FROM conversations c
                 LEFT JOIN (
                    SELECT username, count(*) AS count FROM private_messages
                    WHERE is_acknowledged = 0 GROUP BY username
                 ) unread ON unread.username = c.username
                 WHERE (?1 OR c.is_active = 1) AND (?2 = 0 OR unread.count IS NOT NULL)
                 ORDER BY c.username",
                )?;
                stmt.query_map(params![include_inactive, unacked_only], read_conversation)?
                    .collect()
            })
            .await
    }

    async fn acknowledge(&self, username: String, id: Option<i32>) -> ApiResult<bool> {
        let selected = self
            .db
            .call(move |db| unacknowledged_messages(db, &username, id))
            .await?;
        let Some(mut selected) = selected else {
            return Ok(false);
        };

        let mut sent = 0;
        let mut failure = None;
        for &(_, id, _) in &selected {
            if let Err(error) = self.client.acknowledge_private_message(id as u32).await {
                failure = Some(error);
                break;
            }
            sent += 1;
        }
        if sent != 0 {
            selected.truncate(sent);
            self.db
                .call(move |db| mark_acknowledged(db, &selected))
                .await?;
        }
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(true)
    }

    async fn close(&self, username: String) -> ApiResult<bool> {
        let active = self
            .db
            .call({
                let username = username.clone();
                move |db| exists(db, &username, true)
            })
            .await?;
        if !active {
            return Ok(false);
        }
        self.acknowledge(username.clone(), None).await?;
        self.db.call(move |db| deactivate(db, &username)).await?;
        Ok(true)
    }

    async fn send(&self, username: String, message: String) -> ApiResult<()> {
        self.client
            .send_private_message(&username, &message)
            .await?;
        let timestamp = Utc::now().to_rfc3339();
        self.db.call(move |db| {
            let tx = db.transaction()?;
            activate(&tx, &username)?;
            tx.execute(
                "INSERT INTO private_messages(username,id,timestamp,direction,message,is_acknowledged,was_replayed)
                 VALUES (?1,0,?2,'Out',?3,1,0)",
                params![username, timestamp, message],
            )?;
            tx.commit()
        }).await?;
        Ok(())
    }
}

fn activate(db: &Connection, username: &str) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO conversations(username,is_active) VALUES (?1,1)
         ON CONFLICT(username) DO UPDATE SET is_active=1",
        [username],
    )?;
    Ok(())
}

fn exists(db: &Connection, username: &str, active_only: bool) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversations WHERE username = ?1 AND (?2 = 0 OR is_active = 1))",
        params![username, active_only],
        |row| row.get(0),
    )
}

fn read_conversation(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    let count = row.get::<_, u32>(2)?;
    Ok(Conversation {
        username: row.get(0)?,
        is_active: row.get(1)?,
        un_acknowledged_message_count: count,
        has_un_acknowledged_messages: count != 0,
        messages: None,
    })
}

fn read_message(row: &Row<'_>) -> rusqlite::Result<PrivateMessage> {
    let timestamp = DateTime::parse_from_rfc3339(row.get_ref(0)?.as_str()?)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))?
        .with_timezone(&Utc);
    let direction = match row.get_ref(3)?.as_str()? {
        "In" => MessageDirection::In,
        "Out" => MessageDirection::Out,
        _ => {
            return Err(rusqlite::Error::InvalidColumnType(
                3,
                "direction".into(),
                Type::Text,
            ));
        }
    };
    Ok(PrivateMessage {
        timestamp,
        direction,
        id: row.get(1)?,
        username: row.get(2)?,
        message: row.get(4)?,
        is_acknowledged: row.get(5)?,
        was_replayed: row.get(6)?,
    })
}

fn recent_messages(
    db: &Connection,
    username: &str,
    unacknowledged: bool,
) -> rusqlite::Result<Vec<PrivateMessage>> {
    let mut stmt = db.prepare(
        "SELECT timestamp,id,username,direction,message,is_acknowledged,was_replayed
         FROM (SELECT rowid,* FROM private_messages WHERE username = ?1
               ORDER BY timestamp DESC, rowid DESC LIMIT ?2)
         WHERE (?3 = 0 OR is_acknowledged = 0)
         ORDER BY timestamp, rowid",
    )?;
    stmt.query_map(
        params![username, HISTORY_LIMIT as u32, unacknowledged],
        read_message,
    )?
    .collect()
}

fn unacknowledged_messages(
    db: &Connection,
    username: &str,
    id: Option<i32>,
) -> rusqlite::Result<Option<Vec<(i64, i32, i64)>>> {
    let (known, sql) = match id {
        Some(id) => (
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM private_messages WHERE username = ?1 AND id = ?2)",
                params![username, id],
                |row| row.get(0),
            )?,
            "SELECT rowid, id, revision FROM private_messages
             WHERE username = ?1 AND id = ?2 AND is_acknowledged = 0",
        ),
        None => (
            exists(db, username, false)?,
            "SELECT rowid, id, revision FROM private_messages
             WHERE username = ?1 AND is_acknowledged = 0",
        ),
    };
    if !known {
        return Ok(None);
    }
    let mut stmt = db.prepare(sql)?;
    let mut rows = if let Some(id) = id {
        stmt.query(params![username, id])?
    } else {
        stmt.query([username])?
    };
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        messages.push((row.get(0)?, row.get(1)?, row.get(2)?));
    }
    Ok(Some(messages))
}

/// Marks the selected rows acknowledged, skipping any a replay reset after selection.
fn mark_acknowledged(db: &mut Connection, messages: &[(i64, i32, i64)]) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    {
        let mut update = tx.prepare(
            "UPDATE private_messages SET is_acknowledged = 1 WHERE rowid = ?1 AND revision = ?2",
        )?;
        for &(rowid, _, revision) in messages {
            update.execute([rowid, revision])?;
        }
    }
    tx.commit()
}

/// Deactivates a conversation unless a message arrived (or was reset) since it was acknowledged.
fn deactivate(db: &Connection, username: &str) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE conversations SET is_active = 0 WHERE username = ?1 AND NOT EXISTS(
            SELECT 1 FROM private_messages WHERE username = ?1 AND is_acknowledged = 0)",
        [username],
    )?;
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list))
        .route(
            "/conversations/{username}",
            get(get_one).put(ack_all).delete(close).post(send),
        )
        .route("/conversations/{username}/{id}", put(ack_one))
        .route("/conversations/{username}/messages", get(messages))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ListQuery {
    include_inactive: bool,
    un_acknowledged_only: bool,
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GetQuery {
    include_messages: bool,
}

impl Default for GetQuery {
    fn default() -> Self {
        Self {
            include_messages: true,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MessageQuery {
    un_acknowledged_only: bool,
}

fn not_found() -> ApiError {
    ApiError::NotFound(String::new())
}

async fn list(
    _: Authenticated,
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Conversation>>> {
    ensure_available(&app)?;
    Ok(Json(
        app.services
            .messaging
            .conversations
            .list(q.include_inactive, q.un_acknowledged_only)
            .await?,
    ))
}

async fn get_one(
    _: Authenticated,
    State(app): State<AppState>,
    RoutePath(username): RoutePath<String>,
    Query(q): Query<GetQuery>,
) -> ApiResult<Json<Conversation>> {
    ensure_available(&app)?;
    app.services
        .messaging
        .conversations
        .conversation(username, q.include_messages)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

async fn messages(
    _: Authenticated,
    State(app): State<AppState>,
    RoutePath(username): RoutePath<String>,
    Query(q): Query<MessageQuery>,
) -> ApiResult<Json<Vec<PrivateMessage>>> {
    ensure_available(&app)?;
    app.services
        .messaging
        .conversations
        .messages(username, q.un_acknowledged_only)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

async fn ack_all(
    _: RequireReadWrite,
    State(app): State<AppState>,
    RoutePath(username): RoutePath<String>,
) -> ApiResult<StatusCode> {
    ensure_available(&app)?;
    if !app
        .services
        .messaging
        .conversations
        .acknowledge(username, None)
        .await?
    {
        return Err(not_found());
    }
    Ok(StatusCode::OK)
}

async fn ack_one(
    _: RequireReadWrite,
    State(app): State<AppState>,
    RoutePath((username, id)): RoutePath<(String, i32)>,
) -> ApiResult<StatusCode> {
    ensure_available(&app)?;
    if !app
        .services
        .messaging
        .conversations
        .acknowledge(username, Some(id))
        .await?
    {
        return Err(not_found());
    }
    Ok(StatusCode::OK)
}

async fn close(
    _: RequireReadWrite,
    State(app): State<AppState>,
    RoutePath(username): RoutePath<String>,
) -> ApiResult<StatusCode> {
    ensure_available(&app)?;
    if !app.services.messaging.conversations.close(username).await? {
        return Err(not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn send(
    _: RequireReadWrite,
    State(app): State<AppState>,
    RoutePath(username): RoutePath<String>,
    Json(message): Json<Option<String>>,
) -> ApiResult<StatusCode> {
    ensure_available(&app)?;
    if app.services.users.is_blacklisted(&username, None) {
        return Ok(StatusCode::OK);
    }
    let message = message
        .filter(|message| !message.is_empty())
        .ok_or_else(|| ApiError::BadRequest(String::new()))?;
    app.services
        .messaging
        .conversations
        .send(username, message)
        .await?;
    Ok(StatusCode::CREATED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acknowledging_is_independent_of_history_limit_and_only_marks_selected_rows() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        db.call(|db| {
            activate(db, "alice")?;
            assert_eq!(unacknowledged_messages(db, "missing", None)?, None);
            assert_eq!(unacknowledged_messages(db, "alice", Some(0))?, None);
            let first = Utc::now();
            for id in 1..=102 {
                db.execute(
                    "INSERT INTO private_messages (username,id,timestamp,direction,message,is_acknowledged)
                     VALUES ('alice',?1,?2,'In','hello',?3)",
                    params![id, (first + chrono::TimeDelta::seconds(id)).to_rfc3339(), id > 2],
                )?;
            }
            let recent = recent_messages(db, "alice", false)?;
            assert_eq!(recent.len(), HISTORY_LIMIT);
            assert_eq!(recent.first().unwrap().id, 3);
            assert_eq!(recent.last().unwrap().id, 102);
            assert!(recent_messages(db, "alice", true)?.is_empty());
            let all = unacknowledged_messages(db, "alice", None)?.unwrap();
            assert_eq!(all.len(), 2);
            let first = unacknowledged_messages(db, "alice", Some(1))?.unwrap();
            assert_eq!(first.len(), 1);
            mark_acknowledged(db, &first)?;
            assert!(unacknowledged_messages(db, "alice", Some(1))?.unwrap().is_empty());
            assert_eq!(unacknowledged_messages(db, "alice", None)?.unwrap().iter().map(|&(_, id, _)| id).collect::<Vec<_>>(), vec![2]);
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn replay_during_ack_keeps_message_unread_and_conversation_open() {
        let db = Database::open(Path::new(":memory:"), MIGRATIONS).unwrap();
        db.call(|db| {
            activate(db, "alice")?;
            let upsert = |db: &Connection| {
                db.execute(
                    "INSERT INTO private_messages(username,id,timestamp,direction,message,is_acknowledged,was_replayed)
                     VALUES ('alice',7,'2026-01-01T00:00:00+00:00','In','hi',0,1)
                     ON CONFLICT(username,id,timestamp) DO UPDATE
                     SET is_acknowledged=0, was_replayed=excluded.was_replayed, revision=revision+1",
                    [],
                )
            };
            upsert(db)?;
            let selected = unacknowledged_messages(db, "alice", None)?.unwrap();
            // The server replays the message while the ACK is in flight.
            upsert(db)?;
            mark_acknowledged(db, &selected)?;
            assert_eq!(unacknowledged_messages(db, "alice", None)?.unwrap().len(), 1);
            deactivate(db, "alice")?;
            assert!(exists(db, "alice", true)?);

            let selected = unacknowledged_messages(db, "alice", None)?.unwrap();
            mark_acknowledged(db, &selected)?;
            assert!(unacknowledged_messages(db, "alice", None)?.unwrap().is_empty());
            deactivate(db, "alice")?;
            assert!(!exists(db, "alice", true)?);
            Ok(())
        }).await.unwrap();
    }

    #[test]
    fn query_switches_use_api_casing() {
        let query: ListQuery =
            serde_json::from_str(r#"{"includeInactive":true,"unAcknowledgedOnly":true}"#).unwrap();
        assert!(query.include_inactive && query.un_acknowledged_only);
        let query: GetQuery = serde_json::from_str(r#"{"includeMessages":false}"#).unwrap();
        assert!(!query.include_messages);
        let query: GetQuery = serde_json::from_str("{}").unwrap();
        assert!(query.include_messages);
    }
}
