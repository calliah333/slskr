//! Shares API models (`/api/v0/shares`).
//!
//! `GET /shares/contents` and `GET /shares/{id}/contents` return
//! `Vec<`[`Directory`]`>` (Soulseek.NET directories).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::users::{Directory, File};

/// slskd `Shares.Share`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub is_excluded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(default)]
    pub raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
    /// Directory count, once scanned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directories: Option<u32>,
    /// File count, once scanned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
}

/// Response of `GET /shares`: shares keyed by host name.
pub type SharesByHost = BTreeMap<String, Vec<Share>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_json_shape() {
        let json = r#"{"local":[{"id":"abc","alias":"music","isExcluded":false,"localPath":"/data/music","raw":"[music]/data/music","remotePath":"@@abc\\music","directories":3,"files":42}]}"#;
        let shares: SharesByHost = serde_json::from_str(json).unwrap();
        let share = &shares["local"][0];
        assert_eq!(share.remote_path.as_deref(), Some("@@abc\\music"));
        assert_eq!(share.files, Some(42));
        assert_eq!(serde_json::to_string(&shares).unwrap(), json);

        let excluded = Share {
            id: "x".into(),
            is_excluded: true,
            raw: "-/data/music/private".into(),
            local_path: Some("/data/music/private".into()),
            ..Default::default()
        };
        let value = serde_json::to_value(&excluded).unwrap();
        assert_eq!(value["isExcluded"], true);
        assert!(value.get("directories").is_none());
        assert!(value.get("alias").is_none());
    }
}
