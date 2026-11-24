//! Parse share specifications and build an index without following symlinks.

use std::{
    cell::OnceCell,
    collections::{BTreeMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use regex::{RegexSet, RegexSetBuilder};
use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use super::{ScanOptions, ShareContents, ShareFile, Snapshot, audio, valid_remote_component};

#[derive(Debug)]
pub(super) struct ShareSpec<'a> {
    pub raw: &'a str,
    pub local: &'a Path,
    pub alias: &'a str,
    pub excluded: bool,
}

impl<'a> ShareSpec<'a> {
    pub(super) fn parse(raw: &'a str) -> Self {
        let raw = raw.trim_end_matches(['/', '\\']);
        let (excluded, body) = raw
            .strip_prefix(['-', '!'])
            .map_or((false, raw), |body| (true, body));
        let (alias, local) = body
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .unwrap_or_else(|| (body.rsplit(['/', '\\']).next().unwrap_or_default(), body));
        Self {
            raw,
            local: Path::new(local),
            alias,
            excluded,
        }
    }

    pub(super) fn id(&self) -> String {
        format!("{:X}", Sha1::digest(self.alias.as_bytes()))
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            valid_remote_component(self.alias) && !self.alias.trim().is_empty(),
            "invalid share alias in {:?}",
            self.raw
        );
        ensure!(
            self.local.is_absolute(),
            "share path is not absolute in {:?}",
            self.raw
        );
        ensure!(
            self.local.parent().is_some(),
            "cannot share a filesystem root: {:?}",
            self.raw
        );
        ensure!(
            self.local
                .components()
                .all(|part| !matches!(part, Component::CurDir | Component::ParentDir))
                && !self
                    .local
                    .as_os_str()
                    .to_string_lossy()
                    .split(std::path::is_separator)
                    .any(|part| part == "." || part == ".."),
            "unsafe share path in {:?}",
            self.raw
        );
        Ok(())
    }
}

pub(super) fn specs(roots: &[String]) -> impl Iterator<Item = ShareSpec<'_>> {
    let mut unique = HashSet::with_capacity(roots.len());
    roots
        .iter()
        .map(|spec| ShareSpec::parse(spec))
        .filter(move |spec| unique.insert(spec.raw))
}

pub(super) fn scan_roots(
    options: ScanOptions,
    previous: Option<&Snapshot>,
    cancel: &CancellationToken,
    progress: impl Fn(u32),
) -> Result<Snapshot> {
    ensure!(!cancel.is_cancelled(), "share scan cancelled");
    let scanned_at = Utc::now();
    let filters = RegexSetBuilder::new(&options.filters)
        .case_insensitive(true)
        .build()
        .context("compiling share filters")?;
    let mut aliases = HashSet::new();
    let mut roots = HashSet::new();
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for spec in specs(&options.directories) {
        ensure!(!cancel.is_cancelled(), "share scan cancelled");
        spec.validate()?;
        ensure!(
            aliases.insert(spec.alias),
            "duplicate share alias: {:?}",
            spec.alias
        );
        if spec.excluded {
            let local = match spec.local.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => spec.local.to_owned(),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("resolving excluded share {}", spec.local.display())
                    });
                }
            };
            excludes.push(local);
        } else {
            let metadata = fs::symlink_metadata(spec.local)
                .with_context(|| format!("opening share root {}", spec.local.display()))?;
            ensure!(
                metadata.is_dir(),
                "share root is not a real directory: {}",
                spec.local.display()
            );
            let root = spec
                .local
                .canonicalize()
                .with_context(|| format!("resolving share root {}", spec.local.display()))?;
            ensure!(
                roots.insert(root.clone()),
                "duplicate share directory: {}",
                root.display()
            );
            includes.push((spec, root));
        }
    }

    let mut shares = BTreeMap::new();
    let mut completed = 0_u32;
    for (spec, root) in includes {
        ensure!(!cancel.is_cancelled(), "share scan cancelled");
        if !excludes.iter().any(|path| root.starts_with(path)) {
            let cached = previous
                .and_then(|snapshot| snapshot.shares.get(spec.alias))
                .filter(|share| share.root == root);
            let contents = scan_root(
                &spec,
                root,
                &roots,
                &excludes,
                &filters,
                cancel,
                cached,
                |count| {
                    progress(completed.saturating_add(count));
                },
            )?;
            completed = completed.saturating_add(contents.file_count);
            shares.insert(spec.alias.into(), contents);
        }
        progress(completed);
    }
    Ok(Snapshot {
        options,
        scanned_at,
        shares,
        index: Default::default(),
        browse: Default::default(),
    })
}

