//! Private messages (`/api/conversations`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a message was received or sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    #[default]
    Out,
    In,
}

/// A private message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateMessage {
    /// The server's message id (received messages); unique per user.
    pub id: i32,
    pub timestamp: DateTime<Utc>,
    /// The other party.
    pub username: String,
    pub direction: MessageDirection,
    pub message: String,
    #[serde(default)]
    pub is_acknowledged: bool,
    /// Delivered again by the server after a reconnect.
    #[serde(default)]
    pub is_replayed: bool,
}

/// A conversation with one user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub username: String,
    /// Closed conversations are inactive until a new message arrives.
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default)]
    pub unacknowledged_count: u32,
    /// Recent messages, oldest first; `null` unless requested.
    #[serde(default)]
    pub messages: Option<Vec<PrivateMessage>>,
}

fn default_true() -> bool {
    true
}

/// `POST /api/conversations/{username}/messages` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessage {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_shape() {
        let conversation: Conversation = serde_json::from_value(serde_json::json!({
            "username": "bob",
            "is_active": true,
            "unacknowledged_count": 1,
            "messages": [{
                "id": 5,
                "timestamp": "2024-05-01T12:00:00Z",
                "username": "bob",
                "direction": "in",
                "message": "hi",
                "is_acknowledged": false,
                "is_replayed": false
            }]
        }))
        .unwrap();
        assert_eq!(
            conversation.messages.as_ref().unwrap()[0].direction,
            MessageDirection::In
        );
        let value = serde_json::to_value(&conversation).unwrap();
        assert_eq!(value["unacknowledged_count"], 1);
        assert_eq!(value["messages"][0]["direction"], "in");
    }
}
