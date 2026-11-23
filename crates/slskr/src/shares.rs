//! Local share snapshots, safe path resolution, and `/shares` endpoints.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    ops::Bound::{Excluded, Unbounded},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use slskr_api::{
    options::Options,
    shares::{Share, SharesByHost},
    users::FileAttributeType as NamedAttribute,
};
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult},
    users::convert_directory,
};

mod audio;
mod relay;
mod scan;
use scan::ShareSpec;

const LOCAL: &str = "local";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ScanOptions {
    directories: Vec<String>,
    filters: Vec<String>,
    case_sensitive: bool,
}

impl ScanOptions {
    fn new(options: &Options) -> Self {
        Self {
            directories: options.shares.directories.clone(),
            filters: options.shares.filters.clone(),
            case_sensitive: options.flags.case_sensitive_reg_ex,
        }
    }

    fn matches(&self, options: &Options) -> bool {
        self.directories == options.shares.directories
            && self.filters == options.shares.filters
            && self.case_sensitive == options.flags.case_sensitive_reg_ex
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
    relay_snapshots: RwLock<Arc<relay::Snapshots>>,
    active: Mutex<Option<CancellationToken>>,
    cache_path: Option<PathBuf>,
    relay_cache: Option<PathBuf>,
    changed: watch::Sender<()>,
    relay_changed: watch::Sender<()>,
}

impl ShareService {
    pub fn new(core: &Core) -> Result<Self> {
        let options = core.options.get();
        let cache_path =
            (!options.flags.volatile).then(|| core.paths.data_dir.join("shares-cache.json"));
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
        let relay_cache =
            (!options.flags.volatile).then(|| core.paths.data_dir.join("relay-shares"));
        Ok(Self(Arc::new(Inner {
            core: core.clone(),
            snapshot: RwLock::new(snapshot),
            relay_snapshots: RwLock::new(Arc::new(BTreeMap::new())),
            active: Mutex::new(None),
            cache_path,
            relay_cache,
            changed: watch::channel(()).0,
            relay_changed: watch::channel(()).0,
        })))
    }

