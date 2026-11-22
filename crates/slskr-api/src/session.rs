//! Session (login) DTOs.

use serde::{Deserialize, Serialize};

/// `POST /api/v0/session` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoginRequest {
    pub username: Option<String>,
    pub password: Option<String>,
}

/// JWT issued on login. Times are unix seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenResponse {
    pub expires: i64,
    pub issued: i64,
    pub name: Option<String>,
    pub not_before: i64,
    pub token: String,
    /// Always `"Bearer"`.
    pub token_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_fields() {
        let v = serde_json::to_value(TokenResponse {
            token_type: "Bearer".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(v.get("notBefore").is_some());
        assert_eq!(v["tokenType"], "Bearer");
    }
}
