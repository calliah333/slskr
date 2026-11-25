//! Local share snapshots, safe path resolution, and `/shares` endpoints.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use slskr_types::{config::Config, shares::Share};
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult, Json, Path as RoutePath, Routes},
    users::convert_directory,
};

mod audio;
mod index;
mod scan;
use scan::ShareSpec;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ScanOptions {
    directories: Vec<String>,
    filters: Vec<String>,
}

impl ScanOptions {
    fn new(options: &Config) -> Self {
        Self {
            directories: options.shares.directories.clone(),
            filters: options.shares.exclude.clone(),
        }
    }

    fn matches(&self, options: &Config) -> bool {
        self.directories == options.shares.directories && self.filters == options.shares.exclude
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    options: ScanOptions,
    scanned_at: DateTime<Utc>,
    shares: BTreeMap<String, ShareContents>,
    /// Built from `shares` on first use; never written to the share cache.
    #[serde(skip)]
    index: OnceLock<Option<index::SearchIndex>>,
    /// The SharesReply frame answering browse requests, encoded on first use.
    #[serde(skip)]
    browse: OnceLock<bytes::Bytes>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ShareContents {
    root: PathBuf,
    configured_root: PathBuf,
    directories: BTreeMap<String, Vec<ShareFile>>,
    file_count: u32,
}

impl ShareContents {
    fn directories(&self) -> impl Iterator<Item = soulseek::Directory> + '_ {
        self.directories
            .iter()
            .map(|(name, files)| directory(name, files))
    }

    fn resolve(&self, remote: &str, parent: &str, name: &str) -> Result<(PathBuf, u64)> {
        let files = self.directories.get(parent).context("file is not shared")?;
        ensure!(
            files
                .binary_search_by(|file| file.name.as_str().cmp(name))
                .is_ok(),
            "file is not shared"
        );

        // The recorded root is canonical. Reject a replaced root or a symlink
        // introduced after the scan, even if it happens to target another share.
        ensure!(
            fs::symlink_metadata(&self.configured_root)?.is_dir()
                && self.configured_root.canonicalize()? == self.root,
            "share root has changed"
        );
        let mut candidate = self.root.clone();
        candidate.extend(remote.split('\\').skip(1));
        let canonical = candidate.canonicalize()?;
        ensure!(
            canonical == candidate && canonical.starts_with(&self.root),
            "file is outside its share"
        );
        let metadata = canonical.metadata()?;
        ensure!(metadata.is_file(), "shared path is not a regular file");
        Ok((canonical, metadata.len()))
    }
}

/// Audio properties in the share cache, named as written by earlier versions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum NamedAttribute {
    BitRate,
    Length,
    VariableBitRate,
    SampleRate,
    BitDepth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attribute {
    #[serde(rename = "type")]
    kind: AttributeType,
    value: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
enum AttributeType {
    Named(NamedAttribute),
    Number(i32),
}

impl Attribute {
    fn wire(&self) -> soulseek::FileAttribute {
        use soulseek::FileAttributeType as Wire;
        let kind = match self.kind {
            AttributeType::Named(NamedAttribute::BitRate) => Wire::BitRate,
            AttributeType::Named(NamedAttribute::Length) => Wire::Duration,
            AttributeType::Named(NamedAttribute::VariableBitRate) => Wire::VariableBitRate,
            AttributeType::Named(NamedAttribute::SampleRate) => Wire::SampleRate,
            AttributeType::Named(NamedAttribute::BitDepth) => Wire::BitDepth,
            AttributeType::Number(number) => Wire::from(number as u32),
        };
        soulseek::FileAttribute::new(kind, self.value as u32)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ShareFile {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<DateTime<Utc>>,
    size: u64,
    extension: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attributes: Vec<Attribute>,
}

impl Snapshot {
    fn files(&self, parent: &str) -> &[ShareFile] {
        self.shares
            .get(remote_root(parent))
            .and_then(|share| share.directories.get(parent))
            .map_or(&[], Vec::as_slice)
    }

    /// The search index, building it on first use; `None` when the snapshot
    /// is too large to index and searches scan it linearly.
    fn search_index(&self) -> Option<&index::SearchIndex> {
        self.index
            .get_or_init(|| index::SearchIndex::build(&self.shares))
            .as_ref()
    }

    /// Collects matches in search order: shares, directories, then files.
    fn search(&self, collect: &mut SearchMatches) {
        let Some(index) = self.search_index() else {
            return self.search_linear(collect);
        };
        let Some(lists) = index.candidates(&collect.matcher) else {
            return self.search_linear(collect);
        };
        let mut directory = (usize::MAX, &[][..]);
        for id in index::intersect(&lists) {
            let located = index.locate(id);
            if located.directory != directory.0 {
                directory = (located.directory, self.files(located.name));
                collect.parent(located.name);
            }
            let file = &directory.1[located.file];
            collect.file(&file.name, |filename| file.wire(filename));
            if collect.is_full() {
                break;
            }
        }
    }

    /// The SharesReply frame for browse requests: encoded once, at the highest
    /// compression level, since every browse of this snapshot sends the same bytes.
    fn browse_frame(&self) -> bytes::Bytes {
        self.browse
            .get_or_init(|| self.browse_response().to_bytes_best())
            .clone()
    }

    fn browse_response(&self) -> soulseek::BrowseResponse {
        soulseek::BrowseResponse {
            directories: self
                .shares
                .values()
                .flat_map(ShareContents::directories)
                .collect(),
            locked_directories: Vec::new(),
        }
    }

    /// Tests every shared file; the reference for [`Snapshot::search`].
    fn search_linear(&self, collect: &mut SearchMatches) {
        for share in self.shares.values() {
            for (parent, files) in &share.directories {
                if files.is_empty() {
                    continue;
                }
                collect.parent(parent);
                for file in files {
                    collect.file(&file.name, |filename| file.wire(filename));
                    if collect.is_full() {
                        return;
                    }
                }
            }
        }
    }
}

impl ShareFile {
    fn wire(&self, filename: String) -> soulseek::File {
        soulseek::File {
            code: 1,
            filename,
            size: self.size,
            extension: self.extension.clone(),
            attributes: self.attributes.iter().map(Attribute::wire).collect(),
        }
    }
}

#[derive(Clone)]
pub struct ShareService(Arc<Inner>);

struct Inner {
    core: Core,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    active: Mutex<Option<CancellationToken>>,
    /// `db/shares-cache.json`: the last scan, restored at startup.
    cache_path: PathBuf,
    changed: watch::Sender<()>,
}

impl ShareService {
    pub fn new(core: &Core) -> Result<Self> {
        let options = core.options.get();
        let cache_path = core.paths.db_dir.join("shares-cache.json");
        let snapshot = {
            let path = cache_path.as_path();
            remove_partial(path);
            let result = File::open(path)
                .context("opening share cache")
                .and_then(|file| {
                    serde_json::from_reader::<_, Snapshot>(BufReader::new(file))
                        .context("reading share cache")
                });
            match result {
                Ok(snapshot)
                    if snapshot.options.matches(&options)
                        && snapshot_has_configured_roots(&snapshot) =>
                {
                    snapshot.search_index();
                    Some(Arc::new(snapshot))
                }
                Ok(_) => None,
                Err(error) => {
                    if !error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
                    {
                        tracing::warn!(%error, path = %path.display(), "unable to restore share cache");
                    }
                    None
                }
            }
        };
        Ok(Self(Arc::new(Inner {
            core: core.clone(),
            snapshot: RwLock::new(snapshot),
            active: Mutex::new(None),
            cache_path,
            changed: watch::channel(()).0,
        })))
    }

    pub fn start(&self) {
        self.refresh_state();
        let core = &self.0.core;
        let startup = core.options.get();
        let mut options = core.options.subscribe();
        let mut events = core.client.subscribe();
        let mut changed = self.0.changed.subscribe();
        let service = self.clone();
        tokio::spawn(async move {
            let mut configured = ScanOptions::new(&options.borrow_and_update());
            let hour = Duration::from_secs(3600);
            let mut rescan = tokio::time::interval_at(tokio::time::Instant::now() + hour, hour);
            service.announce().await;
            loop {
                tokio::select! {
                    _ = service.0.core.shutdown.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(soulseek::Event::LoggedIn) |
                        Err(broadcast::error::RecvError::Lagged(_)) => service.announce().await,
                        Err(broadcast::error::RecvError::Closed) => break,
                        _ => {}
                    },
                    update = changed.changed() => {
                        if update.is_err() { break; }
                        service.announce().await;
                    },
                    update = options.changed() => {
                        if update.is_err() { break; }
                        let next = options.borrow_and_update().clone();
                        if !configured.matches(&next) {
                            configured = ScanOptions::new(&next);
                            service.refresh_state();
                            service.0.changed.send_replace(());
                        }
                    },
                    _ = rescan.tick() => {
                        let options = service.0.core.options.get();
                        if options.shares.rescan_interval.is_some() && service.needs_scan(&options)
                        {
                            service.launch_scan(service.0.core.shutdown.child_token());
                        }
                    },
                }
            }
        });
        if self.needs_scan(&startup) {
            self.launch_scan(core.shutdown.child_token());
        }
    }

    fn needs_scan(&self, options: &Config) -> bool {
        self.snapshot_for(options).is_none_or(|snapshot| {
            options.shares.rescan_interval.is_some_and(|interval| {
                Utc::now()
                    .signed_duration_since(snapshot.scanned_at)
                    .to_std()
                    .is_ok_and(|age| age >= interval)
            })
        })
    }

    fn refresh_state(&self) {
        let active = self.0.active.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = self.snapshot();
        let counts = snapshot.as_deref().map_or((0, 0), snapshot_counts);
        self.0.core.state.update(|s| {
            let state = &mut s.shares;
            state.ready = active.is_none() && snapshot.is_some();
            state.scan_pending = snapshot.is_none() || state.faulted || state.cancelled;
            state.directories = counts.0;
            state.files = counts.1;
            if active.is_none() {
                state.scan_progress = if snapshot.is_some() { 1.0 } else { 0.0 };
            }
        });
    }

    async fn announce(&self) {
        let client = &self.0.core.client;
        if client.state().is_logged_in() {
            let (directories, files) = self.counts();
            if let Err(error) = client.set_shared_counts(directories, files).await {
                tracing::warn!(%error, "failed to announce current share counts");
            }
        }
    }

    pub async fn scan(&self, cancel: CancellationToken) -> Result<()> {
        self.launch_scan(cancel)
            .context("A share scan is already in progress")?
            .await?
    }

    pub(crate) fn launch_scan(&self, cancel: CancellationToken) -> Option<JoinHandle<Result<()>>> {
        let mut active = self.0.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active.is_some() {
            return None;
        }
        *active = Some(cancel.clone());
        self.0.core.state.update(|s| {
            s.shares.scanning = true;
            s.shares.ready = false;
            s.shares.cancelled = false;
            s.shares.faulted = false;
            s.shares.scan_progress = 0.0;
            s.shares.scan_files = 0;
        });
        let options = ScanOptions::new(&self.0.core.options.get());
        let service = self.clone();
        Some(tokio::spawn(async move {
            let worker = service.clone();
            let result =
                tokio::task::spawn_blocking(move || worker.scan_and_publish(options, &cancel))
                    .await;
            match result {
                Ok(result) => result,
                Err(error) => {
                    let mut active = service
                        .0
                        .active
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    service.finish_scan(
                        Err(error.into()),
                        active.take().is_some_and(|c| c.is_cancelled()),
                    )
                }
            }
        }))
    }

    fn scan_and_publish(&self, options: ScanOptions, cancel: &CancellationToken) -> Result<()> {
        tracing::info!("scanning shares");
        let previous = self.snapshot();
        let previous_files = previous
            .as_deref()
            .map(snapshot_counts)
            .map_or(0, |(_, n)| n);
        let snapshot = scan::scan_roots(options, previous.as_deref(), cancel, |files| {
            self.0.core.state.update(|s| {
                s.shares.scan_files = files;
                if previous_files != 0 {
                    s.shares.scan_progress =
                        (f64::from(files) / f64::from(previous_files)).min(0.99);
                }
            });
        })
        .and_then(|snapshot| {
            ensure!(!cancel.is_cancelled(), "share scan cancelled");
            let path = &self.0.cache_path;
            fs::create_dir_all(path.parent().expect("share cache parent"))?;
            let mut writer = BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(partial_path(path))?,
            );
            serde_json::to_writer(&mut writer, &snapshot)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            // Index on this blocking thread, not in the first incoming search.
            snapshot.search_index();
            // Likewise encode the share list here, not in the first browse.
            snapshot.browse_frame();
            Ok(snapshot)
        });
        let mut active = self.0.active.lock().unwrap_or_else(PoisonError::into_inner);
        let cancelled = cancel.is_cancelled();
        let result = snapshot.and_then(|snapshot| {
            ensure!(!cancelled, "share scan cancelled");
            ensure!(
                snapshot.options.matches(&self.0.core.options.get()),
                "share configuration changed during the scan"
            );
            let path = &self.0.cache_path;
            fs::rename(partial_path(path), path)?;
            *self
                .0
                .snapshot
                .write()
                .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(snapshot));
            Ok(())
        });
        if result.is_err() {
            remove_partial(&self.0.cache_path);
        }
        *active = None;
        self.finish_scan(result, cancelled)
    }

    fn finish_scan(&self, result: Result<()>, cancelled: bool) -> Result<()> {
        let snapshot = self.snapshot();
        let (directories, files) = snapshot.as_deref().map_or((0, 0), snapshot_counts);
        self.0.core.state.update(|s| {
            s.shares.scanning = false;
            s.shares.ready = snapshot.is_some();
            s.shares.scan_pending = result.is_err() || snapshot.is_none();
            s.shares.cancelled = cancelled;
            s.shares.faulted = !cancelled && result.is_err();
            s.shares.scan_progress = if result.is_ok() { 1.0 } else { 0.0 };
            s.shares.directories = directories;
            s.shares.files = files;
        });
        match &result {
            Ok(()) => {
                tracing::info!(directories, files, "share scan completed");
                self.0.changed.send_replace(());
            }
            Err(_) if cancelled => tracing::info!("share scan cancelled"),
            Err(error) => tracing::warn!(%error, "share scan failed"),
        }
        result
    }

    fn cancel_scan(&self) -> bool {
        self.0
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|token| {
                token.cancel();
                true
            })
    }