fn scan_root(
    spec: &ShareSpec<'_>,
    root: PathBuf,
    roots: &HashSet<PathBuf>,
    excludes: &[PathBuf],
    filters: &RegexSet,
    cancel: &CancellationToken,
    cached: Option<&ShareContents>,
    progress: impl Fn(u32),
) -> Result<ShareContents> {
    let mut directories = BTreeMap::<String, Vec<ShareFile>>::new();
    let mut file_count = 0u32;
    let mut parents = Vec::<String>::new();
    let audio_root = OnceCell::new();
    let mut entries = WalkDir::new(&root)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter();
    while let Some(entry) = entries.next() {
        ensure!(!cancel.is_cancelled(), "share scan cancelled");
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "skipping inaccessible share entry");
                continue;
            }
        };
        let depth = entry.depth();
        let path = entry.path();
        let file_type = entry.file_type();
        let is_dir = file_type.is_dir();
        if !is_dir && !file_type.is_file() {
            continue;
        }
        let Some(name) = (if depth == 0 {
            Some(spec.alias)
        } else {
            entry.file_name().to_str()
        }) else {
            if is_dir {
                entries.skip_current_dir();
            }
            continue;
        };
        if depth > 0
            && (!valid_remote_component(name)
                || name.starts_with('.')
                || (is_dir && roots.contains(path))
                || excludes.iter().any(|exclude| path.starts_with(exclude))
                || is_filtered(path, &root, spec.local, filters))
        {
            if is_dir {
                entries.skip_current_dir();
            }
            continue;
        }
        if is_dir {
            parents.truncate(depth);
            let remote = parents.last().map_or_else(
                || spec.alias.to_owned(),
                |parent| format!("{parent}\\{name}"),
            );
            directories.insert(remote.clone(), Vec::new());
            parents.push(remote);
        } else {
            let Some(parent) = depth.checked_sub(1).and_then(|idx| parents.get(idx)) else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(%error, "skipping inaccessible shared file");
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map_or_else(String::new, str::to_lowercase);
            let modified = metadata.modified().ok().map(chrono::DateTime::<Utc>::from);
            let cached_file = cached
                .and_then(|share| share.directories.get(parent))
                .and_then(|files| {
                    files
                        .binary_search_by(|file| file.name.as_str().cmp(name))
                        .ok()
                        .map(|index| &files[index])
                })
                .filter(|file| {
                    modified.is_some()
                        && file.modified == modified
                        && file.size == metadata.len()
                        && !file.attributes.is_empty()
                });
            let attributes = cached_file.map_or_else(
                || audio::read(&audio_root, &root, path, &extension),
                |file| file.attributes.clone(),
            );
            directories
                .get_mut(parent)
                .expect("scanned parent directory")
                .push(ShareFile {
                    name: name.into(),
                    modified,
                    size: metadata.len(),
                    extension,
                    attributes,
                });
            file_count = file_count.saturating_add(1);
            if file_count == 1 || file_count.is_multiple_of(128) {
                progress(file_count);
            }
        }
    }
    for files in directories.values_mut() {
        files.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(ShareContents {
        root,
        configured_root: spec.local.to_owned(),
        directories,
        file_count,
    })
}

