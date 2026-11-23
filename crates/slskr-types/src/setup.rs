//! First-run setup (`/api/setup`) and the helpers the setup wizard uses before anyone can
//! sign in: the Soulseek login test (`/api/soulseek/test-login`) and the directory picker
//! (`/api/fs/dirs`).
//!
//! Setup is *required* while web authentication is enabled and no web password exists in
//! any configuration layer. Until it is completed, every other API route except
//! `GET /api/auth` answers `503 {"error": "setup required"}`. Callers that are not on the
//! daemon's own machine prove they can read its log with the one-time setup code, sent as
//! `code` in [`SetupRequest`] or in the [`SETUP_CODE_HEADER`] header.

use serde::{Deserialize, Serialize};

/// Header carrying the one-time setup code on setup routes other than `POST /api/setup`.
pub const SETUP_CODE_HEADER: &str = "X-Setup-Code";

/// Error message of the `503` answered while setup is required.
pub const SETUP_REQUIRED: &str = "setup required";

/// `GET /api/setup`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    /// True until a web password is configured (always false with `web.auth = false`).
    pub required: bool,
    /// True when this caller must send the setup code (it is not on the daemon's machine).
    pub code_required: bool,
    /// `web.username` is set by `slskr.toml`, the environment or the command line.
    pub web_username_pinned: bool,
    /// `web.password` is set by `slskr.toml`, the environment or the command line.
    pub web_password_pinned: bool,
    /// Soulseek username and password are both set.
    pub soulseek_configured: bool,
    /// `soulseek.username` or `soulseek.password` is pinned.
    pub soulseek_pinned: bool,
}

/// `POST /api/setup` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    /// The setup code from the daemon's log; optional from the daemon's own machine.
    #[serde(default)]
    pub code: Option<String>,
    pub web: Credentials,
    #[serde(default)]
    pub soulseek: Option<Credentials>,
    /// Absolute paths of directories to share.
    #[serde(default)]
    pub shares: Option<Vec<String>>,
}

/// A username and password (`SetupRequest.web`, `.soulseek`, and the login test).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Why a Soulseek login test failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginTestFailure {
    /// The server refused the password (`INVALIDPASS`).
    InvalidPassword,
    /// The server refused the username (`INVALIDUSERNAME`).
    InvalidUsername,
    /// The server is full (`SVRFULL`).
    ServerFull,
    /// The server only admits invited users (`SVRPRIVATE`).
    ServerPrivate,
    /// The server refused the client version (`INVALIDVERSION`).
    InvalidVersion,
    /// Another rejection; see the message.
    Rejected,
    /// The server could not be resolved or connected to.
    Unreachable,
    /// The server did not answer in time.
    Timeout,
    /// Any other failure; see the message.
    Error,
}

/// `POST /api/soulseek/test-login` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginTestResult {
    pub ok: bool,
    /// Null when `ok`.
    pub reason: Option<LoginTestFailure>,
    /// The server's or the connection's message; null when `ok`.
    pub message: Option<String>,
}

/// `GET /api/fs/dirs` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    /// The listed absolute path.
    pub path: String,
    /// Its parent; null at the filesystem root.
    pub parent: Option<String>,
    /// Subdirectories, sorted by name.
    pub dirs: Vec<DirectoryEntry>,
    /// True when the listing was cut off at its limit.
    pub truncated: bool,
}

/// A subdirectory in a [`DirectoryListing`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let request: SetupRequest = serde_json::from_value(serde_json::json!({
            "web": {"username": "admin", "password": "long enough"}
        }))
        .unwrap();
        assert_eq!(request.code, None);
        assert!(request.soulseek.is_none() && request.shares.is_none());
        assert!(!format!("{request:?}").contains("long enough"));
        let failed = LoginTestResult {
            ok: false,
            reason: Some(LoginTestFailure::InvalidPassword),
            message: Some("INVALIDPASS".into()),
        };
        assert_eq!(
            serde_json::to_value(failed).unwrap(),
            serde_json::json!({"ok": false, "reason": "invalid_password", "message": "INVALIDPASS"})
        );
    }
}
