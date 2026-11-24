use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use chrono::Utc;
use sha2::{Digest, Sha256};
use slskr_types::{
    config::Config,
    transfers::{Batch, Transfer},
};
use tokio::{
    io::{AsyncSeek, AsyncWrite},
    sync::oneshot,
};
use uuid::Uuid;

pub(super) fn remote_directory(filename: &str) -> &str {
    filename
        .rsplit_once(['/', '\\'])
        .map_or("", |(directory, _)| directory)
}

pub(super) fn traverses(value: &str) -> bool {
    value
        .split(['/', '\\'])
        .any(|part| matches!(part, "." | ".."))
}

pub(super) fn relative(value: &str) -> bool {
    !value.starts_with(['/', '\\'])
        && !value.as_bytes().get(1).is_some_and(|c| *c == b':')
        && !value.contains('\0')
        && !traverses(value)
}

/// A remote folder without trailing separators (`Music\Album\` is `Music\Album`).
pub(super) fn trim_folder(folder: &str) -> &str {
    folder.trim_end_matches(['/', '\\'])
}

/// The last component of a remote folder: the local folder a folder download is saved in.
pub(super) fn folder_name(folder: &str) -> &str {
    trim_folder(folder)
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
}

/// `path` relative to the remote folder `root`, or `None` when it is not below `root`.
/// Remote names are compared exactly; either separator ends the root.
pub(super) fn relative_to<'a>(root: &str, path: &'a str) -> Option<&'a str> {
    let root = trim_folder(root);
    if root.is_empty() {
        return None;
    }
    path.strip_prefix(root)?
        .strip_prefix(['/', '\\'])
        .filter(|rest| !rest.is_empty())
}

/// Whether the remote folder `name` is `root` or below it.
pub(super) fn within(root: &str, name: &str) -> bool {
    trim_folder(name) == trim_folder(root) || relative_to(root, name).is_some()
}

/// The download subdirectory of a folder download: the sanitized last component of
/// `root`, then each sanitized folder component of `filename` below `root`.
fn folder_layout(root: &str, filename: &str) -> PathBuf {
    let mut path = PathBuf::from(sanitize(folder_name(root)));
    if let Some(relative) = relative_to(root, filename) {
        path.extend(sanitized_path(remote_directory(relative)).iter());
    }
    path
}

fn sanitize(value: &str) -> String {
    let mut result: String = value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    #[cfg(windows)]
    {
        result.truncate(result.trim_end_matches(['.', ' ']).len());
        let basename = result.split('.').next().unwrap_or_default();
        let device = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
            .iter()
            .any(|name| basename.eq_ignore_ascii_case(name));
        let port = basename.get(..3).is_some_and(|prefix| {
            prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT")
        }) && basename.get(3..).is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
        if device || port {
            result.insert(0, '_');
        }
    }
    if result.is_empty() || matches!(result.as_str(), "." | "..") {
        result = "_".into();
    }
    result
}

fn sanitized_path(value: &str) -> PathBuf {
    value
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(sanitize)
        .collect()
}

