//! Planning a folder download: which files of a peer's folder listing to enqueue.
//!
//! A FolderContentsResponse can list several folders (Soulseek.NET returns a
//! collection), but slskd, Nicotine+ and slskr answer with the requested folder only.
//! A recursive download therefore takes the folders below it from a browse of the
//! user's shares when the response lists none.

use std::collections::HashSet;

use slskr_types::{
    transfers::{DownloadFile, EnqueueFailure},
    users::Directory,
};

use super::{api::file_problem, destination};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FolderPlan {
    /// The files to enqueue, with full remote names.
    pub(super) files: Vec<DownloadFile>,
    /// Files the peer listed under names that cannot be downloaded safely.
    pub(super) rejected: Vec<EnqueueFailure>,
}

/// Whether `directories` lists a folder below `root`.
pub(super) fn has_subfolders(root: &str, directories: &[Directory]) -> bool {
    directories
        .iter()
        .any(|directory| destination::relative_to(root, &directory.name).is_some())
}

/// The files of `root` and, when `recursive`, of every folder below it. Other folders
/// are ignored and a file listed twice is planned once. `None` when no listed folder
/// is `root` or (when `recursive`) below it.
pub(super) fn plan(
    root: &str,
    recursive: bool,
    directories: impl IntoIterator<Item = Directory>,
) -> Option<FolderPlan> {
    let root = destination::trim_folder(root);
    let mut found = false;
    let mut seen = HashSet::new();
    let mut plan = FolderPlan::default();
    for directory in directories {
        let name = destination::trim_folder(&directory.name);
        let selected = if recursive {
            destination::within(root, name)
        } else {
            name == root
        };
        if !selected {
            continue;
        }
        found = true;
        let separator = if name.contains('/') && !name.contains('\\') {
            '/'
        } else {
            '\\'
        };
        for file in directory.files {
            // Browse and folder listings name files relative to their folder.
            let filename = format!("{name}{separator}{}", file.filename);
            if !seen.insert(filename.clone()) {
                continue;
            }
            match file_problem(&filename, file.size) {
                Some(problem) => plan.rejected.push(EnqueueFailure {
                    filename,
                    error: format!("Skipped: {problem}"),
                }),
                None => plan.files.push(DownloadFile {
                    filename,
                    size: file.size,
                }),
            }
        }
    }
    found.then_some(plan)
}

#[cfg(test)]
mod tests {
    use slskr_types::users::File;

    use super::*;

    fn dir(name: &str, files: &[&str]) -> Directory {
        Directory {
            name: name.into(),
            files: files
                .iter()
                .map(|filename| File {
                    filename: (*filename).into(),
                    size: 3,
                    ..File::default()
                })
                .collect(),
        }
    }

    fn names(plan: &FolderPlan) -> Vec<&str> {
        plan.files.iter().map(|f| f.filename.as_str()).collect()
    }

    fn album() -> Vec<Directory> {
        vec![
            dir(r"@@m\Album", &["cover.jpg"]),
            dir(r"@@m\Album\CD1", &["01.flac", "02.flac"]),
            dir(r"@@m\Album\CD2\", &["01.flac"]),
            dir(r"@@m\Album\CD2\Bonus", &["x.flac"]),
            dir(r"@@m\Albums", &["other.flac"]),
            dir(r"@@m", &["root.flac"]),
        ]
    }

    #[test]
    fn non_recursive_takes_only_the_folder() {
        let plan = plan(r"@@m\Album\", false, album()).unwrap();
        assert_eq!(names(&plan), [r"@@m\Album\cover.jpg"]);
        assert!(plan.rejected.is_empty());
        assert!(!has_subfolders(r"@@m\Album", &album()[..1]));
    }

    #[test]
    fn recursive_takes_the_subtree() {
        assert!(has_subfolders(r"@@m\Album", &album()));
        let plan = plan(r"@@m\Album", true, album()).unwrap();
        assert_eq!(
            names(&plan),
            [
                r"@@m\Album\cover.jpg",
                r"@@m\Album\CD1\01.flac",
                r"@@m\Album\CD1\02.flac",
                r"@@m\Album\CD2\01.flac",
                r"@@m\Album\CD2\Bonus\x.flac",
            ]
        );
        // Only subfolders listed (the folder itself had no files).
        let plan = super::plan(r"@@m\Album", true, album().into_iter().skip(1)).unwrap();
        assert_eq!(plan.files.len(), 4);
    }

    #[test]
    fn missing_folder_and_duplicates() {
        assert_eq!(plan(r"@@m\Missing", true, album()), None);
        assert_eq!(plan(r"@@m\Album\CD1", false, Vec::new()), None);
        // A subfolder alone does not satisfy a non-recursive request.
        assert_eq!(plan(r"@@m\Album", false, album().into_iter().skip(1)), None);
        let mut listing = album();
        listing.extend(album());
        let plan = plan(r"@@m\Album", true, listing).unwrap();
        assert_eq!(plan.files.len(), 5);
        // An empty folder is found but plans nothing.
        let empty = super::plan("x", false, [dir("x", &[])]).unwrap();
        assert!(empty.files.is_empty() && empty.rejected.is_empty());
    }

    #[test]
    fn preserves_slash_separated_remote_paths() {
        let listing = [
            dir("@@m/Album", &["cover.jpg"]),
            dir("@@m/Album/CD1", &["01.flac"]),
        ];
        let plan = plan("@@m/Album", true, listing).unwrap();
        assert_eq!(
            names(&plan),
            ["@@m/Album/cover.jpg", "@@m/Album/CD1/01.flac"]
        );
    }

    #[test]
    fn unsafe_peer_names_are_rejected() {
        let listing = vec![
            dir(r"@@m\Album", &["ok.flac", r"..\..\evil", "nul\0byte", ""]),
            dir(r"@@m\Album\..\..\etc", &["passwd"]),
            dir(r"@@m\Album\.", &["dot"]),
        ];
        let plan = plan(r"@@m\Album", true, listing).unwrap();
        assert_eq!(names(&plan), [r"@@m\Album\ok.flac"]);
        assert_eq!(plan.rejected.len(), 5);
        assert!(
            plan.rejected
                .iter()
                .all(|f| f.error.starts_with("Skipped: "))
        );
    }
}