    fn snapshot_for(&self, options: &Config) -> Option<Arc<Snapshot>> {
        self.0
            .snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|snapshot| snapshot.options.matches(options))
            .cloned()
    }

    fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot_for(&self.0.core.options.get())
    }

    /// Whether shared files can be resolved: a scan of the current share settings exists.
    pub fn is_indexed(&self) -> bool {
        self.snapshot().is_some()
    }

    pub fn counts(&self) -> (u32, u32) {
        self.snapshot().as_deref().map_or((0, 0), snapshot_counts)
    }

    /// The encoded browse response; blocks while a new snapshot's frame is encoded.
    pub fn browse_frame(&self) -> bytes::Bytes {
        self.snapshot().map_or_else(
            || soulseek::BrowseResponse::default().to_bytes(),
            |snapshot| snapshot.browse_frame(),
        )
    }

    pub fn list_directory(&self, directory_name: &str) -> Result<Vec<soulseek::Directory>> {
        let name = normalize_remote(directory_name)?;
        let local = self.snapshot();
        let local_files = local
            .as_deref()
            .map_or(&[][..], |snapshot| snapshot.files(&name));
        Ok(vec![directory(&name, local_files)])
    }

    pub fn resolve_file(&self, remote_filename: &str) -> Result<(PathBuf, u64)> {
        let remote = normalize_remote(remote_filename)?;
        let (parent, name) = remote
            .rsplit_once('\\')
            .context("missing shared file parent")?;
        let snapshot = self.snapshot().context("shares have not been scanned")?;
        let share = snapshot
            .shares
            .get(remote_root(parent))
            .context("file is not shared")?;
        share.resolve(&remote, parent, name)
    }

    /// Returns at most `limit` shared files matching `query`, in share order.
    /// Candidates come from the snapshot's word index and collection stops
    /// once the limit is reached, so broad queries stay bounded.
    pub fn search(
        &self,
        query: &soulseek::SearchQuery,
        limit: usize,
    ) -> Result<Vec<soulseek::File>> {
        let mut collect = SearchMatches {
            matcher: query.matcher(),
            limit,
            filename: String::new(),
            matches: Vec::new(),
        };
        if collect.is_full() || collect.matcher.is_empty() {
            return Ok(collect.matches);
        }
        if let Some(snapshot) = &self.snapshot() {
            snapshot.search(&mut collect);
        }
        Ok(collect.matches)
    }

    pub fn shares(&self) -> Vec<Share> {
        let options = self.0.core.options.get();
        let snapshot = self.snapshot_for(&options);
        local_shares(&options, snapshot.as_deref())
    }
}

