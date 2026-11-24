//! Application state (`GET /api/application`, `state` event).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::server::ServerState;

/// Root application state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: VersionState,
    /// A changed Soulseek setting applies on the next reconnect.
    pub pending_reconnect: bool,
    /// A changed setting applies on the next restart.
    pub pending_restart: bool,
    pub server: ServerState,
    pub connection_watchdog: ConnectionWatchdogState,
    pub user: UserState,
    pub distributed_network: DistributedNetworkState,
    pub shares: ShareState,
    /// Joined room names.
    pub rooms: Vec<String>,
    /// Users whose status is being watched.
    pub users: Vec<crate::users::User>,
}

/// Version information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VersionState {
    pub full: String,
    pub current: String,
    /// Latest released version, once checked.
    pub latest: Option<String>,
    /// Whether `latest` is newer than `current`, once checked.
    pub is_update_available: Option<bool>,
    pub is_canary: bool,
    pub is_development: bool,
}

impl Default for VersionState {
    fn default() -> Self {
        Self {
            full: env!("CARGO_PKG_VERSION").to_string(),
            current: env!("CARGO_PKG_VERSION").to_string(),
            latest: None,
            is_update_available: None,
            is_canary: false,
            is_development: false,
        }
    }
}

/// Automatic server reconnection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionWatchdogState {
    pub is_enabled: bool,
    pub is_attempting_connection: bool,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// The logged-in Soulseek user.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserState {
    pub username: Option<String>,
    pub privileges: UserPrivilegeState,
    pub statistics: UserStatisticsState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPrivilegeState {
    pub is_privileged: bool,
    /// Seconds of privileges left, as reported by the server.
    pub privileges_remaining: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserStatisticsState {
    /// Bytes per second.
    pub average_speed: u32,
    pub directory_count: u32,
    pub file_count: u32,
    pub upload_count: u64,
}

/// Distributed (search) network membership.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DistributedNetworkState {
    pub branch_level: i32,
    pub branch_root: Option<String>,
    pub can_accept_children: bool,
    pub child_limit: i32,
    pub children: Vec<String>,
    pub has_parent: bool,
    pub is_branch_root: bool,
    pub parent: Option<String>,
}

/// Share scan state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareState {
    pub scan_pending: bool,
    pub scanning: bool,
    pub ready: bool,
    pub faulted: bool,
    pub cancelled: bool,
    /// 0.0 to 1.0.
    pub scan_progress: f64,
    /// Number of files indexed so far during the current scan.
    pub scan_files: u32,
    pub directories: u32,
    pub files: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_field_names() {
        let v = serde_json::to_value(State::default()).unwrap();
        for key in [
            "version",
            "pending_reconnect",
            "pending_restart",
            "server",
            "connection_watchdog",
            "user",
            "distributed_network",
            "shares",
            "rooms",
            "users",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["version"]["is_update_available"], serde_json::Value::Null);
        assert!(v["shares"].get("scan_progress").is_some());
        assert_eq!(v["server"]["state"], "disconnected");
    }

    #[test]
    fn state_deserializes_partial() {
        let s: State =
            serde_json::from_str(r#"{"pending_restart":true,"server":{"state":"logged_in"}}"#)
                .unwrap();
        assert!(s.pending_restart);
        assert!(s.server.state.is_logged_in());
    }
}
