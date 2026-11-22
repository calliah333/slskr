//! Chat room API models (`/api/v0/rooms`).
//!
//! `GET /rooms/joined` returns the joined room names (`Vec<String>`). Request
//! bodies of `POST /rooms/joined` (room name), `.../messages` (message),
//! `.../ticker` (ticker text) and `.../members` (username) are bare JSON strings.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transfers::time;
use crate::users::UserPresence;

/// slskd `Messaging.API.RoomResponse`: a joined room.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub name: String,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operators: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub users: Vec<UserData>,
    #[serde(default)]
    pub messages: Vec<RoomMessage>,
}

/// slskd `Messaging.API.RoomMessageResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMessage {
    #[serde(with = "time::datetime")]
    pub timestamp: DateTime<Utc>,
    pub username: String,
    pub message: String,
    pub room_name: String,
    /// True when sent by the logged-in user.
    #[serde(rename = "self", default, skip_serializing_if = "Option::is_none")]
    pub self_: Option<bool>,
}

/// slskd `Messaging.API.RoomInfoResponse`: an entry of `GET /rooms/available`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomInfo {
    pub name: String,
    #[serde(default)]
    pub user_count: u32,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_owned: bool,
    #[serde(default)]
    pub is_moderated: bool,
}

/// slskd `UserDataResponse`: a room member, as in `GET /rooms/joined/{room}/users`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserData {
    #[serde(default)]
    pub average_speed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    #[serde(default)]
    pub directory_count: u32,
    #[serde(default)]
    pub file_count: u32,
    /// True for the logged-in user.
    #[serde(rename = "self", default, skip_serializing_if = "Option::is_none")]
    pub self_: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots_free: Option<u32>,
    #[serde(default)]
    pub status: UserPresence,
    #[serde(default)]
    pub upload_count: u64,
    pub username: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_json_shape() {
        let json = r#"{
            "name": "indie",
            "isPrivate": false,
            "users": [{
                "averageSpeed": 100,
                "countryCode": "US",
                "directoryCount": 2,
                "fileCount": 20,
                "self": true,
                "status": "Online",
                "uploadCount": 3,
                "username": "me"
            }],
            "messages": [{
                "timestamp": "2024-05-01T12:00:00Z",
                "username": "me",
                "message": "hello",
                "roomName": "indie",
                "self": true
            }]
        }"#;
        let room: Room = serde_json::from_str(json).unwrap();
        assert_eq!(room.users[0].self_, Some(true));
        assert_eq!(room.users[0].status, UserPresence::Online);
        assert_eq!(room.messages[0].room_name, "indie");

        let value = serde_json::to_value(&room).unwrap();
        assert_eq!(value["users"][0]["self"], true);
        assert_eq!(value["messages"][0]["roomName"], "indie");
        assert_eq!(value["messages"][0]["self"], true);
        assert!(value.get("owner").is_none());
        assert!(value["users"][0].get("slotsFree").is_none());

        let info = RoomInfo {
            name: "x".into(),
            user_count: 4,
            is_private: true,
            is_owned: false,
            is_moderated: false,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"name":"x","userCount":4,"isPrivate":true,"isOwned":false,"isModerated":false}"#
        );
    }
}
