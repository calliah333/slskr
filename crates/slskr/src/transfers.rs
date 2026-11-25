//! Persistent Soulseek transfers, upload scheduling, and live progress.

mod api;
mod backoff;
mod bans;
mod destination;
mod folder;
mod queue;
mod store;

pub use api::routes;
pub(crate) use queue::group_options as group_config;

/// Parses a `downloads.file_mode` value (octal or `rwxr-xr-x`); empty is `None`.
pub(crate) fn parse_file_mode(value: &str) -> io::Result<Option<u32>> {
    destination::mode(Some(value))
}

use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};
use slskr_types::{
    config::{Config, DownloadsConfig, OnConflict},
    events::{DirectoryComplete, EventData, FileComplete},
    realtime::{Event as LiveEvent, Topic},
    telemetry::{CompletedMetrics, CurrentMetrics},
    transfers::{
        Batch, DownloadFile, EnqueueFailure, EnqueuedDownloads, Transfer, TransferDirection,
        TransferState,
    },
};
use soulseek::{
    Error, TransferOptions,
    client::{DownloadOutput, TransferInput, TransferOutput, UploadInput},
};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    db::Database,
    events::EventService,
    shares::ShareService,
    users::UserService,
};
use queue::{Bandwidth, UploadHistory, UploadQueue};
use store::Snapshot;

#[cfg(test)]
pub(crate) use store::MIGRATIONS as TRANSFER_MIGRATIONS;
pub(crate) use store::{date as stored_date, state_from_bits, timestamp as stored_time};

#[derive(Clone)]
pub struct TransferService(Arc<Inner>);

struct Inner {
    core: Core,
    events: EventService,
    users: UserService,
    shares: ShareService,
    db: Database,
    list_epoch: Uuid,
    /// Per direction, bumped after every write to that direction's stored transfers (see
    /// [`TransferService::wrote`]), so a write to one list does not invalidate the other's ETag.
    list_revisions: [AtomicU64; 2],
    runtime: Mutex<Runtime>,
    writes: tokio::sync::Mutex<()>,
    ready: watch::Sender<Option<Result<(), String>>>,
    download_requests: Semaphore,
}

#[derive(Default)]
struct Runtime {
    active: HashMap<String, Active>,
    // Active entries per (user, filename). A count, because a finished entry
    // can linger while a re-request for the same file is already active.
    files: [HashMap<String, HashMap<String, usize>>; 2],
    uploads: UploadQueue,
    history: UploadHistory,
    bandwidth: Bandwidth,
    backoff: backoff::UploadBackoff,
    completed: [CompletedMetrics; 2],
}

struct Active {
    transfer: Transfer,
    cancel: CancellationToken,
    done: CancellationToken,
    endpoint: Option<SocketAddr>,
    stored: bool,
    user_cancelled: bool,
    /// A change other than byte progress waits to be persisted (on the next flush).
    dirty: bool,
    /// Byte progress waits to be persisted (every [`PROGRESS_PERSIST_INTERVAL`]).
    progress: bool,
    /// When this entry was last persisted.
    persisted_at: Instant,
    /// A change waits to be published to the `transfers` topic (on the next flush).
    changed: bool,
    offset: u64,
    remote_size_invalid: bool,
}

impl Active {
    /// Records a change that is persisted and published on the next flush.
    fn touch(&mut self) {
        self.dirty = true;
        self.changed = true;
    }
}

/// Byte progress of a transfer is persisted at most this often; the in-memory entry
/// serves live values meanwhile. Other changes are persisted on the next flush.
const PROGRESS_PERSIST_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct DownloadContext {
    batch: Option<Batch>,
    search_text: Option<String>,
}

impl Runtime {
    fn has(&self, direction: TransferDirection, username: &str, filename: &str) -> bool {
        self.files[direction_index(direction)]
            .get(username)
            .is_some_and(|files| files.contains_key(filename))
    }

    /// Like [`Self::has`], but ignores entries that already ended and are only
    /// waiting for the progress loop to flush and drop them.
    fn has_unfinished(&self, direction: TransferDirection, username: &str, filename: &str) -> bool {
        self.has(direction, username, filename)
            && self.active.values().any(|entry| {
                let t = &entry.transfer;
                t.direction == direction
                    && t.ended_at.is_none()
                    && t.username == username
                    && t.filename == filename
            })
    }

    fn insert(
        &mut self,
        transfer: Transfer,
        cancel: CancellationToken,
        endpoint: Option<SocketAddr>,
        stored: bool,
    ) {
        self.files[direction_index(transfer.direction)]
            .entry(transfer.username.clone())
            .or_default()
            .entry(transfer.filename.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        self.active.insert(
            transfer.id.clone(),
            Active {
                remote_size_invalid: false,
                transfer,
                cancel,
                endpoint,
                stored,
                done: CancellationToken::new(),
                user_cancelled: false,
                dirty: false,
                progress: false,
                persisted_at: Instant::now(),
                // Published once stored.
                changed: true,
                offset: 0,
            },
        );
    }

    fn remove(&mut self, id: &str) {
        let Some(Active {
            transfer: t, done, ..
        }) = self.active.remove(id)
        else {
            return;
        };
        let users = &mut self.files[direction_index(t.direction)];
        if let Some(files) = users.get_mut(&t.username) {
            if let Some(count) = files.get_mut(&t.filename) {
                *count -= 1;
                if *count == 0 {
                    files.remove(&t.filename);
                }
            }
            if files.is_empty() {
                users.remove(&t.username);
            }
        }
        if t.direction == TransferDirection::Upload && !t.state.is_complete() {
            self.uploads.release(id);
        }
        done.cancel();
    }
}

impl TransferService {
    pub fn new(
        core: &Core,
        events: &EventService,
        users: &UserService,
        shares: &ShareService,
    ) -> anyhow::Result<Self> {
        let path = core.paths.db_dir.join("transfers.db");
        Ok(Self(Arc::new(Inner {
            core: core.clone(),
            events: events.clone(),
            users: users.clone(),
            shares: shares.clone(),
            db: Database::open(&path, store::MIGRATIONS)?,
            list_epoch: Uuid::new_v4(),
            list_revisions: Default::default(),
            runtime: Mutex::default(),
            writes: tokio::sync::Mutex::new(()),
            ready: watch::channel(None).0,
            download_requests: Semaphore::new(2),
        })))
    }

