//! Local share snapshots, safe path resolution, and `/shares` endpoints.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
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

#[derive(Debug, Serialize, Deserialize)]
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
    cache_path: Option<PathBuf>,
    changed: watch::Sender<()>,
}

impl ShareService {
    pub fn new(core: &Core) -> Result<Self> {
        let options = core.options.get();
        let cache_path = Some(core.paths.db_dir.join("shares-cache.json"));
        let snapshot = cache_path.as_deref().and_then(|path| {
            remove_partial(path);
            let result = File::open(path)
                .context("opening share cache")
                .and_then(|file| serde_json::from_reader::<_, Snapshot>(BufReader::new(file))
                    .context("reading share cache"));
            match result {
                Ok(snapshot) if snapshot.options.matches(&options) && snapshot_has_configured_roots(&snapshot) => {
                    Some(Arc::new(snapshot))
                }
                Ok(_) => None,
                Err(error) => {
                    if !error.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) {
                        tracing::warn!(%error, path = %path.display(), "unable to restore share cache");
                    }
                    None
                }
            }
        });
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
            let mut retention = tokio::time::interval_at(tokio::time::Instant::now() + hour, hour);
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
                    _ = retention.tick() => {
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
        let active = self.0.active.lock().unwrap();
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
        let mut active = self.0.active.lock().unwrap();
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
                    let mut active = service.0.active.lock().unwrap();
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
        let snapshot = scan::scan_roots(options, cancel, |completed, total| {
            // Reserve the final step for writing and publishing the new snapshot.
            self.0
                .core
                .state
                .update(|s| s.shares.scan_progress = completed as f64 / (total as f64 + 1.0));
        })
        .and_then(|snapshot| {
            ensure!(!cancel.is_cancelled(), "share scan cancelled");
            if let Some(path) = &self.0.cache_path {
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
            }
            Ok(snapshot)
        });
        let mut active = self.0.active.lock().unwrap();
        let cancelled = cancel.is_cancelled();
        let result = snapshot.and_then(|snapshot| {
            ensure!(!cancelled, "share scan cancelled");
            ensure!(
                snapshot.options.matches(&self.0.core.options.get()),
                "share configuration changed during the scan"
            );
            if let Some(path) = &self.0.cache_path {
                fs::rename(partial_path(path), path)?;
            }
            *self.0.snapshot.write().unwrap() = Some(Arc::new(snapshot));
            Ok(())
        });
        if result.is_err()
            && let Some(path) = &self.0.cache_path
        {
            remove_partial(path);
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
        self.0.active.lock().unwrap().as_ref().is_some_and(|token| {
            token.cancel();
            true
        })
    }

    fn snapshot_for(&self, options: &Config) -> Option<Arc<Snapshot>> {
        self.0
            .snapshot
            .read()
            .unwrap()
            .as_ref()
            .filter(|snapshot| snapshot.options.matches(options))
            .cloned()
    }

    fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot_for(&self.0.core.options.get())
    }

    pub fn counts(&self) -> (u32, u32) {
        self.snapshot().as_deref().map_or((0, 0), snapshot_counts)
    }

    pub fn browse(&self) -> Result<soulseek::BrowseResponse> {
        let directories = self
            .snapshot()
            .iter()
            .flat_map(|snapshot| {
                snapshot
                    .shares
                    .values()
                    .flat_map(ShareContents::directories)
            })
            .collect();
        Ok(soulseek::BrowseResponse {
            directories,
            locked_directories: Vec::new(),
        })
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

    /// Returns at most `limit` shared files matching `query`, stopping the scan
    /// once the limit is reached so broad queries stay bounded.
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
            'local: for share in snapshot.shares.values() {
                for (parent, files) in &share.directories {
                    if files.is_empty() {
                        continue;
                    }
                    collect.parent(parent);
                    for file in files {
                        collect.file(&file.name, |filename| file.wire(filename));
                        if collect.is_full() {
                            break 'local;
                        }
                    }
                }
            }
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
        assert_eq!(shares.browse().unwrap().directories.len(), 1);
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
            &CancellationToken::new(),
            |_, _| {},
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
