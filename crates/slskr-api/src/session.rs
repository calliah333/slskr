//! Session (sign-in) DTOs.
//!
//! `POST /api/v0/session` signs in with [`LoginRequest`] and sets the `slskr_session`
//! cookie; `GET` returns the caller's [`Session`]; `DELETE` signs out.

use serde::{Deserialize, Serialize};

/// `POST /api/v0/session` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoginRequest {
    pub username: Option<String>,
    pub password: Option<String>,
}

/// The authenticated caller (`GET /api/v0/session`, and the `POST` response).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Session {
    /// Web username, API key name, or `Anonymous` when authentication is disabled.
    pub username: String,
    /// `ReadOnly`, `ReadWrite` or `Administrator`.
    pub role: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_fields() {
        let v = serde_json::to_value(Session {
            username: "admin".into(),
            role: "Administrator".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"username": "admin", "role": "Administrator"})
        );
    }
}
