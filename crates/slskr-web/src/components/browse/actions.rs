//! Downloads and menus shared by the folder tree and the folder pane.

use std::sync::Arc;

use super::tree::Shares;
use crate::{
    services::{api, util},
    ui::{ContextMenuState, MenuEntry},
};
use leptos::prelude::*;
use slskr_types::transfers::{
    DownloadFile, EnqueueDownloads, EnqueueDownloadsRequest, EnqueuedDownloads,
};

/// Queues `files` (all under the remote folder `root`) with `POST /api/transfers/downloads`;
/// the daemon saves them as `<downloads>/<last component of root>/<path below root>`.
///
/// Browse already holds the whole tree, so folder downloads use this rather than
/// `POST /api/transfers/downloads/folder`, which would ask the peer for the folder again.
async fn enqueue_downloads(
    username: String,
    files: Vec<DownloadFile>,
    root: String,
) -> Result<EnqueuedDownloads, api::ApiError> {
    let request = EnqueueDownloadsRequest {
        downloads: EnqueueDownloads { username, files },
        root: Some(root),
    };
    api::post("/transfers/downloads", &request).await
}

/// The result of the last download request, shown as a toast.
#[derive(Clone, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub variant: &'static str,
    pub title: String,
    pub body: String,
    pub link: bool,
}

pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        super::group_digits(count),
        if count == 1 { one } else { many }
    )
}

/// Page-wide browse actions, provided as context by `Browse`.
#[derive(Clone, Copy)]
pub struct BrowseActions {
    pub username: RwSignal<String>,
    pub results: RwSignal<Option<Arc<Shares>>>,
    pub pending: RwSignal<bool>,
    pub notice: RwSignal<Option<Notice>>,
    pub menu: RwSignal<Option<ContextMenuState>>,
}

impl BrowseActions {
    fn shares(&self) -> Option<Arc<Shares>> {
        self.results.get_untracked()
    }

    /// Downloads a folder's files (and its subfolders' when `recursive`), keeping the
    /// folder structure.
    pub fn download_folder(self, index: usize, recursive: bool) {
        let Some(shares) = self.shares() else { return };
        let files = shares
            .folder_files(index, recursive)
            .into_iter()
            .map(|(folder, file)| {
                let file = &shares.files[folder][file];
                DownloadFile {
                    filename: shares.remote_path(folder, file),
                    size: file.size,
                }
            })
            .collect();
        self.enqueue(files, shares.nodes[index].path.clone());
    }

    /// Downloads some files of one folder.
    pub fn download_files(self, folder: usize, files: &[usize]) {
        let Some(shares) = self.shares() else { return };
        if shares.nodes[folder].locked {
            return;
        }
        let files = files
            .iter()
            .filter_map(|&file| shares.files[folder].get(file))
            .map(|file| DownloadFile {
                filename: shares.remote_path(folder, file),
                size: file.size,
            })
            .collect();
        self.enqueue(files, shares.nodes[folder].path.clone());
    }

    fn enqueue(self, files: Vec<DownloadFile>, root: String) {
        if files.is_empty() || self.pending.get_untracked() {
            return;
        }
        self.pending.set(true);
        let total = files.len();
        let username = self.username.get_untracked();
        let notice = self.notice;
        let pending = self.pending;
        leptos::task::spawn_local(async move {
            let (variant, title, body, link) =
                match enqueue_downloads(username.clone(), files, root).await {
                    Ok(response) if response.failed.is_empty() => (
                        "success",
                        format!("Queued {}", plural(total, "file", "files")),
                        format!("from {username}"),
                        true,
                    ),
                    Ok(response) => (
                        if response.enqueued.is_empty() {
                            "danger"
                        } else {
                            "warning"
                        },
                        if response.enqueued.is_empty() {
                            "Nothing was queued".to_owned()
                        } else {
                            format!(
                                "Queued {}, {} failed",
                                plural(response.enqueued.len(), "file", "files"),
                                response.failed.len()
                            )
                        },
                        response
                            .failed
                            .first()
                            .map(|failure| failure.error.clone())
                            .unwrap_or_default(),
                        true,
                    ),
                    Err(error) => (
                        "danger",
                        "Could not queue the download".to_owned(),
                        error.to_string(),
                        false,
                    ),
                };
            let id = js_sys::Date::now() as u64;
            let _ = notice.try_set(Some(Notice {
                id,
                variant,
                title,
                body,
                link,
            }));
            let _ = pending.try_set(false);
            // Success notices go away on their own.
            if variant == "success" {
                util::sleep(6000).await;
                if notice.try_with_untracked(|n| n.as_ref().map(|n| n.id)) == Some(Some(id)) {
                    let _ = notice.try_set(None);
                }
            }
        });
    }

    /// The folder entries of a menu: download folder, folder + subfolders, copy path.
    pub fn folder_entries(self, index: usize) -> Vec<MenuEntry> {
        let Some(shares) = self.shares() else {
            return Vec::new();
        };
        let node = &shares.nodes[index];
        let (count, size) = shares.folder_totals(index, false);
        let (all, all_size) = shares.folder_totals(index, true);
        let has_children = shares.has_children(index);
        let path = node.path.clone();
        let mut entries = vec![
            MenuEntry::item(
                format!(
                    "Download folder ({}, {})",
                    plural(count, "file", "files"),
                    util::format_bytes(size as f64)
                ),
                "download",
                move || self.download_folder(index, false),
            )
            .disabled(count == 0),
        ];
        if has_children {
            entries.push(
                MenuEntry::item(
                    format!(
                        "Download folder + subfolders ({}, {})",
                        plural(all, "file", "files"),
                        util::format_bytes(all_size as f64)
                    ),
                    "folder-open",
                    move || self.download_folder(index, true),
                )
                .disabled(all == 0),
            );
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::item("Copy path", "copy", move || {
            copy_text(&path)
        }));
        entries
    }

    /// Opens the folder menu.
    pub fn open_folder_menu(self, index: usize, anchor: MenuAnchor) {
        let label = self
            .shares()
            .map(|shares| format!("Folder {}", shares.nodes[index].label))
            .unwrap_or_default();
        let entries = self.folder_entries(index);
        self.menu.set(Some(anchor.place(label, entries)));
    }
}

/// Where a menu opens.
pub enum MenuAnchor {
    Pointer(leptos::ev::MouseEvent),
    Below(web_sys::Element),
}

impl MenuAnchor {
    pub fn place(self, label: String, entries: Vec<MenuEntry>) -> ContextMenuState {
        match self {
            Self::Pointer(event) => ContextMenuState::at_pointer(&event, label, entries),
            Self::Below(element) => ContextMenuState::below(&element, label, entries),
        }
    }
}

pub fn copy_text(text: &str) {
    let _ = window().navigator().clipboard().write_text(text);
}