pub(super) fn derive(
    options: &Config,
    t: &Transfer,
    batch: Option<&Batch>,
    search_text: Option<&str>,
) -> PathBuf {
    if let Some(explicit) = batch.and_then(|b| b.destination.as_deref()) {
        return sanitized_path(explicit);
    }
    if let Some(root) = t.root.as_deref() {
        return folder_layout(root, &t.filename);
    }
    let pattern = options.downloads.subdirectory.trim();
    if pattern.is_empty() {
        return PathBuf::new();
    }
    let remote = remote_directory(&t.filename);
    let remote = if remote
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && remote.as_bytes().get(1) == Some(&b':')
    {
        &remote[2..]
    } else {
        remote
    };
    let source = sanitized_path(remote);
    let parent = source
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    let username = sanitize(&t.username);
    let external = sanitize(
        batch
            .and_then(|b| b.external_id.as_deref())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("unknown_batch_external_id"),
    );
    let search_text = sanitize(
        search_text
            .filter(|text| !text.trim().is_empty())
            .unwrap_or("unknown_search_text"),
    );
    let mut expanded = String::new();
    let mut rest = pattern;
    while let Some((prefix, suffix)) = rest.split_once("${") {
        expanded.push_str(prefix);
        let Some((token, tail)) = suffix.split_once('}') else {
            expanded.push_str("${");
            rest = suffix;
            break;
        };
        match token.to_ascii_uppercase().as_str() {
            "SOURCE_USERNAME" => expanded.push_str(&username),
            "SOURCE_PATH" => expanded.push_str(&source.to_string_lossy()),
            "SOURCE_DIRECTORY" => expanded.push_str(parent),
            "BATCH_ID" => expanded.push_str(batch.map_or("unknown_batch_id", |b| b.id.as_str())),
            "BATCH_EXTERNAL_ID" => expanded.push_str(&external),
            "SEARCH_ID" => expanded.push_str(
                batch
                    .and_then(|b| b.search_id.as_deref())
                    .unwrap_or("unknown_search_id"),
            ),
            "SEARCH_TEXT" => expanded.push_str(&search_text),
            _ => {
                expanded.push_str("${");
                expanded.push_str(token);
                expanded.push('}');
            }
        }
        rest = tail;
    }
    expanded.push_str(rest);
    sanitized_path(&expanded)
}

pub(super) struct Partial {
    root: PathBuf,
    directory: PathBuf,
    filename: String,
}

impl Partial {
    pub(super) fn new(root: PathBuf, username: &str, remote: &str, size: u64) -> Self {
        // Use the exact peer, path and size as the resume identity. Separators,
        // case-folding and sanitization can all collapse distinct remote names.
        let mut digest = Sha256::new();
        digest.update((username.len() as u64).to_le_bytes());
        digest.update(username.as_bytes());
        digest.update(remote.as_bytes());
        digest.update(size.to_le_bytes());
        let directory = PathBuf::from(format!(".slskr-{:x}", digest.finalize()));
        let filename = sanitize(remote.rsplit(['/', '\\']).next().unwrap_or_default());
        Self {
            root,
            directory,
            filename,
        }
    }

    fn parent(&self) -> io::Result<PathBuf> {
        confined_directory(&self.root, &self.directory, Some(0o700))
    }

