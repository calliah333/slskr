//! Sign-in (`/api/session`).
//!
//! `POST /api/session` signs in with [`LoginRequest`] and sets the `slskr_session` cookie;
//! `GET` returns the caller's [`Session`]; `DELETE` signs out. `GET /api/auth` says whether
//! authentication is enabled at all, and whether first-run setup has to happen first.

use serde::{Deserialize, Serialize};

pub use crate::config::ApiKeyRole as Role;

/// `POST /api/session` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// The authenticated caller (`GET /api/session`, and the `POST` response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Web username, API key name, or `Anonymous` when authentication is disabled.
    pub username: String,
    pub role: Role,
}

/// `GET /api/auth`: available without signing in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInfo {
    /// False when `web.auth = false`: every request acts as an administrator.
    pub enabled: bool,
    /// True until a web password is set: nobody can sign in, and the API answers 503 until
    /// first-run setup (`/api/setup`) is completed.
    #[serde(default)]
    pub setup_required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_fields() {
        let v = serde_json::to_value(Session {
            username: "admin".into(),
            role: Role::Admin,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({"username": "admin", "role": "admin"}));
    }
}
