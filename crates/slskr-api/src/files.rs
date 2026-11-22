//! Files API models (`/api/v0/files/{downloads,incomplete}/directories`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transfers::time;

crate::slskd_flags! {
    /// .NET `System.IO.FileAttributes`.
    FileAttributes {
        READ_ONLY = 1 => "ReadOnly",
        HIDDEN = 2 => "Hidden",
        SYSTEM = 4 => "System",
        DIRECTORY = 16 => "Directory",
        ARCHIVE = 32 => "Archive",
        DEVICE = 64 => "Device",
        NORMAL = 128 => "Normal",
        TEMPORARY = 256 => "Temporary",
        SPARSE_FILE = 512 => "SparseFile",
        REPARSE_POINT = 1024 => "ReparsePoint",
        COMPRESSED = 2048 => "Compressed",
        OFFLINE = 4096 => "Offline",
        NOT_CONTENT_INDEXED = 8192 => "NotContentIndexed",
        ENCRYPTED = 16384 => "Encrypted",
        INTEGRITY_STREAM = 32768 => "IntegrityStream",
        NO_SCRUB_DATA = 131072 => "NoScrubData",
    }
}

/// slskd `Files.FilesystemFile`. `fullName` is relative to the listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemFile {
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub length: u64,
    #[serde(default)]
    pub attributes: FileAttributes,
    #[serde(with = "time::datetime")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "time::datetime")]
    pub modified_at: DateTime<Utc>,
}

/// slskd `Files.FilesystemDirectory`. Only the listed (root) directory carries
/// `files`/`directories`; nested entries omit them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemDirectory {
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub attributes: FileAttributes,
    #[serde(with = "time::datetime")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "time::datetime")]
    pub modified_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FilesystemFile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<FilesystemDirectory>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_attributes_round_trip() {
        assert_eq!(
            serde_json::to_string(&FileAttributes::DIRECTORY).unwrap(),
            r#""Directory""#
        );
        let hidden_dir = FileAttributes::HIDDEN | FileAttributes::DIRECTORY;
        assert_eq!(
            serde_json::to_string(&hidden_dir).unwrap(),
            r#""Hidden, Directory""#
        );
        assert_eq!(
            serde_json::from_str::<FileAttributes>(r#""Hidden, Directory""#).unwrap(),
            hidden_dir
        );
        assert_eq!(
            serde_json::to_string(&FileAttributes::default()).unwrap(),
            "0"
        );
        assert_eq!(
            serde_json::from_str::<FileAttributes>("0").unwrap(),
            FileAttributes::default()
        );
    }

    #[test]
    fn directory_json_shape() {
        let json = r#"{
            "name": "downloads",
            "fullName": "",
            "attributes": "Directory",
            "createdAt": "2024-05-01T12:00:00.1234567Z",
            "modifiedAt": "2024-05-02T12:00:00Z",
            "files": [{
                "name": "a.flac",
                "fullName": "a.flac",
                "length": 1024,
                "attributes": "Normal",
                "createdAt": "2024-05-01T12:00:00Z",
                "modifiedAt": "2024-05-01T12:00:00Z"
            }],
            "directories": [{
                "name": "album",
                "fullName": "album",
                "attributes": "Directory",
                "createdAt": "2024-05-01T12:00:00Z",
                "modifiedAt": "2024-05-01T12:00:00Z"
            }]
        }"#;
        let dir: FilesystemDirectory = serde_json::from_str(json).unwrap();
        let files = dir.files.as_ref().unwrap();
        assert_eq!(files[0].length, 1024);
        assert_eq!(files[0].attributes, FileAttributes::NORMAL);
        assert!(dir.directories.as_ref().unwrap()[0].files.is_none());

        let value = serde_json::to_value(&dir).unwrap();
        assert_eq!(value["fullName"], "");
        assert_eq!(value["files"][0]["modifiedAt"], "2024-05-01T12:00:00Z");
        assert!(value["directories"][0].get("files").is_none());
    }
}
