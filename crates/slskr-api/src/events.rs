//! Events (`GET /api/v0/events`, `POST /api/v0/events/{type}`), webhook/script payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Event type; serialized by name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    #[default]
    None,
    Any,
    DownloadFileComplete,
    DownloadDirectoryComplete,
    UploadFileComplete,
    PrivateMessageReceived,
    RoomMessageReceived,
    SoulseekClientConnected,
    SoulseekClientDisconnected,
    Noop,
}

impl EventType {
    /// All defined variants, in declaration order.
    pub const ALL: [EventType; 10] = [
        Self::None,
        Self::Any,
        Self::DownloadFileComplete,
        Self::DownloadDirectoryComplete,
        Self::UploadFileComplete,
        Self::PrivateMessageReceived,
        Self::RoomMessageReceived,
        Self::SoulseekClientConnected,
        Self::SoulseekClientDisconnected,
        Self::Noop,
    ];

    /// Numeric value as stored by slskd.
    pub const fn value(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Any => 1,
            Self::DownloadFileComplete => 2,
            Self::DownloadDirectoryComplete => 3,
            Self::UploadFileComplete => 4,
            Self::PrivateMessageReceived => 6,
            Self::RoomMessageReceived => 7,
            Self::SoulseekClientConnected => 50,
            Self::SoulseekClientDisconnected => 51,
            Self::Noop => i32::MAX,
        }
    }

    /// Variant name (`"DownloadFileComplete"`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Any => "Any",
            Self::DownloadFileComplete => "DownloadFileComplete",
            Self::DownloadDirectoryComplete => "DownloadDirectoryComplete",
            Self::UploadFileComplete => "UploadFileComplete",
            Self::PrivateMessageReceived => "PrivateMessageReceived",
            Self::RoomMessageReceived => "RoomMessageReceived",
            Self::SoulseekClientConnected => "SoulseekClientConnected",
            Self::SoulseekClientDisconnected => "SoulseekClientDisconnected",
            Self::Noop => "Noop",
        }
    }

    /// Case-insensitive name parse (`Enum.TryParse(ignoreCase: true)`).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.name().eq_ignore_ascii_case(s))
    }

    /// Inverse of [`EventType::value`].
    pub fn from_value(v: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.value() == v)
    }
}

/// Persisted event (`EventRecord`). `data` is the event serialized to a JSON string,
/// minus `timestamp`, `type`, `id` and `data`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EventRecord {
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: EventType,
    pub data: Option<String>,
    /// GUID, lowercase hyphenated.
    pub id: String,
}

/// Fields common to every event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EventHeader {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: EventType,
    pub version: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SoulseekClientConnectedEvent {
    #[serde(flatten)]
    pub header: EventHeader,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SoulseekClientDisconnectedEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    pub message: Option<String>,
    /// Serialized .NET exception, if any.
    pub exception: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DownloadFileCompleteEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    pub local_filename: String,
    pub remote_filename: String,
    /// slskd `Transfer` record.
    pub transfer: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DownloadDirectoryCompleteEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    pub local_directory_name: String,
    pub remote_directory_name: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UploadFileCompleteEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    pub local_filename: String,
    pub remote_filename: String,
    /// slskd `Transfer` record.
    pub transfer: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrivateMessageReceivedEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    /// slskd `PrivateMessage` record.
    pub message: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RoomMessageReceivedEvent {
    #[serde(flatten)]
    pub header: EventHeader,
    /// slskd `RoomMessage` record.
    pub message: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoopEvent {
    #[serde(flatten)]
    pub header: EventHeader,
}

/// Any concrete event; serializes as the flat slskd event object.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Event {
    SoulseekClientConnected(SoulseekClientConnectedEvent),
    SoulseekClientDisconnected(SoulseekClientDisconnectedEvent),
    DownloadFileComplete(DownloadFileCompleteEvent),
    DownloadDirectoryComplete(DownloadDirectoryCompleteEvent),
    UploadFileComplete(UploadFileCompleteEvent),
    PrivateMessageReceived(PrivateMessageReceivedEvent),
    RoomMessageReceived(RoomMessageReceivedEvent),
    Noop(NoopEvent),
}

impl Event {
    pub fn header(&self) -> &EventHeader {
        match self {
            Self::SoulseekClientConnected(e) => &e.header,
            Self::SoulseekClientDisconnected(e) => &e.header,
            Self::DownloadFileComplete(e) => &e.header,
            Self::DownloadDirectoryComplete(e) => &e.header,
            Self::UploadFileComplete(e) => &e.header,
            Self::PrivateMessageReceived(e) => &e.header,
            Self::RoomMessageReceived(e) => &e.header,
            Self::Noop(e) => &e.header,
        }
    }

    pub fn kind(&self) -> EventType {
        self.header().kind
    }

    /// Converts to a persisted record, stripping `timestamp`/`type`/`id`/`data` from `data` like slskd.
    pub fn to_record(&self) -> EventRecord {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = value.as_object_mut() {
            for key in ["timestamp", "type", "id", "data"] {
                obj.remove(key);
            }
        }
        let header = self.header();
        EventRecord {
            timestamp: header.timestamp,
            kind: header.kind,
            data: Some(value.to_string()),
            id: header.id.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let kind: EventType = value
            .get("type")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        let r = match kind {
            EventType::SoulseekClientConnected => {
                serde_json::from_value(value).map(Self::SoulseekClientConnected)
            }
            EventType::SoulseekClientDisconnected => {
                serde_json::from_value(value).map(Self::SoulseekClientDisconnected)
            }
            EventType::DownloadFileComplete => {
                serde_json::from_value(value).map(Self::DownloadFileComplete)
            }
            EventType::DownloadDirectoryComplete => {
                serde_json::from_value(value).map(Self::DownloadDirectoryComplete)
            }
            EventType::UploadFileComplete => {
                serde_json::from_value(value).map(Self::UploadFileComplete)
            }
            EventType::PrivateMessageReceived => {
                serde_json::from_value(value).map(Self::PrivateMessageReceived)
            }
            EventType::RoomMessageReceived => {
                serde_json::from_value(value).map(Self::RoomMessageReceived)
            }
            EventType::Noop => serde_json::from_value(value).map(Self::Noop),
            other => {
                return Err(D::Error::custom(format!(
                    "event type {} is not a concrete event",
                    other.name()
                )));
            }
        };
        r.map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json() {
        let e = Event::DownloadDirectoryComplete(DownloadDirectoryCompleteEvent {
            header: EventHeader {
                id: "x".into(),
                kind: EventType::DownloadDirectoryComplete,
                ..Default::default()
            },
            local_directory_name: "l".into(),
            remote_directory_name: "r".into(),
            username: Some("u".into()),
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "DownloadDirectoryComplete");
        assert_eq!(v["localDirectoryName"], "l");
        assert_eq!(v["version"], 0);
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);

        let rec = e.to_record();
        let data: serde_json::Value = serde_json::from_str(rec.data.as_deref().unwrap()).unwrap();
        assert!(data.get("id").is_none());
        assert_eq!(data["remoteDirectoryName"], "r");
        let rv = serde_json::to_value(&rec).unwrap();
        assert_eq!(rv["type"], "DownloadDirectoryComplete");
    }

    #[test]
    fn parse_type() {
        assert_eq!(EventType::parse("noop"), Some(EventType::Noop));
        assert_eq!(EventType::Noop.value(), i32::MAX);
    }
}
