//! The downloads and incomplete directories (`/api/files/{root}`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A file in a listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemFile {
    pub name: String,
    /// Path relative to the root (`downloads` or `incomplete`), `/`-separated.
    pub path: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

/// A directory listing. Subdirectories carry `files` and `directories` only in recursive
/// listings; otherwise they are empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemDirectory {
    pub name: String,
    /// Path relative to the root, `/`-separated; empty for the root itself.
    pub path: String,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    #[serde(default)]
    pub files: Vec<FilesystemFile>,
    #[serde(default)]
    pub directories: Vec<FilesystemDirectory>,
}