    pub(super) fn offset(&self, resume: bool, expected: u64) -> io::Result<u64> {
        if !resume {
            return Ok(0);
        }
        let root = match self.root.canonicalize() {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let directory = root.join(&self.directory);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && directory.canonicalize()? == directory => (),
            Ok(_) => {
                return Err(io::Error::other(
                    "Incomplete subdirectory is not a real directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        }
        let path = directory.join(&self.filename);
        match regular(&path)? {
            // A partial with another name (for example, left behind when an
            // install linked it but could not remove the source) must never be
            // written through. Drop only our name and download from scratch.
            Some(metadata) if hard_linked(&metadata) => {
                unlink_partial(&path)?;
                Ok(0)
            }
            Some(metadata) if metadata.len() <= expected => Ok(metadata.len()),
            _ => Ok(0),
        }
    }

    pub(super) fn open(&self, offset: u64) -> io::Result<File> {
        let path = self.parent()?.join(&self.filename);
        let mut opts = OpenOptions::new();
        opts.write(true);
        let mut file = if regular(&path)?.is_some() {
            opts.open(&path)?
        } else if offset == 0 {
            opts.clone().create_new(true).open(&path)?
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Incomplete file disappeared",
            ));
        };
        let mut metadata = verify(&file, &path)?;
        if hard_linked(&metadata) {
            // Never write through another name: unlink ours and start over.
            drop(file);
            unlink_partial(&path)?;
            if offset != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Incomplete file was hard-linked and has been discarded",
                ));
            }
            file = opts.create_new(true).open(&path)?;
            metadata = verify(&file, &path)?;
            if hard_linked(&metadata) {
                return Err(io::Error::other("Incomplete file is hard-linked"));
            }
        }
        if offset == 0 {
            file.set_len(0)?;
        } else if metadata.len() != offset {
            return Err(io::Error::other("Incomplete file changed before resume"));
        }
        Ok(file)
    }

    pub(super) fn finish(
        &self,
        root: &Path,
        subdirectory: &Path,
        size: u64,
        overwrite: bool,
        mode: Option<u32>,
    ) -> io::Result<PathBuf> {
        let from_dir = self.parent()?;
        let source = from_dir.join(&self.filename);
        let mut file = open_shared(&source, size)?;
        if hard_linked(&file.metadata()?) {
            return Err(io::Error::other("Incomplete file is hard-linked"));
        }
        let destination = confined_directory(root, subdirectory, mode)?;
        set_file_mode(&file, mode)?;
        let mut candidate = destination.join(&self.filename);
        if source == candidate {
            return Ok(source);
        }
        let mut staged = None;
        // Set once the destination filesystem refuses hard links (exFAT, vfat,
        // CIFS, some FUSE); later attempts use an exclusive-create rename.
        let mut link_unsupported = false;
        let mut last_tick = 0_i128;
        loop {
            let input = staged
                .as_ref()
                .map_or(source.as_path(), |file: &Staged| file.0.as_path());
            let result = if overwrite {
                fs::rename(input, &candidate)
            } else if link_unsupported {
                rename_noreplace(input, &candidate)
            } else {
                fs::hard_link(input, &candidate)
            };
            match result {
                Ok(()) => {
                    let source_moved = staged.is_none() && (overwrite || link_unsupported);
                    if !source_moved && let Err(error) = fs::remove_file(&source) {
                        // The download is already installed; a stale partial
                        // is discarded by the next `offset`/`open`.
                        tracing::warn!(
                            source = %source.display(),
                            %error,
                            "failed to remove incomplete file after install",
                        );
                    }
                    let _ = fs::remove_dir(&from_dir);
                    return Ok(candidate);
                }
                Err(error)
                    if !overwrite
                        && !link_unsupported
                        && matches!(
                            error.kind(),
                            io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
                        ) =>
                {
                    link_unsupported = true;
                }
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices && staged.is_none() => {
                    staged = Some(Staged::copy(&mut file, &destination, mode)?);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && !overwrite => {
                    let name = Path::new(&self.filename);
                    let stem = name
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&self.filename);
                    last_tick = (last_tick + 1)
                        .max(Utc::now().timestamp_micros() as i128 * 10 + 621_355_968_000_000_000);
                    let mut renamed = format!("{stem}_{last_tick}");
                    if let Some(extension) = name.extension().and_then(|s| s.to_str()) {
                        renamed.push('.');
                        renamed.push_str(extension);
                    }
                    candidate.set_file_name(renamed);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

// Tokio can still have a blocking write in flight when Soulseek drops the
// stream on cancellation. The caller waits for `done` before retrying or moving
// the partial, so that write cannot race with the next operation.
pub(super) struct PartialOutput {
    file: Option<tokio::fs::File>,
    done: Option<oneshot::Sender<()>>,
}

impl PartialOutput {
    pub(super) fn new(file: File, done: oneshot::Sender<()>) -> Self {
        Self {
            file: Some(tokio::fs::File::from_std(file)),
            done: Some(done),
        }
    }

    fn pinned(&mut self) -> Pin<&mut tokio::fs::File> {
        Pin::new(self.file.as_mut().expect("partial output file"))
    }
}

impl AsyncWrite for PartialOutput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pinned().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pinned().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pinned().poll_shutdown(cx)
    }
}

impl AsyncSeek for PartialOutput {
    fn start_seek(mut self: Pin<&mut Self>, pos: io::SeekFrom) -> io::Result<()> {
        self.pinned().start_seek(pos)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        self.pinned().poll_complete(cx)
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        let file = self.file.take().expect("partial output file");
        let done = self.done.take();
        match file.try_into_std() {
            Ok(file) => {
                drop(file);
                drop(done);
            }
            Err(file) => {
                tokio::spawn(async move {
                    drop(file.into_std().await);
                    drop(done);
                });
            }
        }
    }
}

fn confined_directory(root: &Path, subdirectory: &Path, mode: Option<u32>) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let mut directory = root.canonicalize()?;
    for component in subdirectory.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("Invalid transfer subdirectory"));
        };
        directory.push(name);
        match fs::create_dir(&directory) {
            Ok(()) => {
                let dir_mode = mode.map(|m| 0o700 | m | ((m & 0o444) >> 2));
                set_mode(&directory, dir_mode)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        if !fs::symlink_metadata(&directory)?.is_dir() || directory.canonicalize()? != directory {
            return Err(io::Error::other(
                "Transfer subdirectory is not a real directory",
            ));
        }
    }
    Ok(directory)
}

