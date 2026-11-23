//! The event history (`GET /api/events/history`): notable things that happened, kept in
//! `events.db`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{conversations::PrivateMessage, rooms::RoomMessage, transfers::Transfer};

/// What happened; selects the shape of [`Event::data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// [`FileComplete`].
    DownloadFileComplete,
    /// [`DirectoryComplete`].
    DownloadDirectoryComplete,
    /// [`FileComplete`].
    UploadFileComplete,
    /// [`PrivateMessage`].
    PrivateMessageReceived,
    /// [`RoomMessage`].
    RoomMessageReceived,
    /// `{}`.
    ServerConnected,
    /// [`ServerDisconnected`].
    ServerDisconnected,
}

impl EventType {
    pub const ALL: [Self; 7] = [
        Self::DownloadFileComplete,
        Self::DownloadDirectoryComplete,
        Self::UploadFileComplete,
        Self::PrivateMessageReceived,
        Self::RoomMessageReceived,
        Self::ServerConnected,
        Self::ServerDisconnected,
    ];

    /// The JSON name (`"download_file_complete"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DownloadFileComplete => "download_file_complete",
            Self::DownloadDirectoryComplete => "download_directory_complete",
            Self::UploadFileComplete => "upload_file_complete",
            Self::PrivateMessageReceived => "private_message_received",
            Self::RoomMessageReceived => "room_message_received",
            Self::ServerConnected => "server_connected",
            Self::ServerDisconnected => "server_disconnected",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// A recorded event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// UUID.
    pub id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: EventType,
    /// Depends on `type`, see [`EventType`].
    pub data: serde_json::Value,
}

impl Event {
    /// A new event with a fresh id.
    pub fn new(id: String, timestamp: DateTime<Utc>, data: EventData) -> Self {
        let (kind, data) = match data {
            EventData::DownloadFileComplete(data) => {
                (EventType::DownloadFileComplete, serde_json::to_value(data))
            }
            EventData::DownloadDirectoryComplete(data) => (
                EventType::DownloadDirectoryComplete,
                serde_json::to_value(data),
            ),
            EventData::UploadFileComplete(data) => {
                (EventType::UploadFileComplete, serde_json::to_value(data))
            }
            EventData::PrivateMessageReceived(data) => (
                EventType::PrivateMessageReceived,
                serde_json::to_value(data),
            ),
            EventData::RoomMessageReceived(data) => {
                (EventType::RoomMessageReceived, serde_json::to_value(data))
            }
            EventData::ServerConnected => (EventType::ServerConnected, Ok(serde_json::json!({}))),
            EventData::ServerDisconnected(data) => {
                (EventType::ServerDisconnected, serde_json::to_value(data))
            }
        };
        Self {
            id,
            timestamp,
            kind,
            data: data.unwrap_or_default(),
        }
    }
}

/// The typed payload of a new [`Event`].
#[derive(Debug, Clone, PartialEq)]
pub enum EventData {
    DownloadFileComplete(FileComplete),
    DownloadDirectoryComplete(DirectoryComplete),
    UploadFileComplete(FileComplete),
    PrivateMessageReceived(PrivateMessage),
    RoomMessageReceived(RoomMessage),
    ServerConnected,
    ServerDisconnected(ServerDisconnected),
}

/// `download_file_complete` and `upload_file_complete` data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileComplete {
    pub local_filename: String,
    pub remote_filename: String,
    pub transfer: Transfer,
}

/// `download_directory_complete` data: every file of a remote directory was downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryComplete {
    pub local_directory: String,
    pub remote_directory: String,
    pub username: String,
}

/// `server_disconnected` data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerDisconnected {
    pub message: Option<String>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json() {
        let event = Event::new(
            "x".into(),
            DateTime::<Utc>::default(),
            EventData::DownloadDirectoryComplete(DirectoryComplete {
                local_directory: "l".into(),
                remote_directory: "r".into(),
                username: "u".into(),
            }),
        );
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "id": "x",
                "timestamp": "1970-01-01T00:00:00Z",
                "type": "download_directory_complete",
                "data": {"local_directory": "l", "remote_directory": "r", "username": "u"}
            })
        );
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
        for kind in EventType::ALL {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
            assert_eq!(EventType::parse(kind.as_str()), Some(kind));
        }
    }
}