struct SearchMatches {
    matcher: soulseek::SearchMatcher,
    limit: usize,
    filename: String,
    matches: Vec<soulseek::File>,
}

impl SearchMatches {
    fn is_full(&self) -> bool {
        self.matches.len() >= self.limit
    }

    fn parent(&mut self, parent: &str) {
        self.filename.clear();
        self.filename.push_str(parent);
        self.filename.push('\\');
    }

    fn file(&mut self, name: &str, wire: impl FnOnce(String) -> soulseek::File) {
        let prefix = self.filename.len();
        self.filename.push_str(name);
        if self.matcher.matches(&self.filename) {
            self.matches.push(wire(self.filename.clone()));
        }
        self.filename.truncate(prefix);
    }
}

fn snapshot_counts(snapshot: &Snapshot) -> (u32, u32) {
    snapshot
        .shares
        .values()
        .fold((0u32, 0u32), |(directories, files), share| {
            (
                directories.saturating_add(count(share.directories.len())),
                files.saturating_add(share.file_count),
            )
        })
}

fn snapshot_has_configured_roots(snapshot: &Snapshot) -> bool {
    let configured: BTreeMap<_, _> = scan::specs(&snapshot.options.directories)
        .filter(|spec| !spec.excluded)
        .map(|spec| (spec.alias, spec.local))
        .collect();
    snapshot.shares.iter().all(|(alias, share)| {
        configured
            .get(alias.as_str())
            .is_some_and(|&path| path == share.configured_root)
    })
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
fn local_shares(options: &Config, snapshot: Option<&Snapshot>) -> Vec<Share> {
    let mut specs: Vec<_> = scan::specs(&options.shares.directories).collect();
    specs.sort_by_key(|spec| std::cmp::Reverse(spec.local.as_os_str().len()));
    specs
        .into_iter()
        .map(|spec| share_model(spec, snapshot))
        .collect()
}

fn share_model(spec: ShareSpec<'_>, snapshot: Option<&Snapshot>) -> Share {
    let totals = snapshot.map(|snapshot| {
        snapshot
            .shares
            .get(spec.alias)
            .filter(|_| !spec.excluded)
            .map_or((0, 0), |contents| {
                (
                    count(contents.directories.len().saturating_sub(1)),
                    contents.file_count,
                )
            })
    });
    Share {
        id: spec.id(),
        alias: Some(spec.alias.into()),
        is_excluded: spec.excluded,
        local_path: Some(spec.local.to_string_lossy().into_owned()),
        raw: spec.raw.into(),
        remote_path: Some(spec.alias.into()),
        directories: totals.map(|(dirs, _)| dirs),
        files: totals.map(|(_, files)| files),
    }
}

fn directory(name: &str, files: &[ShareFile]) -> soulseek::Directory {
    soulseek::Directory {
        name: name.into(),
        files: files
            .iter()
            .map(|file| file.wire(file.name.clone()))
            .collect(),
    }
}

fn partial_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

fn remove_partial(path: &Path) {
    let partial = partial_path(path);
    if let Err(error) = fs::remove_file(&partial)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %partial.display(), "unable to remove incomplete share cache");
    }
}

