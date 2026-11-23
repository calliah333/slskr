//! `/api/v0/settings`: the merged configuration as the web UI edits it.
//!
//! Settings come from layers, lowest precedence first: built-in defaults, the app-managed
//! `settings.json` (written from this API), `slskr.toml`, `SLSKR_*` environment variables and
//! command-line flags. A key set by the file, the environment or the command line is
//! *pinned*: it is listed in [`Settings::pinned`] and `PATCH` refuses to change it.
//!
//! Keys are dotted paths into [`Config`] (`web.listen`, `uploads.limits.daily_files`,
//! `groups.friends.members`). Secrets ([`crate::config::SECRET_KEYS`]) are never returned;
//! [`Settings::secrets_set`] says which ones have a value. API keys are listed in
//! [`Settings::api_keys`] without their keys.

use serde::{Deserialize, Serialize, Serializer};

use crate::config::{ApiKeyRole, Config};

/// `GET`/`PATCH /api/v0/settings` response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The merged configuration without secrets and API keys (see [`Config::public_json`]).
    #[serde(serialize_with = "public")]
    pub values: Config,
    /// Dotted keys set by `slskr.toml`, the environment or the command line, sorted.
    pub pinned: Vec<String>,
    /// Dotted secret keys that have a value.
    pub secrets_set: Vec<String>,
    /// True when a changed setting only takes effect after a restart.
    pub pending_restart: bool,
    /// Configured API keys, without the keys themselves.
    pub api_keys: Vec<ApiKeyInfo>,
}

fn public<S: Serializer>(config: &Config, serializer: S) -> Result<S::Ok, S::Error> {
    config.public_json().serialize(serializer)
}

/// An API key as listed by [`Settings::api_keys`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKeyInfo {
    pub name: String,
    pub role: ApiKeyRole,
    pub allow: Vec<String>,
    /// True for keys from `slskr.toml`, which can not be deleted from the API.
    pub pinned: bool,
}

/// `POST /api/v0/settings/api-keys` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApiKey {
    pub name: String,
    #[serde(default)]
    pub role: ApiKeyRole,
    /// CIDR ranges allowed to use the key; empty or absent allows every address.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// `POST /api/v0/settings/api-keys` response: the only time the key is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedApiKey {
    pub name: String,
    pub role: ApiKeyRole,
    pub allow: Vec<String>,
    pub key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_leave_secrets_out_and_round_trip() {
        let mut settings = Settings {
            pinned: vec!["web.listen".into()],
            secrets_set: vec!["web.password".into()],
            ..Default::default()
        };
        settings.values.web.password = "secret".into();
        let json = serde_json::to_value(&settings).unwrap();
        assert!(json["values"]["web"].get("password").is_none());
        assert_eq!(json["values"]["web"]["listen"], "127.0.0.1:5030");
        assert_eq!(json["pinned"][0], "web.listen");
        let back: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(back.values.web.password, "");
        assert_eq!(back.secrets_set, ["web.password"]);
    }
}
