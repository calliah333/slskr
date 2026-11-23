//! Application state (`GET /api/v0/application`, `state` event).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::server::ServerState;

/// Root application state (slskd `State`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: VersionState,
    pub pending_reconnect: bool,
    pub pending_restart: bool,
    pub server: ServerState,
    pub connection_watchdog: ServerConnectionWatchdogState,
    pub health: HealthState,
    pub user: UserState,
    pub distributed_network: DistributedNetworkState,
    pub shares: ShareState,
    pub rooms: Vec<String>,
    /// slskd `Users.User` records (`{username, group, statistics, status}`).
    pub users: Vec<crate::users::User>,
}

/// Runtime health metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HealthState {
    pub search: SearchHealthState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchHealthState {
    pub incoming: IncomingSearchHealthState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IncomingSearchHealthState {
    pub latency: f64,
    pub queue_depth: i32,
    pub drop_rate: f64,
}

/// Application version information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VersionState {
    pub full: String,
    pub current: String,
    pub latest: Option<String>,
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

/// Server reconnect watchdog state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerConnectionWatchdogState {
    pub is_enabled: bool,
    pub is_attempting_connection: bool,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// Logged-in user state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserState {
    pub username: Option<String>,
    pub privileges: UserPrivilegeState,
    pub statistics: UserStatisticsState,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserPrivilegeState {
    pub is_privileged: bool,
    pub privileges_remaining: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserStatisticsState {
    pub average_speed: i32,
    pub directory_count: i32,
    pub file_count: i32,
    pub upload_count: i64,
}

/// Distributed network state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
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
#[serde(rename_all = "camelCase", default)]
pub struct ShareState {
    pub scan_pending: bool,
    pub scanning: bool,
    pub ready: bool,
    pub faulted: bool,
    pub cancelled: bool,
    pub scan_progress: f64,
    pub hosts: Option<Vec<String>>,
    pub directories: i32,
    pub files: i32,
}

/// Shared file cache state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SharedFileCacheState {
    pub filling: bool,
    pub filled: bool,
    pub faulted: bool,
    pub cancelled: bool,
    pub fill_progress: f64,
    pub directories: i32,
    pub files: i32,
    pub excluded_directories: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_field_names() {
        let v = serde_json::to_value(State::default()).unwrap();
        for key in [
            "version",
            "pendingReconnect",
            "pendingRestart",
            "server",
            "connectionWatchdog",
            "health",
            "user",
            "distributedNetwork",
            "shares",
            "rooms",
            "users",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["version"]["isUpdateAvailable"], serde_json::Value::Null);
        assert!(v["shares"].get("scanProgress").is_some());
        assert!(v["user"]["privileges"].get("privilegesRemaining").is_some());
        assert!(v["distributedNetwork"].get("canAcceptChildren").is_some());
        assert_eq!(v["server"]["state"], "None");
        assert!(
            v["health"]["search"]["incoming"]
                .get("queueDepth")
                .is_some()
        );
    }

    #[test]
    fn state_deserializes_partial() {
        let s: State = serde_json::from_str(
            r#"{"pendingRestart":true,"server":{"state":"Connected, LoggedIn"}}"#,
        )
        .unwrap();
        assert!(s.pending_restart);
        assert_eq!(s.server.state.bits(), 6);
    }
}
