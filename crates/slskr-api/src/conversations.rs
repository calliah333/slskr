//! Private messaging API models (`/api/v0/conversations`).
//!
//! `POST /conversations/{username}` takes the message as a bare JSON string.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transfers::time;

/// slskd `Messaging.MessageDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum MessageDirection {
    #[default]
    Out,
    In,
}

/// slskd `Messaging.PrivateMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivateMessage {
    #[serde(with = "time::datetime")]
    pub timestamp: DateTime<Utc>,
    pub id: i32,
    pub username: String,
    pub direction: MessageDirection,
    pub message: String,
    #[serde(default)]
    pub is_acknowledged: bool,
    #[serde(default)]
    pub was_replayed: bool,
}

/// slskd `Messaging.Conversation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub username: String,
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default)]
    pub un_acknowledged_message_count: u32,
    /// Derived: `unAcknowledgedMessageCount > 0`.
    #[serde(default)]
    pub has_un_acknowledged_messages: bool,
    /// Present when messages were requested (`includeMessages=true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<PrivateMessage>>,
}

fn default_true() -> bool {
    true
}

impl Conversation {
    /// Sets the unacknowledged count and keeps the derived flag consistent.
    pub fn set_un_acknowledged_message_count(&mut self, count: u32) {
        self.un_acknowledged_message_count = count;
        self.has_un_acknowledged_messages = count > 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_json_shape() {
        let json = r#"{
            "username": "bob",
            "isActive": true,
            "unAcknowledgedMessageCount": 1,
            "hasUnAcknowledgedMessages": true,
            "messages": [{
                "timestamp": "2024-05-01T12:00:00.1234567",
                "id": 5,
                "username": "bob",
                "direction": "In",
                "message": "hi",
                "isAcknowledged": false,
                "wasReplayed": false
            }]
        }"#;
        let conversation: Conversation = serde_json::from_str(json).unwrap();
        let messages = conversation.messages.as_ref().unwrap();
        assert_eq!(messages[0].direction, MessageDirection::In);

        let value = serde_json::to_value(&conversation).unwrap();
        assert_eq!(value["unAcknowledgedMessageCount"], 1);
        assert_eq!(value["hasUnAcknowledgedMessages"], true);
        assert_eq!(value["messages"][0]["isAcknowledged"], false);
        assert_eq!(value["messages"][0]["direction"], "In");

        let mut bare = conversation.clone();
        bare.messages = None;
        bare.set_un_acknowledged_message_count(0);
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("messages").is_none());
        assert_eq!(value["hasUnAcknowledgedMessages"], false);
    }
}
