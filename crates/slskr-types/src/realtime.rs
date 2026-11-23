//! Live updates over one WebSocket (`GET {url_base}/api/events`).
//!
//! The client sends [`Command`]s to subscribe to [`Topic`]s. For every topic it subscribes
//! to, the server first sends that topic's snapshot, then the changes after it, each as one
//! JSON text frame holding an [`Event`]. Unknown or invalid commands are answered with
//! [`Event::Error`]; they do not close the connection.

use serde::{Deserialize, Serialize};

use crate::{
    application::State,
    config::Config,
    logs::LogRecord,
    search::{Search, SearchResponse},
    telemetry::CurrentMetrics,
};

/// Path of the endpoint below `web.url_base`.
pub const PATH: &str = "/api/events";
/// Largest message a client may send (bytes).
pub const MAX_CLIENT_MESSAGE: usize = 4096;

/// Close code: the session ended or the credentials were revoked; sign in again.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
/// Close code: nothing was received from the client in time.
pub const CLOSE_TIMEOUT: u16 = 4408;
/// Close code: the client did not keep up with its updates; reconnect and resubscribe.
pub const CLOSE_TOO_SLOW: u16 = 4429;

/// A group of related events with a common snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// [`Event::State`].
    State,
    /// [`Event::Settings`].
    Settings,
    /// [`Event::SearchList`] snapshot, then the other `search_*` events.
    Searches,
    /// [`Event::LogBuffer`] snapshot, then [`Event::Log`]. Administrators only.
    Logs,
    /// [`Event::Metrics`].
    Metrics,
}

impl Topic {
    pub const ALL: [Topic; 5] = [
        Self::State,
        Self::Settings,
        Self::Searches,
        Self::Logs,
        Self::Metrics,
    ];

    /// Position in [`Topic::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Settings => "settings",
            Self::Searches => "searches",
            Self::Logs => "logs",
            Self::Metrics => "metrics",
        }
    }
}

/// Client to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Start receiving `topics`; each one's snapshot is sent first, also when the topic was
    /// already subscribed.
    Subscribe { topics: Vec<Topic> },
    /// Stop receiving `topics`.
    Unsubscribe { topics: Vec<Topic> },
}

/// Server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Application state; snapshot and (coalesced) changes of [`Topic::State`].
    State(Box<State>),
    /// The effective configuration with secrets redacted (as `GET /api/config`).
    Settings(Box<Config>),
    /// The buffered recent log records; snapshot of [`Topic::Logs`].
    LogBuffer(Vec<LogRecord>),
    /// A new log record.
    Log(LogRecord),
    /// Every search, without responses; snapshot of [`Topic::Searches`].
    SearchList(Vec<Search>),
    /// A search was created.
    SearchCreated(Search),
    /// A search changed state or counts (progress is coalesced per search).
    SearchUpdated(Search),
    /// A search was deleted.
    SearchDeleted { id: String },
    /// Responses received for a search since the previous batch.
    SearchResponses {
        search_id: String,
        responses: Vec<SearchResponse>,
    },
    /// Live transfer metrics, about once a second.
    Metrics(CurrentMetrics),
    /// A command was not understood or not allowed. `topic` names the refused topic.
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        topic: Option<Topic>,
    },
}

impl Event {
    /// The topic carrying this event; `None` for [`Event::Error`].
    pub fn topic(&self) -> Option<Topic> {
        Some(match self {
            Self::State(_) => Topic::State,
            Self::Settings(_) => Topic::Settings,
            Self::LogBuffer(_) | Self::Log(_) => Topic::Logs,
            Self::SearchList(_)
            | Self::SearchCreated(_)
            | Self::SearchUpdated(_)
            | Self::SearchDeleted { .. }
            | Self::SearchResponses { .. } => Topic::Searches,
            Self::Metrics(_) => Topic::Metrics,
            Self::Error { .. } => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::from_value::<Command>(
                json!({"type": "subscribe", "topics": ["state", "logs"]})
            )
            .unwrap(),
            Command::Subscribe {
                topics: vec![Topic::State, Topic::Logs]
            }
        );
        for invalid in [
            json!({"type": "subscribe", "topics": ["nope"]}),
            json!({"type": "subscribe"}),
            json!({"type": "publish", "topics": []}),
            json!({"type": "unsubscribe", "topics": [], "extra": 1}),
        ] {
            assert!(serde_json::from_value::<Command>(invalid).is_err());
        }
        assert_eq!(
            serde_json::to_value(Event::SearchDeleted { id: "x".into() }).unwrap(),
            json!({"type": "search_deleted", "data": {"id": "x"}})
        );
        assert_eq!(
            serde_json::to_value(Event::Error {
                message: "m".into(),
                topic: Some(Topic::Logs)
            })
            .unwrap(),
            json!({"type": "error", "data": {"message": "m", "topic": "logs"}})
        );
        let state = serde_json::to_value(Event::State(Box::default())).unwrap();
        assert_eq!(state["type"], "state");
        assert!(state["data"]["server"].is_object());
        let round_trip: Event = serde_json::from_value(state).unwrap();
        assert_eq!(round_trip.topic(), Some(Topic::State));
        for topic in Topic::ALL {
            assert_eq!(Topic::ALL[topic.index()], topic);
            assert_eq!(serde_json::to_value(topic).unwrap(), topic.as_str());
        }
    }
}