fn regular(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(io::Error::other("Incomplete path is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn open_shared(path: &Path, size: u64) -> io::Result<File> {
    regular(path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Transfer file disappeared"))?;
    let file = File::open(path)?;
    if verify(&file, path)?.len() != size {
        return Err(io::Error::other("Transfer file length has changed"));
    }
    Ok(file)
}

fn verify(file: &File, path: &Path) -> io::Result<fs::Metadata> {
    let actual = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !actual.is_file() || !named.is_file() || path.canonicalize()? != path {
        return Err(io::Error::other("Transfer file escaped its directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if actual.dev() != named.dev() || actual.ino() != named.ino() {
            return Err(io::Error::other("Transfer file was replaced"));
        }
    }
    Ok(actual)
}

fn hard_linked(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() != 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn unlink_partial(path: &Path) -> io::Result<()> {
    tracing::warn!(
        path = %path.display(),
        "discarding hard-linked incomplete file",
    );
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Moves `from` to `to` without replacing an existing `to`, for filesystems
/// that cannot hard-link. The name is reserved with an exclusive create (which
/// does not follow symlinks), then the source is renamed over our own
/// placeholder. Fails with `AlreadyExists` when `to` is taken.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(to)?);
    fs::rename(from, to).inspect_err(|_| {
        let _ = fs::remove_file(to);
    })
}

fn set_file_mode(file: &File, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (file, mode);
    Ok(())
}

struct Staged(PathBuf);
impl Staged {
    fn copy(input: &mut File, dir: &Path, mode: Option<u32>) -> io::Result<Self> {
        let path = dir.join(format!(".slskr-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&path)?;
        let staged = Self(path);
        io::copy(input, &mut output)?;
        set_file_mode(&output, mode.or(Some(input_mode(input)?)))?;
        Ok(staged)
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn input_mode(file: &File) -> io::Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(file.metadata()?.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(0)
    }
}

fn set_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

pub(super) fn mode(value: Option<&str>) -> io::Result<Option<u32>> {
    let Some(value) = value.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(octal) = u32::from_str_radix(value, 8)
        && octal <= 0o7777
    {
        return Ok(Some(octal));
    }
    let permissions = value
        .strip_prefix('-')
        .filter(|p| p.len() == 9)
        .unwrap_or(value);
    if permissions.len() == 9 {
        let mut bits = 0;
        for (i, (&provided, &expected)) in
            permissions.as_bytes().iter().zip(b"rwxrwxrwx").enumerate()
        {
            if provided == expected {
                bits |= 1 << (8 - i);
            } else if provided != b'-' {
                return Err(io::Error::other("Invalid download permissions mode"));
            }
        }
        return Ok(Some(bits));
    }
    Err(io::Error::other("Invalid download permissions mode"))
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use super::*;
    use crate::transfers::new_transfer;
    use slskr_types::transfers::TransferDirection;

    fn rooted(filename: &str, root: &str) -> Transfer {
        let mut t = new_transfer(
            TransferDirection::Download,
            "peer",
            filename.into(),
            1,
            None,
        );
        t.root = Some(root.into());
        t
    }

    #[test]
    fn folder_layout_keeps_nested_structure() {
        let mut cfg = Config::default();
        cfg.downloads.subdirectory = "${SOURCE_USERNAME}".into();
        let root = r"@@music\Artist\Album";
        for (filename, expected) in [
            (r"@@music\Artist\Album\01.flac", "Album"),
            (r"@@music\Artist\Album\CD1\01.flac", "Album/CD1"),
            (r"@@music\Artist\Album\CD2\Bonus\01.flac", "Album/CD2/Bonus"),
            (r"@@music\Artist\Album\Artwork\\front.jpg", "Album/Artwork"),
            ("@@music\\Artist\\Album/Scans/back.jpg", "Album/Scans"),
        ] {
            assert_eq!(
                derive(&cfg, &rooted(filename, root), None, None),
                PathBuf::from(expected),
                "{filename}"
            );
        }
        // A trailing separator on the root changes nothing.
        assert_eq!(
            derive(&cfg, &rooted(r"a\Album\CD1\x", r"a\Album\"), None, None),
            PathBuf::from("Album/CD1")
        );
        // Without a root, the configured pattern still applies.
        let mut plain = rooted(r"a\Album\CD1\x", "");
        plain.root = None;
        assert_eq!(derive(&cfg, &plain, None, None), PathBuf::from("peer"));
        // A batch's explicit destination wins.
        let batch = Batch {
            id: "b".into(),
            search_id: None,
            username: "peer".into(),
            created_at: Utc::now(),
            destination: Some("Explicit".into()),
            external_id: None,
            transfers: Vec::new(),
        };
        assert_eq!(
            derive(&cfg, &rooted(r"a\Album\x", r"a\Album"), Some(&batch), None),
            PathBuf::from("Explicit")
        );
    }

    #[test]
    fn folder_layout_sanitizes_peer_components() {
        let cfg = Config::default();
        for (filename, root) in [
            (r"r\Album\..\..\etc\passwd", r"r\Album"),
            (r"r\Album\.\x", r"r\Album"),
            (r"r\Album\C:\Windows\x", r"r\Album"),
            ("r\\Album\\/abs\\x", r"r\Album"),
            (r"C:\Album\sub\x", r"C:\Album"),
            ("/home/u/Album/sub/x", "/home/u/Album"),
            (r"r\..\x", r"r\.."),
            (r"\\server\share\x", r"\\server\share"),
            (r"r\a<b>c|d\x", r"r\a<b>c|d"),
        ] {
            let path = derive(&cfg, &rooted(filename, root), None, None);
            assert!(
                path.components()
                    .all(|c| matches!(c, Component::Normal(name) if name != "." && name != "..")),
                "{filename}: {}",
                path.display()
            );
            assert!(path.components().count() >= 1, "{filename}");
        }
        assert_eq!(
            derive(&cfg, &rooted(r"r\Album\..\..\x", r"r\Album"), None, None),
            PathBuf::from("Album/_/_")
        );
        assert_eq!(
            derive(&cfg, &rooted(r"r\Album\C:\x", r"r\Album"), None, None),
            PathBuf::from("Album/C_")
        );
        assert_eq!(
            derive(&cfg, &rooted(r"r\..\x", r"r\.."), None, None),
            PathBuf::from("_")
        );
        // A stored root the file is not under keeps only the root's folder.
        assert_eq!(
            derive(&cfg, &rooted(r"other\sub\x", r"r\Album"), None, None),
            PathBuf::from("Album")
        );
    }

    #[test]
    fn folder_installs_confined_under_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let downloads = tmp.path().join("downloads");
        let cfg = Config::default();
        let t = rooted(r"r\Album\..\..\CD1\song.flac", r"r\Album");
        let subdirectory = derive(&cfg, &t, None, None);
        let part = Partial::new(tmp.path().join("incomplete"), "peer", &t.filename, 3);
        part.open(0).unwrap().write_all(b"abc").unwrap();
        let installed = part
            .finish(&downloads, &subdirectory, 3, false, None)
            .unwrap();
        assert!(installed.starts_with(downloads.canonicalize().unwrap()));
        assert!(installed.ends_with("Album/_/_/CD1/song.flac"));
    }

    #[test]
    fn root_matching() {
        assert_eq!(relative_to(r"a\b", r"a\b\c\d"), Some(r"c\d"));
        assert_eq!(relative_to(r"a\b\", r"a\b\c"), Some("c"));
        assert_eq!(relative_to(r"a\b", "a\\b/c"), Some("c"));
        assert_eq!(relative_to(r"a\b", r"a\bc\d"), None);
        assert_eq!(relative_to(r"a\b", r"a\b"), None);
        assert_eq!(relative_to(r"a\b", r"a\b\"), None);
        assert_eq!(relative_to(r"A\B", r"a\b\c"), None);
        assert_eq!(relative_to("", r"a\b"), None);
        assert!(within(r"a\b", r"a\b\"));
        assert!(within(r"a\b", r"a\b\c"));
        assert!(!within(r"a\b", r"a\bc"));
        assert_eq!(folder_name(r"@@music\Artist\Album\"), "Album");
        assert_eq!(folder_name("Album"), "Album");
    }

    #[test]
    fn resume_identity_includes_exact_filename_and_size() {
        let tmp = tempfile::tempdir().unwrap();
        let incomplete = tmp.path().join("incomplete");
        let part = Partial::new(incomplete.clone(), "peer", r"Album\song.flac", 6);
        part.open(0).unwrap().write_all(b"abc").unwrap();
        assert_eq!(part.offset(true, 6).unwrap(), 3);
        assert_eq!(part.offset(false, 6).unwrap(), 0);
        assert_eq!(
            Partial::new(incomplete.clone(), "peer", "Album/song.flac", 6)
                .offset(true, 6)
                .unwrap(),
            0
        );
        assert_eq!(
            Partial::new(incomplete, "peer", r"Album\song.flac", 7)
                .offset(true, 7)
                .unwrap(),
            0
        );

        let mut resumed = part.open(3).unwrap();
        resumed.seek(SeekFrom::Start(3)).unwrap();
        resumed.write_all(b"def").unwrap();
        drop(resumed);
        let completed = tmp.path().join("downloads");
        let album = Path::new("Album");
        fs::create_dir_all(completed.join(album)).unwrap();
        let collision = completed.join(album).join("song.flac");
        fs::write(&collision, b"existing").unwrap();
        let installed = part.finish(&completed, album, 6, false, None).unwrap();
        assert_eq!(fs::read(installed).unwrap(), b"abcdef");
        assert_eq!(fs::read(collision).unwrap(), b"existing");
        assert_eq!(part.offset(true, 6).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_destination_and_hard_link_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("downloads");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let part = Partial::new(tmp.path().join("partial"), "peer", "song.flac", 3);
        part.open(0).unwrap().write_all(b"abc").unwrap();
        assert!(
            part.finish(&root, Path::new("escape"), 3, false, None)
                .is_err()
        );
        assert_eq!(outside.read_dir().unwrap().count(), 0);
        let file = part.parent().unwrap().join(&part.filename);
        fs::hard_link(&file, outside.join("link")).unwrap();
        // A hard-linked partial is discarded, never written through.
        part.open(0).unwrap().write_all(b"xyz").unwrap();
        assert_eq!(fs::read(outside.join("link")).unwrap(), b"abc");
        assert_eq!(fs::read(&file).unwrap(), b"xyz");

        fs::hard_link(&file, outside.join("link2")).unwrap();
        assert_eq!(part.offset(true, 3).unwrap(), 0);
        assert!(!file.exists());
        assert_eq!(fs::read(outside.join("link2")).unwrap(), b"xyz");
    }

    #[test]
    fn rename_noreplace_never_clobbers() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let to = tmp.path().join("to");
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"existing").unwrap();
        let error = rename_noreplace(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"existing");

        let fresh = tmp.path().join("fresh");
        rename_noreplace(&from, &fresh).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&fresh).unwrap(), b"new");

        // A failed rename releases the reserved name.
        let missing = tmp.path().join("missing");
        let target = tmp.path().join("target");
        assert!(rename_noreplace(&missing, &target).is_err());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_succeeds_when_partial_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("downloads");
        let part = Partial::new(tmp.path().join("partial"), "peer", "song.flac", 3);
        part.open(0).unwrap().write_all(b"abc").unwrap();
        let from_dir = part.parent().unwrap();
        let source = from_dir.join(&part.filename);
        fs::set_permissions(&from_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let installed = part.finish(&root, Path::new(""), 3, false, None);
        fs::set_permissions(&from_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let installed = installed.unwrap();
        assert_eq!(fs::read(&installed).unwrap(), b"abc");
        // Root ignores directory permissions, so the source may be gone.
        if source.exists() {
            assert_eq!(part.offset(true, 3).unwrap(), 0);
            assert!(!source.exists());
            assert_eq!(fs::read(&installed).unwrap(), b"abc");
        }
    }
}
