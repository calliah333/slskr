//! Peer response models.

use super::file::{Directory, File};

/// A search response received from (or sent to) a peer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResponse {
    pub username: String,
    pub token: u32,
    pub files: Vec<File>,
    pub locked_files: Vec<File>,
    pub has_free_upload_slot: bool,
    pub upload_speed: u32,
    pub queue_length: u64,
}

impl SearchResponse {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn locked_file_count(&self) -> usize {
        self.locked_files.len()
    }
}

/// User information returned by a peer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserInfo {
    pub description: String,
    pub picture: Option<Vec<u8>>,
    pub upload_slots: u32,
    pub queue_length: u32,
    pub has_free_upload_slot: bool,
    pub upload_permissions: Option<u32>,
}

impl UserInfo {
    pub fn has_picture(&self) -> bool {
        self.picture.is_some()
    }
}

/// A peer's shares listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrowseResponse {
    pub directories: Vec<Directory>,
    pub locked_directories: Vec<Directory>,
}

impl BrowseResponse {
    pub fn directory_count(&self) -> usize {
        self.directories.len()
    }

    pub fn locked_directory_count(&self) -> usize {
        self.locked_directories.len()
    }
}