    pub fn start(&self, app: AppState) {
        self.refresh_state();
        let core = &self.0.core;
        let startup = core.options.get();
        let mut options = core.options.subscribe();
        let mut events = core.client.subscribe();
        let mut changed = self.0.changed.subscribe();
        let mut relay_changed = self.0.relay_changed.subscribe();
        let service = self.clone();
        let relay = app.services.relay.clone();
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
                        relay.synchronize_shares();
                        service.announce().await;
                    },
                    update = relay_changed.changed() => {
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
                        if options.shares.cache.retention.is_some_and(|minutes| minutes > 0)
                            && service.needs_scan(&options)
                        {
                            service.launch_scan(service.0.core.shutdown.child_token());
                        }
                    },
                }
            }
        });
        if !startup.flags.no_share_scan
            && (startup.flags.force_share_scan || self.needs_scan(&startup))
        {
            self.launch_scan(core.shutdown.child_token());
        }
    }

    fn needs_scan(&self, options: &Options) -> bool {
        self.snapshot_for(options).is_none_or(|snapshot| {
            options.shares.cache.retention.is_some_and(|minutes| {
                minutes > 0
                    && Utc::now()
                        .signed_duration_since(snapshot.scanned_at)
                        .num_minutes()
                        >= i64::from(minutes)
            })
        })
    }

    fn refresh_state(&self) {
        let active = self.0.active.lock().unwrap();
        let snapshot = self.snapshot();
        let remotes = self.relay_snapshots();
        let counts = total_counts(snapshot.as_deref(), &remotes);
        self.0.core.state.update(|s| {
            let state = &mut s.shares;
            state.hosts = Some(share_hosts(&remotes));
            state.ready = active.is_none() && snapshot.is_some();
            state.scan_pending = snapshot.is_none() || state.faulted || state.cancelled;
            state.directories = state_count(counts.0);
            state.files = state_count(counts.1);
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

    fn launch_scan(&self, cancel: CancellationToken) -> Option<JoinHandle<Result<()>>> {
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
        let remotes = self.relay_snapshots();
        let totals = total_counts(snapshot.as_deref(), &remotes);
        self.0.core.state.update(|s| {
            s.shares.scanning = false;
            s.shares.ready = snapshot.is_some();
            s.shares.scan_pending = result.is_err() || snapshot.is_none();
            s.shares.cancelled = cancelled;
            s.shares.faulted = !cancelled && result.is_err();
            s.shares.scan_progress = if result.is_ok() { 1.0 } else { 0.0 };
            s.shares.hosts = Some(share_hosts(&remotes));
            s.shares.directories = state_count(totals.0);
            s.shares.files = state_count(totals.1);
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

    fn snapshot_for(&self, options: &Options) -> Option<Arc<Snapshot>> {
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

    fn relay_snapshots(&self) -> Arc<relay::Snapshots> {
        self.0.relay_snapshots.read().unwrap().clone()
    }

    pub fn export_relay(&self, path: &Path) -> Result<serde_json::Value> {
        let options = self.0.core.options.get();
        let snapshot = self.snapshot_for(&options);
        let metadata = serde_json::to_value(local_shares(&options, snapshot.as_deref()))?;
        relay::export(path, snapshot.as_deref(), &options.shares)?;
        Ok(metadata)
    }

    pub fn import_relay(
        &self,
        agent: &str,
        metadata: serde_json::Value,
        path: &Path,
        closed: &CancellationToken,
    ) -> Result<()> {
        ensure!(!closed.is_cancelled(), "relay agent disconnected");
        relay::validate_agent(agent)?;
        let snapshot = relay::import(path, metadata)?;
        let cache = self
            .0
            .relay_cache
            .as_deref()
            .map(|directory| relay::stage_cache(directory, agent, &snapshot))
            .transpose()?;
        let mut remotes = self.0.relay_snapshots.write().unwrap();
        // Deactivation takes this same lock, so a disconnected agent cannot
        // republish its shares after the controller removes them.
        ensure!(!closed.is_cancelled(), "relay agent disconnected");
        if let Some((temp, path)) = cache {
            temp.persist(path)?;
        }
        Arc::make_mut(&mut *remotes).insert(agent.to_owned(), Arc::new(snapshot));
        drop(remotes);
        self.refresh_state();
        self.0.relay_changed.send_replace(());
        Ok(())
    }

    pub(crate) fn deactivate_relay(&self, agent: &str) {
        let mut remotes = self.0.relay_snapshots.write().unwrap();
        if !remotes.contains_key(agent) {
            return;
        }
        Arc::make_mut(&mut *remotes).remove(agent);
        drop(remotes);
        self.refresh_state();
        self.0.relay_changed.send_replace(());
    }

    pub fn resolve_relay(&self, filename: &str) -> Option<(String, u64)> {
        let remote = normalize_remote(filename).ok()?;
        let (parent, name) = remote.rsplit_once('\\')?;
        let options = self.0.core.options.get();
        let local = self.0.snapshot.read().unwrap();
        // The local manifest takes precedence even if resolving its file on
        // disk fails (removed, moved, or replaced since the last scan).
        if local.as_deref().is_some_and(|snapshot| {
            snapshot.options.matches(&options) && contains_file(snapshot.files(parent), name)
        }) {
            return None;
        }
        self.relay_snapshots().iter().find_map(|(agent, snapshot)| {
            snapshot
                .find_file(parent, name)
                .map(|file| (agent.clone(), file.size))
        })
    }

    pub fn counts(&self) -> (u32, u32) {
        total_counts(self.snapshot().as_deref(), &self.relay_snapshots())
    }

    pub fn browse(&self) -> Result<soulseek::BrowseResponse> {
        let local = self.snapshot();
        let remotes = self.relay_snapshots();
        let directories = if remotes.is_empty() {
            local
                .iter()
                .flat_map(|snapshot| {
                    snapshot
                        .shares
                        .values()
                        .flat_map(ShareContents::directories)
                })
                .collect()
        } else {
            let mut merged = BTreeMap::new();
            if let Some(local) = &local {
                for share in local.shares.values() {
                    for (name, files) in &share.directories {
                        merged.insert(
                            name.as_str(),
                            files
                                .iter()
                                .map(|file| file.wire(file.name.clone()))
                                .collect::<Vec<_>>(),
                        );
                    }
                }
            }
            for (agent, snapshot) in remotes.iter() {
                for (parent, files) in &snapshot.directories {
                    let local_files = local
                        .as_deref()
                        .map_or(&[][..], |snapshot| snapshot.files(parent));
                    let merged_files = merged.entry(parent.as_str()).or_default();
                    merged_files.extend(
                        files
                            .iter()
                            .filter(|file| {
                                !is_shadowed(&remotes, agent, parent, &file.name, local_files)
                            })
                            .map(|file| file.wire(file.name.clone())),
                    );
                }
            }
            merged
                .into_iter()
                .map(|(name, mut files)| {
                    files.sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
                    soulseek::Directory {
                        name: name.into(),
                        files,
                    }
                })
                .collect()
        };
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
        let mut result = directory(&name, local_files);
        let remotes = self.relay_snapshots();
        for (agent, snapshot) in remotes.iter() {
            if let Some(files) = snapshot.directories.get(name.as_ref()) {
                result.files.extend(
                    files
                        .iter()
                        .filter(|file| {
                            !is_shadowed(&remotes, agent, &name, &file.name, local_files)
                        })
                        .map(|file| file.wire(file.name.clone())),
                );
            }
        }
        result
            .files
            .sort_unstable_by(|a, b| a.filename.cmp(&b.filename));
        Ok(vec![result])
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
            query,
            limit,
            filename: String::new(),
            matches: Vec::new(),
        };
        if collect.is_full() {
            return Ok(collect.matches);
        }
        let local = self.snapshot();
        if let Some(snapshot) = &local {
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
        if collect.is_full() {
            return Ok(collect.matches);
        }
        let remotes = self.relay_snapshots();
        'remote: for (agent, snapshot) in remotes.iter() {
            for (parent, files) in &snapshot.directories {
                if files.is_empty() {
                    continue;
                }
                collect.parent(parent);
                let local_files = local
                    .as_deref()
                    .map_or(&[][..], |snapshot| snapshot.files(parent));
                for file in files {
                    if !is_shadowed(&remotes, agent, parent, &file.name, local_files) {
                        collect.file(&file.name, |filename| file.wire(filename));
                        if collect.is_full() {
                            break 'remote;
                        }
                    }
                }
            }
        }
        Ok(collect.matches)
    }

    pub fn shares(&self) -> SharesByHost {
        let options = self.0.core.options.get();
        let snapshot = self.snapshot_for(&options);
        let remotes = self.relay_snapshots();
        let mut shares =
            BTreeMap::from([(LOCAL.into(), local_shares(&options, snapshot.as_deref()))]);
        shares.extend(
            remotes
                .iter()
                .map(|(agent, snapshot)| (agent.clone(), snapshot.shares.clone())),
        );
        shares
    }
}

struct SearchMatches<'a> {
    query: &'a soulseek::SearchQuery,
    limit: usize,
    filename: String,
    matches: Vec<soulseek::File>,
}

impl SearchMatches<'_> {
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
        if self.query.matches(&self.filename) {
            self.matches.push(wire(self.filename.clone()));
        }
        self.filename.truncate(prefix);
    }
}

fn contains_file(files: &[ShareFile], name: &str) -> bool {
    files
        .binary_search_by(|file| file.name.as_str().cmp(name))
        .is_ok()
}

fn is_shadowed(
    remotes: &relay::Snapshots,
    agent: &str,
    parent: &str,
    name: &str,
    local: &[ShareFile],
) -> bool {
    contains_file(local, name)
        || remotes
            .range::<str, _>((Unbounded, Excluded(agent)))
            .any(|(_, earlier)| earlier.find_file(parent, name).is_some())
}

fn share_hosts(remotes: &relay::Snapshots) -> Vec<String> {
    std::iter::once(LOCAL.into())
        .chain(remotes.keys().cloned())
        .collect()
}

fn total_counts(local: Option<&Snapshot>, remotes: &relay::Snapshots) -> (u32, u32) {
    remotes.values().fold(
        local.map_or((0, 0), snapshot_counts),
        |(directories, files), remote| {
            let (remote_directories, remote_files) = remote.counts();
            (
                directories.saturating_add(remote_directories),
                files.saturating_add(remote_files),
            )
        },
    )
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
fn state_count(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn local_shares(options: &Options, snapshot: Option<&Snapshot>) -> Vec<Share> {
    let mut specs: Vec<_> = scan::specs(&options.shares.directories).collect();
    specs.sort_by_key(|spec| std::cmp::Reverse(spec.local.as_os_str().len()));
    specs
        .into_iter()
        .map(|spec| share_model(spec, snapshot))
        .collect()
}

fn relay_share<'a>(
    snapshots: &'a relay::Snapshots,
    id: &str,
) -> Option<(&'a relay::Snapshot, &'a Share)> {
    snapshots.values().find_map(|snapshot| {
        snapshot
            .shares
            .iter()
            .find(|share| share.id == id)
            .map(|share| (snapshot.as_ref(), share))
    })
}

fn relay_contents(snapshot: &relay::Snapshot, share: &Share) -> Vec<slskr_api::users::Directory> {
    if share.is_excluded {
        return Vec::new();
    }
    let Some(root) = share.remote_path.as_deref() else {
        return Vec::new();
    };
    let root = root.trim_end_matches('\\');
    let prefix = format!("{root}\\");
    let root_dir = snapshot
        .directories
        .get_key_value(root)
        .filter(|(_, files)| !files.is_empty());
    root_dir
        .into_iter()
        .chain(
            snapshot
                .directories
                .range::<str, _>((std::ops::Bound::Included(prefix.as_str()), Unbounded))
                .take_while(|(name, _)| name.starts_with(&prefix)),
        )
        .map(|(name, files)| {
            convert_directory(soulseek::Directory {
                name: name.clone(),
                files: files
                    .iter()
                    .map(|file| file.wire(file.name.clone()))
                    .collect(),
            })
        })
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

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/shares", get(shares).put(start_scan).delete(cancel_scan))
        .route("/shares/contents", get(contents))
        .route("/shares/{id}", get(share))
        .route("/shares/{id}/contents", get(share_contents))
}

async fn shares(_: Authenticated, State(s): State<AppState>) -> Json<SharesByHost> {
    Json(s.services.shares.shares())
}

async fn share(
    _: Authenticated,
    State(s): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Share>> {
    let service = &s.services.shares;
    let options = s.core.options.get();
    if let Some(spec) = scan::specs(&options.shares.directories).find(|spec| spec.id() == id) {
        return Ok(Json(share_model(
            spec,
            service.snapshot_for(&options).as_deref(),
        )));
    }
    let remotes = service.relay_snapshots();
    relay_share(&remotes, &id)
        .map(|(_, share)| Json(share.clone()))
        .ok_or_else(|| ApiError::NotFound(String::new()))
}

async fn contents(
    _: Authenticated,
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<slskr_api::users::Directory>>> {
    Ok(Json(
        s.services
            .shares
            .browse()?
            .directories
            .into_iter()
            .map(convert_directory)
            .collect(),
    ))
}

async fn share_contents(
    _: Authenticated,
    State(s): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Vec<slskr_api::users::Directory>>> {
    let options = s.core.options.get();
    let Some(spec) = scan::specs(&options.shares.directories).find(|spec| spec.id() == id) else {
        let remotes = s.services.shares.relay_snapshots();
        return relay_share(&remotes, &id)
            .map(|(snapshot, share)| Json(relay_contents(snapshot, share)))
            .ok_or_else(|| ApiError::NotFound(String::new()));
    };
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

async fn start_scan(_: RequireReadWrite, State(s): State<AppState>) -> ApiResult<StatusCode> {
    s.services
        .shares
        .launch_scan(s.core.shutdown.child_token())
        .ok_or_else(|| ApiError::Conflict("A share scan is already in progress".into()))?;
    Ok(StatusCode::OK)
}

async fn cancel_scan(_: RequireReadWrite, State(s): State<AppState>) -> ApiResult<StatusCode> {
    if s.services.shares.cancel_scan() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound(String::new()))
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
    async fn relay_snapshots_merge_atomically_without_replacing_the_local_manifest() {
        use crate::{
            app::{Hubs, Paths},
            auth::{Auth, AuthConfig},
            config::OptionsStore,
            signalr::Hub,
            state::StateStore,
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("collision.txt"), b"local").unwrap();
        let config_file = temp.path().join("slskd.yml");
        let options = OptionsStore::load(temp.path(), config_file.clone(), Default::default())
            .await
            .unwrap();
        options.patch(|options| {
            options.flags.volatile = false;
            options.shares.directories = vec![format!("[music]{}", root.display())];
            options.shares.filters.clear();
        });
        let hubs = Hubs {
            application: Hub::new("application"),
            logs: Hub::new("logs"),
            search: Hub::new("search"),
            metrics: Hub::new("metrics"),
            relay: Hub::new("relay"),
        };
        let core = Core {
            paths: Paths {
                app_dir: temp.path().into(),
                data_dir: temp.path().join("data"),
                config_file,
                logs_dir: temp.path().join("logs"),
                content_path: None,
            },
            options,
            state: StateStore::new(hubs.application.clone()),
            client: soulseek::SoulseekClient::new(
                Default::default(),
                Arc::new(soulseek::resolvers::DefaultResolvers),
            ),
            hubs,
            auth: Auth::new(AuthConfig {
                disabled: true,
                username: "test".into(),
                password: "test".into(),
                jwt_key: vec![0; 32],
                jwt_ttl_ms: 1_000,
                api_keys: Vec::new(),
            }),
            logs: Default::default(),
            shutdown: CancellationToken::new(),
        };
        let shares = ShareService::new(&core).unwrap();
        shares.scan(CancellationToken::new()).await.unwrap();
        let uploaded = temp.path().join("agent.db");
        relay::export(&uploaded, None, &Default::default()).unwrap();
        let db = rusqlite::Connection::open(&uploaded).unwrap();
        db.execute_batch("
            INSERT INTO directories VALUES ('music', 0), ('music\\albums', 0);
            INSERT INTO files VALUES
                ('music\\collision.txt', '/agent/collision.txt', 90, '', 1, 'txt', '[]', 0),
                ('music\\albums\\remote.txt', '/agent/albums/remote.txt', 80, '', 1, 'txt', '[]', 0);
        ").unwrap();
        let metadata = serde_json::json!([
            {"id":"agent-music","alias":"music","remotePath":"music","directories":1,"files":2}
        ]);
        let agent_connection = CancellationToken::new();
        shares
            .import_relay("agent", metadata.clone(), &uploaded, &agent_connection)
            .unwrap();
        assert_eq!(shares.shares()["agent"][0].id, "agent-music");
        assert_eq!(
            shares.resolve_relay("music\\albums\\remote.txt"),
            Some(("agent".into(), 80))
        );
        assert_eq!(shares.resolve_relay("music\\collision.txt"), None);
        fs::remove_file(root.join("collision.txt")).unwrap();
        assert!(shares.resolve_file("music\\collision.txt").is_err());
        assert_eq!(shares.resolve_relay("music\\collision.txt"), None);
        assert_eq!(shares.browse().unwrap().directories.len(), 2);
        assert_eq!(shares.list_directory("music").unwrap()[0].files[0].size, 5);
        let found = shares
            .search(&soulseek::SearchQuery::parse("remote"), usize::MAX)
            .unwrap();
        assert_eq!(found[0].filename, "music\\albums\\remote.txt");
        // The response limit caps collection, local matches first, and skips relays when full.
        let txt = soulseek::SearchQuery::parse("txt");
        assert_eq!(shares.search(&txt, usize::MAX).unwrap().len(), 2);
        let capped = shares.search(&txt, 1).unwrap();
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].filename, "music\\collision.txt");
        assert!(shares.search(&txt, 0).unwrap().is_empty());

        db.execute("UPDATE files SET size = -1", []).unwrap();
        drop(db);
        assert!(
            shares
                .import_relay("agent", metadata.clone(), &uploaded, &agent_connection)
                .is_err()
        );
        assert_eq!(
            shares.resolve_relay("music\\albums\\remote.txt"),
            Some(("agent".into(), 80))
        );

        agent_connection.cancel();
        shares.deactivate_relay("agent");
        assert!(!shares.shares().contains_key("agent"));
        assert!(
            shares
                .import_relay("agent", metadata, &uploaded, &agent_connection)
                .is_err()
        );
        assert_eq!(
            fs::read_dir(core.paths.data_dir.join("relay-shares"))
                .unwrap()
                .count(),
            1
        );
        let resumed = ShareService::new(&core).unwrap();
        assert_eq!(resumed.resolve_relay("music\\albums\\remote.txt"), None);
        assert_eq!(resumed.resolve_relay("music\\collision.txt"), None);
        let outgoing = temp.path().join("outgoing.db");
        assert_eq!(
            resumed
                .export_relay(&outgoing)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let db = rusqlite::Connection::open(outgoing).unwrap();
        let rows: i64 = db
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
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
                case_sensitive: false,
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
