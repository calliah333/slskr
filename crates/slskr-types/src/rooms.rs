//! Chat rooms (`/api/rooms`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::users::UserPresence;

/// A joined room (`GET /api/rooms/joined/{room}`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Room {
    pub name: String,
    pub is_private: bool,
    /// Private rooms only.
    pub owner: Option<String>,
    /// Private rooms only.
    pub operators: Vec<String>,
    pub users: Vec<RoomUser>,
    pub messages: Vec<RoomMessage>,
}

/// A message in a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomMessage {
    pub timestamp: DateTime<Utc>,
    pub room: String,
    pub username: String,
    pub message: String,
    /// Sent by the logged-in user.
    #[serde(default)]
    pub is_self: bool,
}

/// A room on the server (`GET /api/rooms`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoomInfo {
    pub name: String,
    pub user_count: u32,
    pub is_private: bool,
    pub is_owned: bool,
    pub is_moderated: bool,
}

/// A member of a joined room (`GET /api/rooms/joined/{room}/users`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoomUser {
    pub username: String,
    pub status: UserPresence,
    /// Bytes per second.
    pub average_speed: u32,
    pub upload_count: u64,
    pub file_count: u32,
    pub directory_count: u32,
    /// ISO 3166 code, when known.
    pub country_code: Option<String>,
    pub slots_free: Option<u32>,
    /// The logged-in user.
    pub is_self: bool,
}

/// `POST /api/rooms/joined` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRoom {
    pub name: String,
}

/// `PUT /api/rooms/joined/{room}/ticker` body and `GET` response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ticker {
    /// Empty when no ticker is set.
    pub ticker: String,
}

/// `POST /api/rooms/joined/{room}/members` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddMember {
    pub username: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_shape() {
        let info = RoomInfo {
            name: "x".into(),
            user_count: 4,
            is_private: true,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"name":"x","user_count":4,"is_private":true,"is_owned":false,"is_moderated":false}"#
        );
        let user: RoomUser =
            serde_json::from_str(r#"{"username":"me","status":"online","is_self":true}"#).unwrap();
        assert!(user.is_self);
        assert_eq!(user.status, UserPresence::Online);
    }
}