/// Checks share specifications without touching the filesystem: valid aliases, absolute
/// paths and no duplicate entries or aliases.
pub(crate) fn validate_directories(directories: &[String]) -> Result<()> {
    let mut raw = std::collections::HashSet::new();
    let mut aliases = std::collections::HashSet::new();
    for spec in directories.iter().map(|spec| scan::ShareSpec::parse(spec)) {
        ensure!(raw.insert(spec.raw), "duplicate entry {:?}", spec.raw);
        spec.validate()?;
        ensure!(
            aliases.insert(spec.alias),
            "duplicate share alias {:?}",
            spec.alias
        );
    }
    Ok(())
}

fn remote_root(path: &str) -> &str {
    path.split_once('\\').map_or(path, |(root, _)| root)
}

fn valid_remote_component(part: &str) -> bool {
    !matches!(part, "" | "." | "..") && !part.contains(['\0', ':', '/', '\\'])
}

fn normalize_remote(path: &str) -> Result<Cow<'_, str>> {
    ensure!(
        path.split(['/', '\\']).all(valid_remote_component),
        "invalid remote share path"
    );
    Ok(if path.contains('/') {
        Cow::Owned(path.replace('/', "\\"))
    } else {
        Cow::Borrowed(path)
    })
}

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/shares", shares)
        .post("/api/shares/scan", start_scan)
        .delete("/api/shares/scan", cancel_scan)
        .get("/api/shares/{id}", share)
        .get("/api/shares/{id}/contents", share_contents)
}

