//! Soulseek server connection (`GET /api/server`, `State.server`).

use serde::{Deserialize, Serialize};

/// The server connection's state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    /// Connected, not logged in.
    Connected,
    /// Connected, waiting for the login response.
    LoggingIn,
    LoggedIn,
    Disconnecting,
}

impl ConnectionState {
    /// The JSON name (`"logged_in"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::LoggingIn => "logging_in",
            Self::LoggedIn => "logged_in",
            Self::Disconnecting => "disconnecting",
        }
    }

    /// Whether a connection to the server is open (also while logging in).
    pub const fn is_connected(self) -> bool {
        matches!(self, Self::Connected | Self::LoggingIn | Self::LoggedIn)
    }

    pub const fn is_logged_in(self) -> bool {
        matches!(self, Self::LoggedIn)
    }

    /// Connecting, logging in or disconnecting.
    pub const fn is_transitioning(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::LoggingIn | Self::Disconnecting
        )
    }
}

/// The server connection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerState {
    /// Configured server, `host:port`.
    pub address: Option<String>,
    /// Resolved server address, `ip:port`, while connected.
    pub endpoint: Option<String>,
    pub state: ConnectionState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_state_shape() {
        let s = ServerState {
            address: Some("server.slsknet.org:2242".into()),
            endpoint: Some("1.2.3.4:2242".into()),
            state: ConnectionState::LoggedIn,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "address": "server.slsknet.org:2242",
                "endpoint": "1.2.3.4:2242",
                "state": "logged_in"
            })
        );
        assert!(ConnectionState::LoggingIn.is_connected());
        assert!(ConnectionState::LoggingIn.is_transitioning());
        assert!(!ConnectionState::Connecting.is_connected());
    }
}
