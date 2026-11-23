//! Peers (`/api/users/{username}/...`) and the shared file and directory shapes used by
//! browse, search and share listings.

use serde::{Deserialize, Serialize};

/// A user's online status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserPresence {
    #[default]
    Offline,
    Away,
    Online,
}

/// A shared file. `filename` is the full remote path in browse and search results and the
/// file name within its [`Directory`] in directory listings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct File {
    pub filename: String,
    pub size: u64,
    /// Lowercase, without the dot; may be empty.
    pub extension: String,
    /// kbit/s.
    pub bit_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    /// Hz.
    pub sample_rate: Option<u32>,
    pub duration_ms: Option<u64>,
    pub is_variable_bit_rate: Option<bool>,
}

/// A shared directory and the files directly in it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Directory {
    /// Full remote path.
    pub name: String,
    pub files: Vec<File>,
}

/// A user's shares (`GET /api/users/{username}/browse`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowseResponse {
    pub directories: Vec<Directory>,
    /// Directories the user shares only with some users; their files can not be downloaded.
    pub locked_directories: Vec<Directory>,
}

/// Progress of a running browse (`GET /api/users/{username}/browse/status`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowseStatus {
    pub username: String,
    pub size: u64,
    pub bytes_transferred: u64,
    pub bytes_remaining: u64,
    pub percent_complete: f64,
}

/// A user's network address (`GET /api/users/{username}/endpoint`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
}

impl Endpoint {
    pub fn from_socket_addr(addr: std::net::SocketAddr) -> Self {
        Self {
            address: addr.ip().to_string(),
            port: addr.port(),
        }
    }
}

/// A user's self description (`GET /api/users/{username}/info`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub description: Option<String>,
    pub has_free_upload_slot: bool,
    /// Base64-encoded image bytes.
    pub picture: Option<String>,
    pub queue_length: u32,
    pub upload_slots: u32,
}

/// `GET /api/users/{username}/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub presence: UserPresence,
    pub is_privileged: bool,
}

/// `GET /api/users/{username}/statistics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Statistics {
    /// Bytes per second.
    pub average_speed: u32,
    pub directory_count: u32,
    pub file_count: u32,
    pub upload_count: u64,
}

/// A watched user (`State.users`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    pub username: String,
    pub group: Option<String>,
    pub statistics: Option<Statistics>,
    pub status: Option<Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_and_endpoint_shapes() {
        let file = File {
            filename: "@@music\\a.mp3".into(),
            size: 1234,
            extension: "mp3".into(),
            bit_rate: Some(320),
            is_variable_bit_rate: Some(false),
            ..Default::default()
        };
        let value = serde_json::to_value(&file).unwrap();
        assert_eq!(value["bit_rate"], 320);
        assert_eq!(value["is_variable_bit_rate"], false);
        assert!(value["sample_rate"].is_null());
        assert_eq!(serde_json::from_value::<File>(value).unwrap(), file);

        let endpoint = Endpoint::from_socket_addr("1.2.3.4:2234".parse().unwrap());
        assert_eq!(
            serde_json::to_string(&endpoint).unwrap(),
            r#"{"address":"1.2.3.4","port":2234}"#
        );
        let status: Status =
            serde_json::from_str(r#"{"is_privileged":true,"presence":"away"}"#).unwrap();
        assert_eq!(status.presence, UserPresence::Away);
    }
}