async fn shares(_: Authenticated, State(s): State<AppState>) -> Json<Vec<Share>> {
    Json(s.services.shares.shares())
}

async fn share(
    _: Authenticated,
    State(s): State<AppState>,
    RoutePath(id): RoutePath<String>,
) -> ApiResult<Json<Share>> {
    let service = &s.services.shares;
    let options = s.core.options.get();
    let spec = scan::specs(&options.shares.directories)
        .find(|spec| spec.id() == id)
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(share_model(
        spec,
        service.snapshot_for(&options).as_deref(),
    )))
}

async fn share_contents(
    _: Authenticated,
    State(s): State<AppState>,
    RoutePath(id): RoutePath<String>,
) -> ApiResult<Json<Vec<slskr_types::users::Directory>>> {
    let options = s.core.options.get();
    let spec = scan::specs(&options.shares.directories)
        .find(|spec| spec.id() == id)
        .ok_or_else(ApiError::not_found)?;
    let snapshot = s.services.shares.snapshot_for(&options);
    let directories = snapshot
        .as_ref()
        .filter(|_| !spec.excluded)
        .and_then(|snapshot| snapshot.shares.get(spec.alias))
        .map(|contents| {
            contents
                .directories()
                .filter(|directory| directory.name != spec.alias || !directory.files.is_empty())
                .map(convert_directory)
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(directories))
}

/// Starts a share scan (202); 409 while one is running.
async fn start_scan(_: RequireReadWrite, State(s): State<AppState>) -> ApiResult<StatusCode> {
    s.services
        .shares
        .launch_scan(s.core.shutdown.child_token())
        .ok_or_else(|| ApiError::Conflict("A share scan is already in progress".into()))?;
    Ok(StatusCode::ACCEPTED)
}

/// Cancels the running share scan (204); 404 when none is running.
async fn cancel_scan(_: RequireReadWrite, State(s): State<AppState>) -> ApiResult<StatusCode> {
    if s.services.shares.cancel_scan() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("No share scan is running".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_paths_reject_traversal_and_normalize_separators() {
        assert!(normalize_remote("music\\..\\secret.mp3").is_err());
        assert_eq!(
            normalize_remote("music/album/song.mp3").unwrap(),
            "music\\album\\song.mp3"
        );
        assert!(normalize_remote("C:\\outside.mp3").is_err());
    }
    #[test]
    fn remote_paths_reject_absolute_and_ambiguous_components() {
        for path in [
            "",
            "\\\\server\\share",
            "/absolute",
            "music//song",
            "music\\.",
            "music\\bad\0name",
        ] {
            assert!(normalize_remote(path).is_err(), "{path:?}");
        }
        assert_eq!(remote_root("music\\album\\song.mp3"), "music");
    }

    #[tokio::test]
    async fn local_shares_browse_and_search_within_limits() {
        use crate::{
            app::Paths,
            auth::{Auth, AuthConfig, SessionStore},
            config::{ConfigStore, Overrides, Sources},
            state::StateStore,
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("first.txt"), b"local").unwrap();
        fs::write(root.join("second.txt"), b"other").unwrap();
        let config_file = temp.path().join("slskr.toml");
        let mut config = slskr_types::config::Config::default();
        config.shares.directories = vec![format!("[music]{}", root.display())];
        let sources = Sources::new(temp.path().into(), Vec::new(), Overrides::default()).unwrap();
        let options = ConfigStore::from_config(config_file.clone(), sources, config);
        let core = Core {
            paths: Paths {
                data_dir: temp.path().into(),
                db_dir: temp.path().join("db"),
                config_file,
                logs_dir: temp.path().join("logs"),
                web_dir: None,
            },
            options,
            state: StateStore::new(),
            client: soulseek::SoulseekClient::new(
                Default::default(),
                Arc::new(soulseek::resolvers::DefaultResolvers),
            ),
            bus: Default::default(),
            auth: Auth::new(
                AuthConfig {
                    disabled: true,
                    ..AuthConfig::default()
                },
                SessionStore::memory(std::time::Duration::from_secs(3_600)),
            ),
            logs: Default::default(),
            shutdown: CancellationToken::new(),
        };
        let shares = ShareService::new(&core).unwrap();
        shares.scan(CancellationToken::new()).await.unwrap();
        assert_eq!(shares.shares()[0].alias.as_deref(), Some("music"));
        let frame = shares.browse_frame();
        assert_eq!(
            frame.as_ptr(),
            shares.browse_frame().as_ptr(),
            "the scan encodes the frame once"
        );
        let soulseek::protocol::peer::PeerIncoming::BrowseResponse(browse) =
            soulseek::protocol::peer::PeerIncoming::decode(frame.slice(4..)).unwrap()
        else {
            panic!("not a browse response");
        };
        assert_eq!(browse.directories.len(), 1);
        assert_eq!(shares.list_directory("music").unwrap()[0].files[0].size, 5);
        let found = shares
            .search(&soulseek::SearchQuery::parse("second"), usize::MAX)
            .unwrap();
        assert_eq!(found[0].filename, "music\\second.txt");
        // The response limit caps collection.
        let txt = soulseek::SearchQuery::parse("txt");
        assert_eq!(shares.search(&txt, usize::MAX).unwrap().len(), 2);
        let capped = shares.search(&txt, 1).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].filename, "music\\first.txt");
        assert!(shares.search(&txt, 0).unwrap().is_empty());
        fs::remove_file(root.join("first.txt")).unwrap();
        assert!(shares.resolve_file("music\\first.txt").is_err());
    }

    /// Words for synthetic shares: diacritics, digits, punctuation, words
    /// that are suffixes of others, and a non-Latin script.
    const WORDS: &[&str] = &[
        "Pink",
        "Floyd",
        "pink",
        "FLOYD",
        "Beyoncé",
        "beyonce",
        "halo",
        "Hello",
        "jello",
        "ello",
        "AC-DC",
        "ac",
        "dc",
        "Live",
        "live_at",
        "Dogs",
        "dog",
        "01",
        "02",
        "2001",
        "the",
        "The",
        "Café",
        "cafe",
        "Ünder",
        "under",
        "Сон",
        "сон",
        "a",
        "b",
        "mp3",
        "x.y",
        "Mötley",
        "Crüe",
        "remaster",
        "remastered",
        "+",
        "(",
        ")",
        "-",
        "[",
        "]",
    ];

    fn phrase(
        rng: &mut impl rand::Rng,
        words: std::ops::Range<usize>,
        separators: &[&str],
    ) -> String {
        use rand::seq::IndexedRandom;
        let mut text = String::new();
        for i in 0..rng.random_range(words) {
            if i > 0 {
                text.push_str(separators.choose(rng).unwrap());
            }
            text.push_str(WORDS.choose(rng).unwrap());
        }
        text
    }

    /// Each file name also gets one of `rare` generated words (`w0`, `w1`, ...).
    fn synthetic_snapshot(
        seed: u64,
        shares: usize,
        dirs: usize,
        files: usize,
        rare: u32,
    ) -> Snapshot {
        use rand::{Rng, SeedableRng};
        let rng = &mut rand::rngs::StdRng::seed_from_u64(seed);
        let separators = [" ", " - ", "_", ".", ""];
        let mut snapshot = Snapshot {
            options: ScanOptions {
                directories: Vec::new(),
                filters: Vec::new(),
            },
            scanned_at: Utc::now(),
            shares: BTreeMap::new(),
            index: OnceLock::new(),
            browse: OnceLock::new(),
        };
        for s in 0..shares {
            let alias = format!(
                "share{s}{}",
                phrase(rng, 1..2, &[""]).replace(['\\', '/'], "")
            );
            let mut directories = BTreeMap::new();
            let mut file_count = 0u32;
            directories.insert(alias.clone(), Vec::new());
            for _ in 0..dirs {
                let mut name = alias.clone();
                for _ in 0..rng.random_range(1..4) {
                    name.push('\\');
                    name.push_str(&phrase(rng, 1..4, &separators));
                }
                let count = rng.random_range(0..=files);
                let mut entries: Vec<ShareFile> = (0..count)
                    .map(|_| ShareFile {
                        name: format!(
                            "{} w{}.mp3",
                            phrase(rng, 1..5, &separators),
                            rng.random_range(0..rare.max(1))
                        ),
                        modified: None,
                        size: 1,
                        extension: "mp3".into(),
                        attributes: Vec::new(),
                    })
                    .collect();
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                entries.dedup_by(|a, b| a.name == b.name);
                file_count += count as u32;
                directories.entry(name).or_insert(entries);
            }
            snapshot.shares.insert(
                alias,
                ShareContents {
                    root: PathBuf::new(),
                    configured_root: PathBuf::new(),
                    directories,
                    file_count,
                },
            );
        }
        snapshot
    }

    fn run_search(
        snapshot: &Snapshot,
        query: &soulseek::SearchQuery,
        limit: usize,
        linear: bool,
    ) -> Vec<String> {
        let mut collect = SearchMatches {
            matcher: query.matcher(),
            limit,
            filename: String::new(),
            matches: Vec::new(),
        };
        if !collect.is_full() && !collect.matcher.is_empty() {
            if linear {
                snapshot.search_linear(&mut collect);
            } else {
                snapshot.search(&mut collect);
            }
        }
        collect
            .matches
            .into_iter()
            .map(|file| file.filename)
            .collect()
    }

    #[test]
    fn indexed_search_matches_linear_scan() {
        use rand::{Rng, SeedableRng, seq::IndexedRandom};
        let snapshot = synthetic_snapshot(7, 3, 60, 12, 40);
        assert!(snapshot.search_index().is_some());
        let rng = &mut rand::rngs::StdRng::seed_from_u64(11);
        let mut queries: Vec<String> = [
            "",
            " ",
            "-",
            "*",
            "!",
            "pink floyd",
            "PINK",
            "beyonce",
            "BEYONCÉ",
            "*ello",
            "*LO",
            "*o",
            "*",
            "ac/dc",
            "ac-dc -live",
            "the -the",
            "mp3",
            "mp3 -pink",
            "-pink",
            "сон",
            "*он",
            "café",
            "a b",
            "share0",
            "share1pink",
            "missingword",
            "pink missingword",
            "*zzz",
            "remaster -remastered",
            "x.y",
            "Mötley Crüe",
            "01 -0",
            "*a *b",
        ]
        .iter()
        .map(|q| (*q).to_owned())
        .collect();
        for _ in 0..400 {
            let mut query = String::new();
            for i in 0..rng.random_range(1..4) {
                if i > 0 {
                    query.push(' ');
                }
                match rng.random_range(0..10) {
                    0 => query.push('-'),
                    1 => query.push('*'),
                    _ => {}
                }
                query.push_str(&phrase(rng, 1..3, &["-", "", "_", "."]));
            }
            queries.push(query);
        }
        let mut nonempty = 0;
        for text in &queries {
            let query = soulseek::SearchQuery::parse(text);
            for limit in [0, 1, 3, 50, usize::MAX] {
                let expected = run_search(&snapshot, &query, limit, true);
                assert_eq!(
                    run_search(&snapshot, &query, limit, false),
                    expected,
                    "{text:?} limit {limit}"
                );
                nonempty += usize::from(!expected.is_empty());
            }
        }
        assert!(nonempty > queries.len(), "corpus should produce matches");
        // An exclusion phrase made of several words still filters.
        let exclusions = soulseek::SearchQuery::with_exclusions(
            WORDS.choose(rng).unwrap(),
            vec!["pink floyd".into(), "x.y".into()],
        );
        assert_eq!(
            run_search(&snapshot, &exclusions, usize::MAX, false),
            run_search(&snapshot, &exclusions, usize::MAX, true)
        );
    }

    #[test]
    fn index_handles_words_shared_by_directories_and_files() {
        let mut snapshot = synthetic_snapshot(1, 1, 0, 0, 0);
        let share = snapshot.shares.values_mut().next().unwrap();
        let alias = share.directories.keys().next().unwrap().clone();
        let file = |name: &str| ShareFile {
            name: name.into(),
            modified: None,
            size: 1,
            extension: String::new(),
            attributes: Vec::new(),
        };
        share.directories.insert(
            format!("{alias}\\Pink Pink"),
            vec![file("floyd pink.mp3"), file("other.mp3"), file("pink.mp3")],
        );
        share
            .directories
            .insert(format!("{alias}\\z"), vec![file("pink.mp3")]);
        let pink = soulseek::SearchQuery::parse("pink");
        let found = run_search(&snapshot, &pink, usize::MAX, false);
        assert_eq!(found, run_search(&snapshot, &pink, usize::MAX, true));
        assert_eq!(found.len(), 4);
    }

    /// Times 1000 searches over ~100k files:
    /// `cargo test --release -p slskr search_benchmark -- --ignored --nocapture`

    #[test]
    #[ignore = "benchmark"]
    fn browse_frame_benchmark() {
        use std::time::Instant;
        let snapshot = synthetic_snapshot(3, 4, 2_500, 20, 50_000);
        let response = snapshot.browse_response();
        let started = Instant::now();
        let default = response.to_bytes();
        let default_time = started.elapsed();
        let started = Instant::now();
        let best = response.to_bytes_best();
        let best_time = started.elapsed();
        println!(
            "{} files: default {} bytes in {default_time:?}, best {} bytes in {best_time:?} ({:.1}% smaller)",
            snapshot_counts(&snapshot).1,
            default.len(),
            best.len(),
            100.0 * (1.0 - best.len() as f64 / default.len() as f64)
        );
    }
    #[test]
    #[ignore = "benchmark"]
    fn search_benchmark() {
        use rand::{Rng, SeedableRng, seq::IndexedRandom};
        use std::time::Instant;
        let started = Instant::now();
        let snapshot = synthetic_snapshot(3, 4, 2_500, 20, 50_000);
        let files = snapshot_counts(&snapshot).1;
        let generated = started.elapsed();
        let started = Instant::now();
        let index = snapshot.search_index().unwrap();
        let built = started.elapsed();
        println!(
            "{files} files ({generated:?} to generate), index built in {built:?}: \
             {} words, {} postings, ~{} KiB",
            index.words(),
            index.postings_len(),
            index.heap_bytes() / 1024
        );
        let rng = &mut rand::rngs::StdRng::seed_from_u64(5);
        // Common: one to three of the few corpus words, every tenth a `*`
        // suffix. Rare: a corpus word plus a generated word, like an artist
        // or title in a real library.
        for mix in ["common", "rare"] {
            let queries: Vec<_> = (0..1000)
                .map(|i| {
                    let mut text = (0..rng.random_range(1..4))
                        .map(|_| (*WORDS.choose(rng).unwrap()).to_owned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if mix == "rare" {
                        text.push_str(&format!(" w{}", rng.random_range(0..50_000)));
                    } else if i % 10 == 0 {
                        text.insert(0, '*');
                    }
                    if i % 7 == 0 {
                        text.push_str(" -live");
                    }
                    soulseek::SearchQuery::parse(&text)
                })
                .collect();
            let mut results = [0usize; 2];
            let mut times = [Duration::ZERO; 2];
            for (path, linear) in [(0, true), (1, false)] {
                let started = Instant::now();
                for query in &queries {
                    results[path] += run_search(&snapshot, query, 100, linear).len();
                }
                times[path] = started.elapsed();
            }
            assert_eq!(results[0], results[1]);
            println!(
                "1000 {mix} searches (limit 100, {} results): linear {:?}, indexed {:?} ({:.0}x)",
                results[1],
                times[0],
                times[1],
                times[0].as_secs_f64() / times[1].as_secs_f64()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn indexed_file_cannot_be_replaced_by_an_outside_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let file = root.join("track.flac");
        fs::write(&file, b"shared").unwrap();
        let outside = temp.path().join("outside.flac");
        fs::write(&outside, b"private").unwrap();
        let snapshot = scan::scan_roots(
            ScanOptions {
                directories: vec![format!("[music]{}", root.display())],
                filters: Vec::new(),
            },
            None,
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap();
        let contents = &snapshot.shares["music"];
        assert_eq!(
            contents
                .resolve("music\\track.flac", "music", "track.flac")
                .unwrap()
                .1,
            6
        );
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(outside, file).unwrap();
        assert!(
            contents
                .resolve("music\\track.flac", "music", "track.flac")
                .is_err()
        );
    }
}