    pub fn start(&self, app: AppState) {
        let service = self.clone();
        tokio::spawn(async move {
            let initialized = store::initialize(service.database()).await;
            service.wrote(TransferDirection::Download);
            service.wrote(TransferDirection::Upload);
            match initialized {
                Ok((history, downloads)) => {
                    service.recover(&app, history, downloads).await;
                    service.0.ready.send_replace(Some(Ok(())));
                    let pruner = service.clone();
                    tokio::spawn(async move { pruner.prune_banned().await });
                }
                Err(error) => {
                    tracing::error!(%error, "could not initialize transfers");
                    service.0.ready.send_replace(Some(Err(error.to_string())));
                    return;
                }
            }

            let mut options = service.0.core.options.subscribe();
            let mut blacklist = options.borrow().blacklist.clone();
            let mut progress = tokio::time::interval(Duration::from_millis(250));
            let mut retention = tokio::time::interval(Duration::from_secs(60));
            progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = service.0.core.shutdown.cancelled() => { service.flush_all().await; break; }
                    _ = progress.tick() => { service.flush().await; }
                    _ = retention.tick() => {
                        {
                            let mut runtime = service.runtime();
                            runtime.history.prune();
                            runtime.backoff.prune();
                        }
                        let cfg = service.0.core.options.get();
                        let pruned = store::prune(service.database(), &cfg.retention).await;
                        service.wrote(TransferDirection::Download);
                        service.wrote(TransferDirection::Upload);
                        match pruned {
                            Ok(removed) => service.publish_removed(removed),
                            Err(error) => tracing::warn!(%error, "could not apply transfer retention"),
                        }
                    }
                    change = options.changed() => {
                        if change.is_err() { break; }
                        let cfg = options.borrow_and_update().clone();
                        let mut runtime = service.runtime();
                        for entry in runtime.active.values() {
                            if entry.transfer.direction == TransferDirection::Upload && is_queued(entry.transfer.state)
                                && service.0.users.is_blacklisted(&entry.transfer.username, entry.endpoint.map(|e| e.ip()))
                            {
                                entry.cancel.cancel();
                            }
                        }
                        runtime.uploads.dispatch(&service.0.users, &cfg, service.upload_slots(&cfg));
                        drop(runtime);
                        if cfg.blacklist != blacklist {
                            blacklist = cfg.blacklist.clone();
                            let pruner = service.clone();
                            tokio::spawn(async move { pruner.prune_banned().await });
                        }
                    }
                }
            }
        });
    }

    fn runtime(&self) -> MutexGuard<'_, Runtime> {
        self.0
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn database(&self) -> &Database {
        &self.0.db
    }

    async fn ready(&self) -> anyhow::Result<()> {
        let mut ready = self.0.ready.subscribe();
        let initialized = ready.wait_for(Option::is_some).await?;
        initialized
            .as_ref()
            .expect("initialized transfer service")
            .clone()
            .map_err(anyhow::Error::msg)
    }

    fn request_permit(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.0.download_requests.try_acquire().ok()
    }

    fn upload_slots(&self, cfg: &Config) -> u32 {
        // The SDK owns a fixed data semaphore. A reload can lower the daemon
        // queue limit immediately, but increases beyond that semaphore need a restart.
        cfg.uploads
            .slots
            .min(self.0.core.options.startup().uploads.slots)
    }

    pub fn enqueue_upload(
        &self,
        username: &str,
        endpoint: SocketAddr,
        filename: &str,
    ) -> Result<(), String> {
        if self.0.core.shutdown.is_cancelled() || !matches!(*self.0.ready.borrow(), Some(Ok(()))) {
            return Err("Transfer service unavailable".into());
        }
        // Shared names are backslash-masked and resolved exactly, as upstream
        // does; a '/' spelling must not alias (and re-queue) the same file.
        if filename.contains('/') || self.0.users.is_blacklisted(username, Some(endpoint.ip())) {
            return Err("File not shared.".into());
        }
        if self
            .runtime()
            .has_unfinished(TransferDirection::Upload, username, filename)
        {
            return Ok(());
        }
        let (source, size) = self
            .0
            .shares
            .resolve_file(filename)
            .map_err(|_| "File not shared.")?;
        if size > i64::MAX as u64 {
            return Err("File too large.".into());
        }
        let cancel = self.0.core.shutdown.child_token();
        let transfer = new_transfer(
            TransferDirection::Upload,
            username,
            filename.into(),
            size,
            None,
        );
        let cfg = self.0.core.options.get();
        let receiver = {
            let mut runtime = self.runtime();
            if runtime.has_unfinished(TransferDirection::Upload, username, filename) {
                return Ok(());
            }
            let group = self.0.users.group(username);
            runtime.history.check(
                runtime
                    .active
                    .values()
                    .map(|entry| &entry.transfer)
                    // Upstream counts every upload that has not ended,
                    // including in-progress ones, against the queued limits.
                    .filter(|t| t.direction == TransferDirection::Upload && t.ended_at.is_none()),
                &cfg,
                &group,
                username,
                size,
            )?;
            let receiver = runtime.uploads.push(&transfer);
            runtime.insert(transfer.clone(), cancel.clone(), Some(endpoint), false);
            receiver
        };
        let service = self.clone();
        tokio::spawn(async move {
            service.run_upload(transfer, source, cancel, receiver).await;
        });
        Ok(())
    }

    pub fn place_in_queue(&self, username: &str, filename: &str) -> Option<u32> {
        let cfg = self.0.core.options.get();
        self.runtime()
            .uploads
            .position(&self.0.users, &cfg, username, filename)
    }

    pub fn upload_info(&self) -> (u32, u32, bool) {
        let cfg = self.0.core.options.get();
        self.runtime().uploads.info(self.upload_slots(&cfg))
    }

    pub fn upload_info_for(&self, username: &str) -> (u32, u32, bool) {
        let cfg = self.0.core.options.get();
        self.runtime()
            .uploads
            .forecast(&self.0.users, &cfg, self.upload_slots(&cfg), username)
    }

    pub fn metrics(&self) -> CurrentMetrics {
        let runtime = self.runtime();
        let mut metrics = CurrentMetrics::default();
        let mut in_progress = [HashSet::new(), HashSet::new()];
        let mut queued = [HashSet::new(), HashSet::new()];
        for entry in runtime.active.values() {
            let t = &entry.transfer;
            let (index, direction) = match t.direction {
                TransferDirection::Download => (0, &mut metrics.downloads),
                TransferDirection::Upload => (1, &mut metrics.uploads),
            };
            if is_queued(t.state) {
                queued[index].insert(t.username.as_str());
                direction.queued.files += 1;
                direction.queued.bytes += t.size;
            } else if matches!(
                t.state,
                TransferState::Initializing | TransferState::InProgress
            ) {
                in_progress[index].insert(t.username.as_str());
                direction.in_progress.files += 1;
                direction.in_progress.total_speed += t.average_speed;
            }
        }
        for (index, direction) in [(0, &mut metrics.downloads), (1, &mut metrics.uploads)] {
            direction.queued.users = queued[index].len() as u64;
            direction.in_progress.users = in_progress[index].len() as u64;
            if direction.in_progress.files > 0 {
                direction.in_progress.average_speed =
                    direction.in_progress.total_speed / direction.in_progress.files as f64;
            }
            direction.completed = runtime.completed[index].clone();
        }
        metrics
    }

    async fn recover(&self, app: &AppState, history: Vec<Transfer>, downloads: Vec<Transfer>) {
        let mut contexts = HashMap::<String, Arc<DownloadContext>>::new();
        let mut jobs = Vec::with_capacity(downloads.len());
        let throttle = Arc::new(Semaphore::new(5));
        for transfer in downloads {
            let context = match &transfer.batch_id {
                Some(id) => {
                    if let Some(context) = contexts.get(id) {
                        context.clone()
                    } else {
                        let batch = match store::batch_info(self.database(), id.clone()).await {
                            Ok(batch) => batch,
                            Err(error) => {
                                tracing::warn!(%error, %id, "could not recover transfer batch");
                                continue;
                            }
                        };
                        let context = Arc::new(DownloadContext::new(app, batch).await);
                        contexts.insert(id.clone(), context.clone());
                        context
                    }
                }
                None => Arc::default(),
            };
            let cancel = self.0.core.shutdown.child_token();
            jobs.push((transfer, context, cancel));
        }
        {
            let mut runtime = self.runtime();
            for transfer in history {
                runtime.history.record(&transfer);
            }
            for (transfer, _, cancel) in &jobs {
                runtime.insert(transfer.clone(), cancel.clone(), None, true);
            }
        }
        for (transfer, context, cancel) in jobs {
            let service = self.clone();
            let throttle = throttle.clone();
            tokio::spawn(async move {
                service
                    .run_download(transfer, context, throttle, cancel)
                    .await;
            });
        }
    }

    async fn enqueue_downloads(
        &self,
        username: String,
        files: Vec<DownloadFile>,
        root: Option<String>,
        context: DownloadContext,
    ) -> anyhow::Result<EnqueuedDownloads> {
        self.ready().await?;
        let service = self.clone();
        tokio::spawn(async move {
            let context = Arc::new(context);
            let cancel = service.0.core.shutdown.child_token();
            let mut response = EnqueuedDownloads {
                files: files.len(),
                ..EnqueuedDownloads::default()
            };
            {
                let mut runtime = service.runtime();
                for file in files {
                    if runtime.has(TransferDirection::Download, &username, &file.filename) {
                        response.failed.push(EnqueueFailure {
                            filename: file.filename,
                            error: "Skipped: Already in progress".into(),
                        });
                        continue;
                    }
                    let mut transfer = new_transfer(
                        TransferDirection::Download,
                        &username,
                        file.filename,
                        file.size,
                        context.batch.as_ref().map(|b| b.id.clone()),
                    );
                    transfer.root.clone_from(&root);
                    runtime.insert(transfer.clone(), cancel.child_token(), None, false);
                    response.enqueued.push(transfer);
                }
            }
            if response.enqueued.is_empty() {
                return Ok(response);
            }
            let inserted = store::insert(service.database(), response.enqueued.clone()).await;
            service.wrote(TransferDirection::Download);
            match inserted {
                Ok(superseded) => service.publish_removed(superseded),
                Err(error) => {
                    let mut runtime = service.runtime();
                    for transfer in &response.enqueued {
                        runtime.remove(&transfer.id);
                    }
                    return Err(error);
                }
            }
            let count = response.enqueued.len();
            let throttle = Arc::new(Semaphore::new(if count > 30 { 5 } else { count }));
            for transfer in &response.enqueued {
                let service = service.clone();
                let context = context.clone();
                let throttle = throttle.clone();
                let transfer = transfer.clone();
                let cancel = {
                    let mut runtime = service.runtime();
                    let entry = runtime
                        .active
                        .get_mut(&transfer.id)
                        .expect("reserved download");
                    entry.stored = true;
                    entry.cancel.clone()
                };
                tokio::spawn(async move {
                    service
                        .run_download(transfer, context, throttle, cancel)
                        .await;
                });
            }
            Ok(response)
        })
        .await?
    }

    async fn run_upload(
        &self,
        transfer: Transfer,
        source: PathBuf,
        cancel: CancellationToken,
        slot: oneshot::Receiver<()>,
    ) {
        let id = &transfer.id;
        let inserted = store::insert(self.database(), vec![transfer.clone()]).await;
        self.wrote(transfer.direction);
        match inserted {
            Ok(superseded) => self.publish_removed(superseded),
            Err(error) => {
                tracing::warn!(%error, %id, "could not persist queued upload");
                self.runtime().remove(id);
                return;
            }
        }
        if let Some(entry) = self.runtime().active.get_mut(id) {
            entry.stored = true;
        }
        self.hold_failing_upload(&transfer, &cancel).await;
        let result = self.upload(&transfer, source, cancel, slot).await;
        self.finish(id, result).await;
    }

    /// Keeps an upload queued while its file is backing off for this user after repeated
    /// failures (see [`backoff`]). It does not take a slot meanwhile, and the peer sees it
    /// queued, with the time of the next attempt.
    async fn hold_failing_upload(&self, t: &Transfer, cancel: &CancellationToken) {
        let delay = {
            let mut runtime = self.runtime();
            let Some(delay) = runtime.backoff.delay(&t.username, &t.filename) else {
                return;
            };
            if let Some(entry) = runtime.active.get_mut(&t.id) {
                entry.transfer.next_attempt_at = TimeDelta::from_std(delay)
                    .ok()
                    .and_then(|delay| Utc::now().checked_add_signed(delay));
                entry.touch();
            }
            delay
        };
        crate::profiling::UPLOADS_HELD.add(1);
        crate::profiling::UPLOADS_HOLDING.add(1);
        tracing::debug!(username = %t.username, filename = %t.filename, delay_s = delay.as_secs(),
            "holding an upload that keeps failing");
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(delay) => {}
        }
        crate::profiling::UPLOADS_HOLDING.add(-1);
        if let Some(entry) = self.runtime().active.get_mut(&t.id) {
            entry.transfer.next_attempt_at = None;
            entry.touch();
        }
    }

    async fn upload(
        &self,
        t: &Transfer,
        local: PathBuf,
        cancel: CancellationToken,
        slot: oneshot::Receiver<()>,
    ) -> soulseek::Result<PathBuf> {
        let size = t.size;
        let filename = t.filename.clone();
        let mut options = self.callbacks(&t.id);
        let expected_path = local.clone();
        let shares = self.0.shares.clone();
        let input = UploadInput::Stream {
            size,
            factory: Box::new(move |_| {
                Box::pin(async move {
                    let file = tokio::task::spawn_blocking(move || {
                        let (path, size_now) =
                            shares.resolve_file(&filename).map_err(stream_error)?;
                        if path != expected_path || size_now != size {
                            return Err(Error::TransferStream(
                                "Shared file changed before upload began".into(),
                            ));
                        }
                        destination::open_shared(&path, size).map_err(Error::from)
                    })
                    .await
                    .map_err(task_error)??;
                    Ok(Box::new(tokio::fs::File::from_std(file)) as Box<dyn TransferInput>)
                })
            }),
        };
        let service = self.clone();
        let id = t.id.clone();
        let receiver = Mutex::new(Some(slot));
        options.slot_awaiter = Some(Arc::new(move |_| {
            let slot = receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let cfg = service.0.core.options.get();
            {
                let mut runtime = service.runtime();
                runtime.uploads.ready(&id);
                runtime
                    .uploads
                    .dispatch(&service.0.users, &cfg, service.upload_slots(&cfg));
            }
            Box::pin(async move {
                slot.ok_or(Error::Cancelled)?
                    .await
                    .map_err(|_| Error::Cancelled)
            })
        }));
        if queue::has_group_speed_limit(&self.0.core.options.get()) {
            let service = self.clone();
            let id = t.id.clone();
            options.governor = Some(Arc::new(move |_, bytes| {
                let cfg = service.0.core.options.get();
                let (granted, ready) = {
                    let mut runtime = service.runtime();
                    let Runtime {
                        ref uploads,
                        ref mut bandwidth,
                        ..
                    } = *runtime;
                    bandwidth.grant(&cfg, uploads.group(&id), bytes)
                };
                Box::pin(async move {
                    if ready > tokio::time::Instant::now() {
                        tokio::time::sleep_until(ready).await;
                    }
                    granted
                })
            }));
        }
        self.0
            .core
            .client
            .enqueue_upload(&t.username, &t.filename, input, None, options, cancel)
            .await?
            .wait()
            .await?;
        Ok(local)
    }

    async fn run_download(
        &self,
        t: Transfer,
        context: Arc<DownloadContext>,
        throttle: Arc<Semaphore>,
        cancel: CancellationToken,
    ) {
        let cfg = self.0.core.options.get();
        let retry = &cfg.downloads;
        let subdirectory = destination::derive(
            &cfg,
            &t,
            context.batch.as_ref(),
            context.search_text.as_deref(),
        );
        let mut next = t.next_attempt_at;
        loop {
            if let Some(at) = next.take() {
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    _ = tokio::time::sleep((at - Utc::now()).to_std().unwrap_or_default()) => {},
                }
            }
            let result = match self.wait_for_login(&cancel).await {
                Ok(()) => {
                    let Some(size) = self.begin_attempt(&t.id) else {
                        return;
                    };
                    self.download(&t, size, &cfg, &subdirectory, &throttle, cancel.clone())
                        .await
                }
                Err(error) => Err(error),
            };
            if cancel.is_cancelled() || self.0.core.shutdown.is_cancelled() {
                self.finish(&t.id, result).await;
                return;
            }
            match result {
                Err(error) if retryable(&error) => {
                    next = self.schedule_retry(&t.id, retry, &error);
                    if next.is_some() {
                        self.flush().await;
                        continue;
                    }
                    self.finish(&t.id, Err(error)).await;
                }
                result => self.finish(&t.id, result).await,
            }
            return;
        }
    }

    async fn wait_for_login(&self, cancel: &CancellationToken) -> soulseek::Result<()> {
        let mut events = self.0.core.client.subscribe();
        while !self.0.core.client.state().is_logged_in() {
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                event = events.recv() => if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    return Err(Error::Connection("Client event stream closed".into()));
                },
            }
        }
        if cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn begin_attempt(&self, id: &str) -> Option<u64> {
        let mut runtime = self.runtime();
        let entry = runtime.active.get_mut(id)?;
        let t = &mut entry.transfer;
        t.attempts = t.attempts.saturating_add(1);
        t.next_attempt_at = None;
        t.error = None;
        t.place_in_queue = None;
        t.started_at = None;
        t.average_speed = 0.0;
        let size = t.size;
        entry.touch();
        Some(size)
    }

    fn schedule_retry(
        &self,
        id: &str,
        retry: &DownloadsConfig,
        error: &Error,
    ) -> Option<DateTime<Utc>> {
        let mut runtime = self.runtime();
        let entry = runtime.active.get_mut(id)?;
        let t = &mut entry.transfer;
        if t.attempts >= retry.retry_attempts.max(1) {
            return None;
        }
        let delay = retry
            .retry_delay
            .saturating_mul(1_u32 << t.attempts.saturating_sub(1).min(30))
            .min(retry.retry_max_delay);
        let at = TimeDelta::from_std(delay)
            .ok()
            .and_then(|delay| Utc::now().checked_add_signed(delay))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        t.next_attempt_at = Some(at);
        t.state = TransferState::QueuedLocally;
        t.error = Some(exception_text(error));
        entry.touch();
        Some(at)
    }

    async fn download(
        &self,
        t: &Transfer,
        expected: u64,
        cfg: &Config,
        subdir: &Path,
        throttle: &Semaphore,
        cancel: CancellationToken,
    ) -> soulseek::Result<PathBuf> {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            permit = throttle.acquire() => permit.map_err(|_| Error::Cancelled)?,
        };
        let rejected = || Error::TransferRejected("Download source is blacklisted".into());
        if self.0.users.is_blacklisted(&t.username, None) {
            return Err(rejected());
        }
        if !self.0.core.options.get().blacklist.cidrs.is_empty() {
            let endpoint = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                endpoint = self.0.core.client.get_user_endpoint(&t.username) => endpoint?,
            };
            if self
                .0
                .users
                .is_blacklisted(&t.username, Some(endpoint.ip()))
            {
                return Err(rejected());
            }
        }
        let settings = &cfg.downloads;
        let mode = destination::mode(Some(settings.file_mode.as_str()))?;
        let resume = settings.resume;
        let root = PathBuf::from(&settings.incomplete_directory);
        let known = (expected != 0).then(|| {
            Arc::new(destination::Partial::new(
                root.clone(),
                &t.username,
                &t.filename,
                expected,
            ))
        });
        let offset = if let Some(partial) = &known {
            let partial = partial.clone();
            tokio::task::spawn_blocking(move || partial.offset(resume, expected))
                .await
                .map_err(task_error)??
        } else {
            0
        };
        let remote = t.filename.clone();
        let username = t.username.clone();
        let service = self.clone();
        let id = t.id.clone();
        let partial = known.clone();
        let incomplete = root.clone();
        let (file_task, file_done) = oneshot::channel::<()>();
        let output = DownloadOutput::Stream(Box::new(move || {
            Box::pin(async move {
                let size = {
                    let runtime = service.runtime();
                    let entry = runtime.active.get(&id).ok_or(Error::Cancelled)?;
                    if entry.remote_size_invalid {
                        return Err(Error::TransferStream("Remote file is too large".into()));
                    }
                    entry.transfer.size
                };
                let (file, done) = tokio::task::spawn_blocking(move || {
                    let file = match partial {
                        Some(partial) => partial.open(offset),
                        None => {
                            destination::Partial::new(incomplete, &username, &remote, size).open(0)
                        }
                    }?;
                    Ok::<_, io::Error>((file, file_task))
                })
                .await
                .map_err(task_error)??;
                Ok(Box::new(destination::PartialOutput::new(file, done))
                    as Box<dyn TransferOutput>)
            })
        }));
        let transfer_cancel = cancel.clone();
        let outcome = async move {
            let handle = {
                let _permit = permit;
                self.0
                    .core
                    .client
                    .enqueue_download(
                        &t.username,
                        &t.filename,
                        output,
                        (expected != 0).then_some(expected),
                        offset,
                        None,
                        self.callbacks(&t.id),
                        transfer_cancel,
                    )
                    .await?
            };
            handle.wait().await
        };
        let result = outcome.await;
        let _ = file_done.await;
        let completed = result?;
        if cancel.is_cancelled() && !self.0.core.shutdown.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let partial = known.unwrap_or_else(|| {
            Arc::new(destination::Partial::new(
                root,
                &t.username,
                &t.filename,
                completed.size,
            ))
        });
        let downloads = PathBuf::from(&settings.directory);
        let subdir = subdir.to_owned();
        let overwrite = settings.on_conflict == OnConflict::Overwrite;
        tokio::task::spawn_blocking(move || {
            partial.finish(&downloads, &subdir, completed.size, overwrite, mode)
        })
        .await
        .map_err(task_error)?
        .map_err(Error::from)
    }

    fn callbacks(&self, id: &str) -> TransferOptions {
        let id: Arc<str> = Arc::from(id);
        let state_id = id.clone();
        let states = self.clone();
        let progress = self.clone();
        TransferOptions {
            state_changed: Some(Arc::new(move |_, wire| states.update(&state_id, wire))),
            progress_updated: Some(Arc::new(move |_, wire| progress.update(&id, wire))),
            ..TransferOptions::default()
        }
    }

    fn update(&self, id: &str, wire: &soulseek::Transfer) {
        let mut runtime = self.runtime();
        let Some(entry) = runtime.active.get_mut(id) else {
            return;
        };
        let state = state_from_bits(wire.state.0);
        let t = &mut entry.transfer;
        let before = (
            t.state,
            t.enqueued_at,
            t.started_at,
            t.size,
            entry.offset,
            entry.remote_size_invalid,
        );
        let progress = (t.bytes_transferred, t.average_speed);
        // A transport completion is not a daemon completion: downloads still
        // need to move the file, and failed attempts can be retried.
        if !state.is_complete() {
            t.state = state;
        }
        if (state == TransferState::QueuedRemotely
            || t.direction == TransferDirection::Upload && state == TransferState::QueuedLocally)
            && t.enqueued_at.is_none()
        {
            t.enqueued_at = Some(Utc::now());
        }
        if t.started_at.is_none() {
            t.started_at = wire.start_time.map(DateTime::<Utc>::from);
        }
        entry.remote_size_invalid = wire.size > i64::MAX as u64;
        if !entry.remote_size_invalid {
            t.size = wire.size;
        }
        t.bytes_transferred = wire
            .bytes_transferred
            .min(i64::MAX as u64)
            .max(wire.start_offset);
        t.average_speed = wire.average_speed;
        entry.offset = wire.start_offset;
        let t = &entry.transfer;
        if before
            != (
                t.state,
                t.enqueued_at,
                t.started_at,
                t.size,
                entry.offset,
                entry.remote_size_invalid,
            )
        {
            entry.touch();
        } else if progress != (t.bytes_transferred, t.average_speed) {
            // Pure byte progress: persisted less often, published on the next flush.
            entry.progress = true;
            entry.changed = true;
        }
    }

    async fn finish(&self, id: &str, result: soulseek::Result<PathBuf>) {
        let (transfer, last, ban) = {
            let cfg = self.0.core.options.get();
            let mut runtime = self.runtime();
            let Some(entry) = runtime.active.get_mut(id) else {
                return;
            };
            if result.is_err() && self.0.core.shutdown.is_cancelled() && !entry.user_cancelled {
                entry.done.cancel();
                return;
            }
            let t = &mut entry.transfer;
            let aborted = peer_aborted(t.state, &result);
            let (disposition, reason) = match &result {
                Ok(_) => (TransferState::Succeeded, None),
                Err(error) => (failure_state(error), Some(exception_text(error))),
            };
            t.state = disposition;
            t.error = reason;
            t.ended_at = Some(Utc::now());
            t.next_attempt_at = None;
            t.refresh_derived(Utc::now());
            let transfer = t.clone();
            entry.touch();
            let offset = entry.offset;
            crate::profiling::transfer_ended(transfer.direction, disposition);
            if transfer.direction == TransferDirection::Upload {
                runtime.uploads.release(id);
                runtime.history.record(&transfer);
                runtime.backoff.record(&transfer, offset, aborted);
                runtime
                    .uploads
                    .dispatch(&self.0.users, &cfg, self.upload_slots(&cfg));
            }
            let completed = &mut runtime.completed[direction_index(transfer.direction)];
            if disposition == TransferState::Succeeded {
                completed.succeeded += 1;
                completed.bytes += transfer.bytes_transferred;
            } else {
                completed.failed += 1;
            }
            let directory = destination::remote_directory(&transfer.filename);
            let last = transfer.direction == TransferDirection::Download
                && result.is_ok()
                && !runtime.active.values().any(|entry| {
                    let t = &entry.transfer;
                    t.direction == TransferDirection::Download
                        && !t.state.is_complete()
                        && t.username == transfer.username
                        && destination::remote_directory(&t.filename) == directory
                });
            let ban = transfer.direction == TransferDirection::Upload
                && disposition != TransferState::Succeeded
                && self.loop_ban_due(&mut runtime, &cfg, &transfer.username);
            (transfer, last, ban)
        };
        self.flush().await;
        log_outcome(&transfer, &result);
        if ban {
            self.ban_looping_user(transfer.username.clone());
        }
        if let Ok(local) = result {
            self.raise_completed(transfer, local, last);
        }
    }

    fn raise_completed(&self, transfer: Transfer, local: PathBuf, last: bool) {
        let local_filename = local.to_string_lossy().into_owned();
        let complete = |transfer: Transfer| FileComplete {
            local_filename,
            remote_filename: transfer.filename.clone(),
            transfer,
        };
        match transfer.direction {
            TransferDirection::Upload => self
                .0
                .events
                .raise(EventData::UploadFileComplete(complete(transfer))),
            TransferDirection::Download => {
                let directory = last.then(|| DirectoryComplete {
                    local_directory: local
                        .parent()
                        .unwrap_or(&local)
                        .to_string_lossy()
                        .into_owned(),
                    remote_directory: destination::remote_directory(&transfer.filename).into(),
                    username: transfer.username.clone(),
                });
                self.0
                    .events
                    .raise(EventData::DownloadFileComplete(complete(transfer)));
                if let Some(directory) = directory {
                    self.0
                        .events
                        .raise(EventData::DownloadDirectoryComplete(directory));
                }
            }
        }
    }

    /// Publishes changed transfers and persists due changes: state changes now, byte
    /// progress every [`PROGRESS_PERSIST_INTERVAL`]. Drops ended transfers once persisted.
    async fn flush(&self) {
        self.flush_with(false).await;
    }

    /// Like [`Self::flush`], but persists all pending progress too (shutdown).
    async fn flush_all(&self) {
        self.flush_with(true).await;
    }

    async fn flush_with(&self, all: bool) {
        let _write = self.0.writes.lock().await;
        let mut finished = Vec::new();
        let (updates, directions) = {
            let mut runtime = self.runtime();
            let bus = &self.0.core.bus;
            let publish = bus.wants(Topic::Transfers);
            let (instant, now) = (Instant::now(), Utc::now());
            let mut changed = Vec::new();
            let mut updates = Vec::new();
            let mut directions = [false; 2];
            for entry in runtime.active.values_mut() {
                if !entry.stored {
                    continue;
                }
                if std::mem::take(&mut entry.changed) && publish {
                    let mut transfer = entry.transfer.clone();
                    transfer.refresh_derived(now);
                    changed.push(transfer);
                }
                let due = entry.dirty
                    || entry.progress
                        && (all
                            || instant.duration_since(entry.persisted_at)
                                >= PROGRESS_PERSIST_INTERVAL);
                if !due {
                    continue;
                }
                entry.dirty = false;
                entry.progress = false;
                entry.persisted_at = instant;
                let transfer = &entry.transfer;
                if transfer.state.is_complete() {
                    finished.push(transfer.id.clone());
                }
                directions[direction_index(transfer.direction)] = true;
                updates.push(Snapshot::new(transfer, entry.offset));
            }
            if !changed.is_empty() {
                // In list order, and under the runtime lock so that a snapshot taken
                // concurrently is exactly before or after this batch.
                changed
                    .sort_unstable_by(|a, b| (a.requested_at, &a.id).cmp(&(b.requested_at, &b.id)));
                bus.publish(&LiveEvent::TransferUpdated(changed));
            }
            (updates, directions)
        };
        if updates.is_empty() {
            return;
        }
        let saved = store::save(self.database(), updates).await;
        for direction in [TransferDirection::Download, TransferDirection::Upload] {
            if directions[direction_index(direction)] {
                self.wrote(direction);
            }
        }
        if let Err(error) = saved {
            tracing::warn!(%error, "could not persist transfer updates");
            let mut runtime = self.runtime();
            for id in finished {
                if let Some(entry) = runtime.active.get(&id) {
                    entry.done.cancel();
                }
            }
            for entry in runtime.active.values_mut() {
                entry.dirty |= entry.stored;
            }
            return;
        }
        let mut runtime = self.runtime();
        for id in finished {
            // A cancel/remove request can update a snapshot while the write is running.
            if runtime
                .active
                .get(&id)
                .is_some_and(|entry| !entry.dirty && !entry.progress)
            {
                runtime.remove(&id);
            }
        }
    }

    /// The transfers the daemon is working on, for the `transfers` topic snapshot, with
    /// the bus sequence number they include.
    pub(crate) fn snapshot(&self) -> (u64, Vec<Transfer>) {
        // Batches are published under the runtime lock (see `flush_with`).
        let runtime = self.runtime();
        let seq = self.0.core.bus.seq();
        let now = Utc::now();
        let mut transfers = runtime
            .active
            .values()
            .filter(|entry| entry.stored)
            .map(|entry| {
                let mut transfer = entry.transfer.clone();
                transfer.refresh_derived(now);
                transfer
            })
            .collect::<Vec<_>>();
        drop(runtime);
        transfers.sort_unstable_by(|a, b| (a.requested_at, &a.id).cmp(&(b.requested_at, &b.id)));
        (seq, transfers)
    }

    /// Tells `transfers` subscribers that these transfers left the list.
    fn publish_removed(&self, ids: Vec<String>) {
        if !ids.is_empty() {
            self.0
                .core
                .bus
                .publish_with(Topic::Transfers, || LiveEvent::TransferRemoved { ids });
        }
    }

    fn overlay(&self, transfers: &mut [Transfer]) {
        let now = Utc::now();
        let runtime = self.runtime();
        for transfer in transfers {
            if let Some(entry) = runtime.active.get(&transfer.id) {
                let removed = transfer.removed;
                *transfer = entry.transfer.clone();
                transfer.removed |= removed;
                transfer.refresh_derived(now);
            }
        }
    }

    /// Records a write to the stored `direction` list. Call it after every store write, even
    /// one that failed: a list read that raced the write is then not tagged as current.
    fn wrote(&self, direction: TransferDirection) {
        self.0.list_revisions[direction_index(direction)].fetch_add(1, Ordering::SeqCst);
    }

    /// Live rows need fresh elapsed time, progress and queue positions on each poll.
    fn idle_list_revision(&self, direction: TransferDirection, user: Option<&str>) -> Option<u64> {
        let runtime = self.runtime();
        let live = runtime.active.values().any(|entry| {
            entry.transfer.direction == direction
                && user.is_none_or(|user| entry.transfer.username == user)
        });
        (!live).then(|| self.0.list_revisions[direction_index(direction)].load(Ordering::SeqCst))
    }

    async fn list(
        &self,
        direction: TransferDirection,
        user: Option<String>,
        include_removed: bool,
        recent_completed: Option<u32>,
    ) -> anyhow::Result<Vec<Transfer>> {
        self.ready().await?;
        let mut transfers = store::list(
            self.database(),
            direction,
            user,
            include_removed,
            recent_completed,
        )
        .await?;
        self.overlay(&mut transfers);
        if !include_removed {
            transfers.retain(|t| !t.removed);
        }
        Ok(transfers)
    }

    async fn find(
        &self,
        direction: TransferDirection,
        id: String,
    ) -> anyhow::Result<Option<Transfer>> {
        self.ready().await?;
        let mut transfer = store::find(self.database(), direction, id).await?;
        if let Some(transfer) = &mut transfer {
            self.overlay(std::slice::from_mut(transfer));
        }
        Ok(transfer)
    }

    async fn batch(&self, id: String) -> anyhow::Result<Option<Batch>> {
        self.ready().await?;
        let mut batch = store::batch(self.database(), id).await?;
        if let Some(batch) = batch.as_mut() {
            self.overlay(&mut batch.transfers);
        }
        Ok(batch)
    }

    async fn cancel(
        &self,
        direction: TransferDirection,
        id: String,
        remove: bool,
    ) -> anyhow::Result<()> {
        self.ready().await?;
        let active = {
            let mut runtime = self.runtime();
            runtime
                .active
                .get_mut(&id)
                .filter(|entry| entry.transfer.direction == direction)
                .map(|entry| {
                    entry.user_cancelled = true;
                    entry.cancel.cancel();
                    entry.transfer.removed |= remove;
                    if remove {
                        entry.touch();
                    }
                    entry.done.clone()
                })
        };
        // A transfer that is no longer active only needs the (optional) removal.
        if let Some(done) = active {
            done.cancelled().await;
        }
        if remove {
            let removed = store::remove(self.database(), direction, Some(id)).await;
            self.wrote(direction);
            let removed = removed?;
            self.publish_removed(removed);
        }
        Ok(())
    }

    async fn download_position(&self, t: &Transfer) -> anyhow::Result<u32> {
        let inactive = || {
            Error::TransferNotFound(format!(
                "A download of {} from user {} is not active",
                t.filename, t.username
            ))
        };
        let (attempt, cancel) = self
            .runtime()
            .active
            .get(&t.id)
            .filter(|entry| !entry.transfer.state.is_complete())
            .map(|entry| (entry.transfer.attempts, entry.cancel.clone()))
            .ok_or_else(inactive)?;
        let pos = self
            .0
            .core
            .client
            .get_download_place_in_queue(&t.username, &t.filename, cancel)
            .await?;
        let _write = self.0.writes.lock().await;
        {
            let mut runtime = self.runtime();
            let entry = runtime
                .active
                .get_mut(&t.id)
                .filter(|entry| {
                    entry.transfer.attempts == attempt && !entry.transfer.state.is_complete()
                })
                .ok_or_else(inactive)?;
            entry.transfer.place_in_queue = Some(pos);
            entry.touch();
        }
        let saved = store::set_position(self.database(), t.id.clone(), pos).await;
        self.wrote(t.direction);
        saved?;
        Ok(pos)
    }
}

