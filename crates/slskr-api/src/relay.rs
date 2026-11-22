//! Relay DTOs (RelayController, RelayHub, `State.relay`).

use serde::{Deserialize, Serialize};

/// Relay operating mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelayMode {
    #[default]
    Controller,
    Agent,
    Debug,
}

impl RelayMode {
    /// Parses the options string (`"controller"`, `"agent"`, `"debug"`; case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "controller" => Some(Self::Controller),
            "agent" => Some(Self::Agent),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// Relay client (agent → controller) connection state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelayClientState {
    #[default]
    Disconnected,
    Connected,
    Connecting,
    Reconnecting,
}

/// A registered relay agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Agent {
    pub name: Option<String>,
    #[serde(rename = "ipAddress")]
    pub ip_address: Option<String>,
}

/// `State.relay`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayState {
    pub mode: RelayMode,
    pub controller: RelayControllerState,
    pub agents: Vec<Agent>,
}

/// `State.relay.controller`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayControllerState {
    pub address: Option<String>,
    pub state: RelayClientState,
}

/// Hub method names the controller invokes on agents (`IRelayHub`).
pub mod hub_client_methods {
    pub const CHALLENGE: &str = "Challenge";
    pub const REQUEST_FILE_INFO: &str = "RequestFileInfo";
    pub const REQUEST_FILE_UPLOAD: &str = "RequestFileUpload";
    pub const NOTIFY_FILE_DOWNLOAD_COMPLETED: &str = "NotifyFileDownloadCompleted";
}

/// Hub method names agents invoke on the controller (`RelayHub`).
pub mod hub_server_methods {
    pub const LOGIN: &str = "Login";
    pub const BEGIN_SHARE_UPLOAD: &str = "BeginShareUpload";
    pub const NOTIFY_FILE_UPLOAD_FAILED: &str = "NotifyFileUploadFailed";
    pub const RETURN_FILE_INFO: &str = "ReturnFileInfo";
}

/// HTTP headers used by relay file/share transfers.
pub mod headers {
    pub const AGENT: &str = "X-Relay-Agent";
    pub const CREDENTIAL: &str = "X-Relay-Credential";
    pub const FILENAME_BASE64: &str = "X-Relay-Filename-Base64";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_state_json() {
        let v = serde_json::to_value(RelayState {
            agents: vec![Agent {
                name: Some("a".into()),
                ip_address: Some("1.2.3.4".into()),
            }],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v["mode"], "Controller");
        assert_eq!(v["controller"]["state"], "Disconnected");
        assert_eq!(v["agents"][0]["ipAddress"], "1.2.3.4");
    }
}