fn is_filtered(path: &Path, root: &Path, configured_root: &Path, filters: &RegexSet) -> bool {
    if filters.is_empty() {
        return false;
    }
    if configured_root == root {
        filters.is_match(&path.to_string_lossy())
    } else {
        let configured = configured_root.join(path.strip_prefix(root).expect("walked share path"));
        filters.is_match(&configured.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shares::{Attribute, AttributeType, NamedAttribute};

    #[test]
    fn rescans_reuse_metadata_and_refresh_changed_files() {
        let root = tempfile::tempdir().unwrap();
        let track = root.path().join("track.mp3");
        fs::write(&track, b"invalid audio").unwrap();
        let scan = |previous: Option<&Snapshot>| {
            let seen = std::cell::Cell::new(0);
            let snapshot = scan_roots(
                ScanOptions {
                    directories: vec![format!("[music]{}", root.path().display())],
                    filters: Vec::new(),
                },
                previous,
                &CancellationToken::new(),
                |count| seen.set(count),
            )
            .unwrap();
            assert_eq!(seen.get(), 1);
            snapshot
        };
        let mut first = scan(None);
        first
            .shares
            .get_mut("music")
            .unwrap()
            .directories
            .get_mut("music")
            .unwrap()[0]
            .attributes
            .push(Attribute {
                kind: AttributeType::Named(NamedAttribute::BitRate),
                value: 320,
            });
        let second = scan(Some(&first));
        assert_eq!(
            second.shares["music"].directories["music"][0].attributes[0].value,
            320
        );
        fs::write(&track, b"different size, invalid audio").unwrap();
        let third = scan(Some(&second));
        assert!(
            third.shares["music"].directories["music"][0]
                .attributes
                .is_empty()
        );
    }

    #[test]
    fn upstream_share_id_and_spec_syntax() {
        let share = ShareSpec::parse("[abc]/some/music/");
        assert_eq!(share.id(), "A9993E364706816ABA3E25717850C26C9CD0D89D");
        assert_eq!(share.raw, "[abc]/some/music");
        assert_eq!(share.local, Path::new("/some/music"));
        assert!(!share.excluded);
        assert_eq!(ShareSpec::parse("-/some/music").alias, "music");
        assert!(ShareSpec::parse("!/some/music").excluded);
        assert!(ShareSpec::parse("[a\\b]/some/music").validate().is_err());
        assert!(ShareSpec::parse("[a]relative/music").validate().is_err());
        assert!(ShareSpec::parse("[a]/some/./music").validate().is_err());
    }

    #[test]
    fn deepest_root_wins_and_exclusions_and_regex_prune_subtrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for dir in ["nested", "private", "Filtered", "empty"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        for path in [
            "a.MP3",
            "nested/song.flac",
            "private/secret.mp3",
            "Filtered/secret.mp3",
            "ignore.TMP",
        ] {
            fs::write(root.join(path), b"audio").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("private"), root.join("linked")).unwrap();
        let snapshot = scan_roots(
            ScanOptions {
                directories: vec![
                    format!("[collection]{}", root.display()),
                    format!("[album]{}", root.join("nested").display()),
                    format!("!{}", root.join("private").display()),
                ],
                filters: vec![r"\.tmp$".into(), r"Filtered$".into()],
            },
            None,
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap();
        let collection = &snapshot.shares["collection"];
        assert_eq!(
            collection
                .directories
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["collection", "collection\\empty"]
        );
        assert_eq!(collection.directories["collection"][0].extension, "mp3");
        assert_eq!(
            snapshot.shares["album"].directories["album"][0].name,
            "song.flac"
        );
        assert_eq!(super::super::snapshot_counts(&snapshot), (3, 2));
    }

    #[test]
    fn cancelled_scan_does_not_build_an_index() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            scan_roots(
                ScanOptions {
                    directories: Vec::new(),
                    filters: Vec::new(),
                },
                None,
                &token,
                |_| {}
            )
            .is_err()
        );
    }
}