impl DownloadContext {
    async fn new(app: &AppState, batch: Option<Batch>) -> Self {
        let search_text = if let Some(id) = batch.as_ref().and_then(|b| b.search_id.as_deref()) {
            match app.services.search.search_text(id).await {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%error, %id, "could not look up transfer batch search text");
                    None
                }
            }
        } else {
            None
        };
        Self { batch, search_text }
    }
}

fn new_transfer(
    direction: TransferDirection,
    username: &str,
    filename: String,
    size: u64,
    batch_id: Option<String>,
) -> Transfer {
    Transfer {
        id: Uuid::new_v4().to_string(),
        batch_id,
        username: username.into(),
        direction,
        filename,
        root: None,
        size,
        state: TransferState::QueuedLocally,
        requested_at: Utc::now(),
        enqueued_at: None,
        started_at: None,
        ended_at: None,
        place_in_queue: None,
        error: None,
        attempts: 0,
        next_attempt_at: None,
        removed: false,
        bytes_transferred: 0,
        average_speed: 0.0,
        bytes_remaining: size,
        percent_complete: 0.0,
        elapsed_ms: None,
        remaining_ms: None,
    }
}

fn direction_index(direction: TransferDirection) -> usize {
    match direction {
        TransferDirection::Download => 0,
        TransferDirection::Upload => 1,
    }
}

