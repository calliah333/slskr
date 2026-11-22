//! Server-related model types (users, rooms, interests, server info).

use std::time::{Duration, SystemTime};

/// Online presence of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum UserPresence {
    #[default]
    Offline = 0,
    Away = 1,
    Online = 2,
}

impl UserPresence {
    /// Converts a wire value; unknown values map to `Offline`.
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Away,
            2 => Self::Online,
            _ => Self::Offline,
        }
    }

    /// Wire value.
    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// Status of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserStatus {
    pub username: String,
    pub presence: UserPresence,
    pub is_privileged: bool,
}

/// Statistics of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserStatistics {
    pub username: String,
    pub average_speed: u32,
    pub upload_count: u64,
    pub file_count: u32,
    pub directory_count: u32,
}

/// User data as reported in room and watch messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserData {
    pub username: String,
    pub status: UserPresence,
    pub average_speed: u32,
    pub upload_count: u64,
    pub file_count: u32,
    pub directory_count: u32,
    pub country_code: Option<String>,
    pub slots_free: Option<u32>,
}

/// Data of a joined chat room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomData {
    pub name: String,
    pub users: Vec<UserData>,
    pub is_private: bool,
    pub owner: Option<String>,
    pub operators: Vec<String>,
}

/// Summary information about a chat room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub name: String,
    pub user_count: u32,
    pub users: Vec<String>,
}

impl RoomInfo {
    /// Room info with only a user count.
    pub fn with_count(name: impl Into<String>, user_count: u32) -> Self {
        Self {
            name: name.into(),
            user_count,
            users: Vec::new(),
        }
    }

    /// Room info with a user list (count derived).
    pub fn with_users(name: impl Into<String>, users: Vec<String>) -> Self {
        Self {
            name: name.into(),
            user_count: users.len() as u32,
            users,
        }
    }
}

/// The server's list of chat rooms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomList {
    pub public: Vec<RoomInfo>,
    pub private: Vec<RoomInfo>,
    pub owned: Vec<RoomInfo>,
    /// Rooms we moderate (operator of); user counts are not provided by the server.
    pub moderated_rooms: Vec<RoomInfo>,
}

/// A chat room ticker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTicker {
    pub username: String,
    pub message: String,
}

/// An interest recommendation (item and score/count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub item: String,
    pub score: i32,
}

/// A user's liked and hated interests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInterests {
    pub username: String,
    pub liked: Vec<String>,
    pub hated: Vec<String>,
}

/// Server-provided parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerInfo {
    pub parent_min_speed: Option<u32>,
    pub parent_speed_ratio: Option<u32>,
    pub wishlist_interval: Option<Duration>,
    pub is_supporter: Option<bool>,
}

/// A private message received from a user (or the server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateMessage {
    pub id: u32,
    pub timestamp: SystemTime,
    pub username: String,
    pub message: String,
    /// True when sent by the server administrator (`server` user).
    pub is_admin: bool,
    /// True when the message was queued while offline and is being replayed.
    pub replayed: bool,
}

/// A chat room message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomMessage {
    pub room: String,
    pub username: String,
    pub message: String,
}

/// Notification that privileges were granted to a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeNotification {
    /// Notification id to acknowledge, if any.
    pub id: Option<u32>,
    pub username: String,
}
