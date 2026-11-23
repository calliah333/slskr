//! Shared directories (`/api/shares`).
//!
//! `GET /api/shares/{id}/contents` returns `Vec<`[`Directory`]`>`.

use serde::{Deserialize, Serialize};

pub use crate::users::{Directory, File};

/// A configured `shares.directories` entry.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Share {
    pub id: String,
    /// The configured entry, as written.
    pub raw: String,
    /// The name peers see (the last path component unless `[alias]` is given).
    pub alias: Option<String>,
    /// `-path` entries exclude a directory below another share.
    pub is_excluded: bool,
    pub local_path: Option<String>,
    /// The path peers see.
    pub remote_path: Option<String>,
    /// Directory count, once scanned.
    pub directories: Option<u32>,
    /// File count, once scanned.
    pub files: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_shape() {
        let share = Share {
            id: "x".into(),
            is_excluded: true,
            raw: "-/data/music/private".into(),
            local_path: Some("/data/music/private".into()),
            ..Default::default()
        };
        let value = serde_json::to_value(&share).unwrap();
        assert_eq!(value["is_excluded"], true);
        assert!(value["directories"].is_null());
    }
}