fn is_queued(state: TransferState) -> bool {
    state == TransferState::Requested || state.is_queued()
}

fn task_error(error: tokio::task::JoinError) -> Error {
    Error::Io(std::io::Error::other(error))
}

fn stream_error(error: impl std::fmt::Display) -> Error {
    Error::TransferStream(error.to_string())
}

fn retryable(error: &Error) -> bool {
    matches!(
        error,
        Error::Connection(_)
            | Error::Timeout(_)
            | Error::NoResponse(_)
            | Error::UserOffline(_)
            | Error::InvalidState(_)
            | Error::UserEndPoint(_)
            | Error::TransferReportedFailed(_)
    )
}

/// Whether a failed upload was the peer accepting it and then closing the file connection
/// before asking for any data (instead of sending the resume offset). Only this counts
/// towards the automatic ban: it is what broken or abusive clients loop on, while failing
/// to reach the peer, a refusal, a timeout, a dropped connection mid-file (a bad link, on
/// either side) and errors reading the file here can all happen to well-behaved users.
fn peer_aborted(prior: TransferState, result: &soulseek::Result<PathBuf>) -> bool {
    matches!(
        (prior, result),
        (TransferState::Initializing, Err(Error::Connection(message)))
            if message.starts_with(UPLOAD_OFFSET_FAILURE)
    )
}

/// How the client words a peer closing the file connection before the resume offset
/// (`soulseek::client::transfers::stream::upload`).
const UPLOAD_OFFSET_FAILURE: &str = "Could not read the upload resume offset";

fn failure_state(error: &Error) -> TransferState {
    match error {
        Error::Cancelled => TransferState::Cancelled,
        Error::Timeout(_) | Error::NoResponse(_) => TransferState::TimedOut,
        Error::TransferRejected(_) => TransferState::Rejected,
        Error::TransferSizeMismatch { .. } => TransferState::Aborted,
        _ => TransferState::Errored,
    }
}

/// One log line per finished transfer: successes and cancellations at info, failures at
/// warn with the reason, so failed uploads can be found in the log.
fn log_outcome(t: &Transfer, result: &soulseek::Result<PathBuf>) {
    let direction = t.direction.as_str();
    let username = t.username.as_str();
    let filename = t.filename.as_str();
    match result {
        Ok(_) => tracing::info!(
            direction,
            username,
            filename,
            bytes = t.bytes_transferred,
            speed_kib = (t.average_speed / 1024.0).round(),
            "{direction} succeeded"
        ),
        Err(Error::Cancelled) => {
            tracing::info!(direction, username, filename, "{direction} cancelled")
        }
        Err(error) => tracing::warn!(
            direction,
            username,
            filename,
            bytes = t.bytes_transferred,
            size = t.size,
            %error,
            "{direction} failed"
        ),
    }
}

fn exception_text(error: &Error) -> String {
    let kind = match error {
        Error::Cancelled => "OperationCanceledException",
        Error::Timeout(_) => "TimeoutException",
        Error::NoResponse(_) => "NoResponseException",
        Error::UserOffline(_) => "UserOfflineException",
        Error::TransferRejected(_) => "TransferRejectedException",
        Error::TransferSizeMismatch { .. } => "TransferSizeMismatchException",
        Error::TransferStream(_) => "TransferStreamException",
        Error::TransferReportedFailed(_) => "TransferReportedFailedException",
        Error::Connection(_) => "ConnectionException",
        Error::Io(_) => "IOException",
        _ => "TransferException",
    };
    format!("{kind}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(filename: &str) -> Transfer {
        new_transfer(TransferDirection::Upload, "peer", filename.into(), 1, None)
    }

    #[test]
    fn finished_entry_does_not_block_or_unindex_a_re_request() {
        let file = r"Music\a.flac";
        let mut runtime = Runtime::default();
        let mut first = upload(file);
        first.ended_at = Some(Utc::now());
        first.state = TransferState::Succeeded;
        let first_id = first.id.clone();
        runtime.insert(first, CancellationToken::new(), None, true);
        assert!(runtime.has(TransferDirection::Upload, "peer", file));
        assert!(!runtime.has_unfinished(TransferDirection::Upload, "peer", file));

        let second = upload(file);
        let second_id = second.id.clone();
        runtime.insert(second, CancellationToken::new(), None, false);
        assert!(runtime.has_unfinished(TransferDirection::Upload, "peer", file));

        // Dropping the lingering finished entry keeps the live one indexed.
        runtime.remove(&first_id);
        assert!(runtime.has_unfinished(TransferDirection::Upload, "peer", file));
        runtime.remove(&second_id);
        assert!(!runtime.has(TransferDirection::Upload, "peer", file));
        assert!(runtime.files[direction_index(TransferDirection::Upload)].is_empty());
    }

    /// The `transfer_updated` batches of the published events (other events fail).
    fn batches(
        events: &mut tokio::sync::broadcast::Receiver<crate::realtime::Published>,
    ) -> Vec<Vec<Transfer>> {
        let mut batches = Vec::new();
        while let Ok(published) = events.try_recv() {
            match serde_json::from_str(published.frame.as_str()).unwrap() {
                LiveEvent::TransferUpdated(batch) => batches.push(batch),
                other => panic!("unexpected {other:?}"),
            }
        }
        batches
    }

    #[tokio::test]
    async fn writes_change_only_their_own_list_revision() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::bootstrap::test_app(dir.path(), "[soulseek]\nconnect = false\n", &[])
            .await
            .unwrap();
        let service = app.services.transfers.clone();
        service.0.ready.send_replace(Some(Ok(())));
        let revision =
            |direction| service.0.list_revisions[direction_index(direction)].load(Ordering::SeqCst);
        let (download, upload) = (TransferDirection::Download, TransferDirection::Upload);

        // A persisted download change bumps only the downloads list.
        let transfer = new_transfer(download, "peer", r"album\one.flac".into(), 100, None);
        store::insert(service.database(), vec![transfer.clone()])
            .await
            .unwrap();
        let id = transfer.id.clone();
        service
            .runtime()
            .insert(transfer, CancellationToken::new(), None, true);
        service.runtime().active.get_mut(&id).unwrap().touch();
        let before = (revision(download), revision(upload));
        service.flush().await;
        assert_eq!(
            (revision(download), revision(upload)),
            (before.0 + 1, before.1)
        );

        // Removing a stored upload bumps only the uploads list, and changes its tag.
        let finished = new_transfer(upload, "peer", r"album\two.flac".into(), 100, None);
        store::insert(service.database(), vec![finished.clone()])
            .await
            .unwrap();
        let tag = service.idle_list_revision(upload, None);
        let before = (revision(download), revision(upload));
        service.cancel(upload, finished.id, true).await.unwrap();
        assert_eq!(
            (revision(download), revision(upload)),
            (before.0, before.1 + 1)
        );
        assert_ne!(service.idle_list_revision(upload, None), tag);
    }

    #[tokio::test]
    async fn flush_publishes_every_change_and_throttles_progress_writes() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::bootstrap::test_app(dir.path(), "[soulseek]\nconnect = false\n", &[])
            .await
            .unwrap();
        let service = app.services.transfers.clone();
        let filename = r"album\track.flac";
        let transfer = new_transfer(
            TransferDirection::Download,
            "peer",
            filename.into(),
            100,
            None,
        );
        let id = transfer.id.clone();
        store::insert(service.database(), vec![transfer.clone()])
            .await
            .unwrap();
        service
            .runtime()
            .insert(transfer, CancellationToken::new(), None, true);
        let stored = || async {
            store::find(service.database(), TransferDirection::Download, id.clone())
                .await
                .unwrap()
                .unwrap()
        };

        // Without subscribers nothing is published, and the pending change is dropped.
        let mut events = app.bus.receiver();
        service.flush().await;
        assert!(batches(&mut events).is_empty());

        let mut connection = app.bus.connection();
        connection.subscribe(Topic::Transfers);
        let (seq, snapshot) = service.snapshot();
        assert_eq!(seq, app.bus.seq());
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, id);

        // A state change is published and persisted on the next flush.
        let mut wire = soulseek::Transfer::new(
            soulseek::TransferDirection::Download,
            "peer",
            filename,
            1,
            soulseek::TransferState::IN_PROGRESS,
            100,
            0,
        );
        wire.bytes_transferred = 10;
        service.update(&id, &wire);
        service.flush().await;
        let published = batches(&mut events);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0][0].state, TransferState::InProgress);
        assert_eq!(published[0][0].bytes_transferred, 10);
        assert_eq!(stored().await.state, TransferState::InProgress);
        assert_eq!(stored().await.bytes_transferred, 10);

        // Byte progress is published at once but persisted only after the interval.
        wire.bytes_transferred = 40;
        service.update(&id, &wire);
        service.update(&id, &wire);
        service.flush().await;
        let published = batches(&mut events);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0][0].bytes_transferred, 40);
        assert_eq!(published[0][0].percent_complete, 40.0);
        assert_eq!(stored().await.bytes_transferred, 10);
        // Nothing changed: nothing is published.
        service.flush().await;
        assert!(batches(&mut events).is_empty());
        service.runtime().active.get_mut(&id).unwrap().persisted_at -= PROGRESS_PERSIST_INTERVAL;
        service.flush().await;
        assert_eq!(stored().await.bytes_transferred, 40);
        assert!(batches(&mut events).is_empty());

        // Shutdown persists pending progress at once.
        wire.bytes_transferred = 60;
        service.update(&id, &wire);
        service.flush_all().await;
        assert_eq!(stored().await.bytes_transferred, 60);
        batches(&mut events);

        // Completion is persisted and published, then the entry is dropped.
        wire.bytes_transferred = 100;
        service.update(&id, &wire);
        service.finish(&id, Ok(dir.path().join("track.flac"))).await;
        let published = batches(&mut events);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0][0].state, TransferState::Succeeded);
        let stored = stored().await;
        assert_eq!(stored.state, TransferState::Succeeded);
        assert_eq!(stored.bytes_transferred, 100);
        assert!(service.runtime().active.is_empty());
        assert!(service.snapshot().1.is_empty());

        // Removals name the transfers that left the list.
        let removed = store::remove(service.database(), TransferDirection::Download, None)
            .await
            .unwrap();
        assert_eq!(removed, vec![id.clone()]);
        service.publish_removed(removed);
        let published = events.try_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<LiveEvent>(published.frame.as_str()).unwrap(),
            LiveEvent::TransferRemoved { ids: vec![id] }
        );
    }
}
