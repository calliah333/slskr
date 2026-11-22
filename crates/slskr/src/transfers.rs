//! Persistent Soulseek transfers, upload scheduling, and live progress.

mod api;
mod destination;
mod queue;
mod store;

pub use api::routes;

use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use slskr_api::{
    events::{
        DownloadDirectoryCompleteEvent, DownloadFileCompleteEvent, Event, EventHeader, EventType,
        UploadFileCompleteEvent,
    },
    options::{DownloadRetryOptions, Options, TransfersOptions},
    telemetry::{CompletedMetrics, CurrentMetrics},
    transfers::{
        Batch, EnqueueDownloadBatchResponseFailure, EnqueueDownloadsResponse, QueueDownloadRequest,
        Transfer, TransferDirection, TransferStates,
    },
};
use soulseek::{
    Error, TransferOptions,
    client::{DownloadOutput, TransferInput, TransferOutput, UploadInput},
};
use tokio::{
    io::{AsyncRead, AsyncSeek, ReadBuf},
    sync::{Semaphore, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    app::{AppState, Core},
    db::Database,
    events::EventService,
    relay::RelayService,
    shares::ShareService,
    users::UserService,
};
use queue::{Bandwidth, UploadHistory, UploadQueue};
use store::Snapshot;

const LOCAL_QUEUE: TransferStates = TransferStates::from_bits(2 | 2048);
const REMOTE_QUEUE: TransferStates = TransferStates::from_bits(2 | 4096);

#[derive(Clone)]
pub struct TransferService(Arc<Inner>);

struct Inner {
    core: Core,
    events: EventService,
    users: UserService,
    shares: ShareService,
    relay: OnceLock<RelayService>,
    db: Database,
    runtime: Mutex<Runtime>,
    writes: tokio::sync::Mutex<()>,
    ready: watch::Sender<Option<Result<(), String>>>,
    download_requests: Semaphore,
}

#[derive(Default)]
struct Runtime {
    active: HashMap<String, Active>,
    files: [HashMap<String, HashSet<String>>; 2],
    uploads: UploadQueue,
    history: UploadHistory,
    bandwidth: Bandwidth,
    completed: [CompletedMetrics; 2],
}

struct Active {
    transfer: Transfer,
    cancel: CancellationToken,
    done: CancellationToken,
    endpoint: Option<SocketAddr>,
    stored: bool,
    user_cancelled: bool,
    dirty: bool,
    offset: u64,
    remote_size_invalid: bool,
}

enum UploadSource {
    Local(PathBuf),
    Relay {
        service: RelayService,
        agent: String,
    },
}

// A relay reader starts at the requested offset; it cannot seek thereafter.
struct RelayInput<T>(T);

impl<T: AsyncRead + Unpin> AsyncRead for RelayInput<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: Unpin> AsyncSeek for RelayInput<T> {
    fn start_seek(self: Pin<&mut Self>, _: io::SeekFrom) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Relay input does not support seeking",
        ))
    }

    fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Relay input does not support seeking",
        )))
    }
}

#[derive(Default)]
struct DownloadContext {
    batch: Option<Batch>,
    search_text: Option<String>,
}

impl Runtime {
    fn has(&self, direction: TransferDirection, username: &str, filename: &str) -> bool {
        self.files[direction_index(direction)]
            .get(username)
            .is_some_and(|files| files.contains(filename))
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
            .insert(transfer.filename.clone());
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
            files.remove(&t.filename);
            if files.is_empty() {
                users.remove(&t.username);
            }
        }
        if t.direction == TransferDirection::Upload && !t.state.contains(TransferStates::COMPLETED)
        {
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
        let path = if core.options.startup().flags.volatile {
            PathBuf::from(":memory:")
        } else {
            core.paths.data_dir.join("transfers.db")
        };
        Ok(Self(Arc::new(Inner {
            core: core.clone(),
            events: events.clone(),
            users: users.clone(),
            shares: shares.clone(),
            db: Database::open(&path, store::MIGRATIONS)?,
            runtime: Mutex::default(),
            writes: tokio::sync::Mutex::new(()),
            ready: watch::channel(None).0,
            download_requests: Semaphore::new(2),
            relay: OnceLock::new(),
        })))
    }

    pub fn start(&self, app: AppState) {
        let _ = self.0.relay.set(app.services.relay.clone());
        let service = self.clone();
        tokio::spawn(async move {
            match store::initialize(service.database()).await {
                Ok((history, downloads)) => {
                    service.recover(&app, history, downloads).await;
                    service.0.ready.send_replace(Some(Ok(())));
                }
                Err(error) => {
                    tracing::error!(%error, "could not initialize transfers");
                    service.0.ready.send_replace(Some(Err(error.to_string())));
                    return;
                }
            }

            let mut options = service.0.core.options.subscribe();
            let mut progress = tokio::time::interval(Duration::from_millis(250));
            let mut retention = tokio::time::interval(Duration::from_secs(60));
            progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            retention.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = service.0.core.shutdown.cancelled() => { service.flush().await; break; }
                    _ = progress.tick() => { service.flush().await; }
                    _ = retention.tick() => {
                        service.runtime().history.prune();
                        let cfg = service.0.core.options.get();
                        if let Err(error) = store::prune(service.database(), cfg.retention.transfers.clone()).await {
                            tracing::warn!(%error, "could not apply transfer retention");
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
                        runtime.uploads.dispatch(&service.0.users, &cfg.transfers, service.upload_slots(&cfg.transfers));
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

    fn upload_slots(&self, cfg: &TransfersOptions) -> u32 {
        // The SDK owns a fixed data semaphore. A reload can lower the daemon
        // queue limit immediately, but increases beyond that semaphore need a restart.
        cfg.upload
            .slots
            .min(self.0.core.options.startup().transfers.upload.slots)
            .max(0) as u32
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
        if self.0.users.is_blacklisted(username, Some(endpoint.ip())) {
            return Err("File not shared.".into());
        }
        if self
            .runtime()
            .has(TransferDirection::Upload, username, filename)
        {
            return Ok(());
        }
        let (source, size) = self.upload_source(filename).ok_or("File not shared.")?;
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
            if runtime.has(TransferDirection::Upload, username, filename) {
                return Ok(());
            }
            let group = self.0.users.group(username);
            runtime.history.check(
                runtime
                    .active
                    .values()
                    .map(|entry| &entry.transfer)
                    .filter(|t| t.direction == TransferDirection::Upload && is_queued(t.state)),
                &cfg.transfers,
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

    fn upload_source(&self, filename: &str) -> Option<(UploadSource, u64)> {
        if let Ok((path, size)) = self.0.shares.resolve_file(filename) {
            return Some((UploadSource::Local(path), size));
        }
        let service = self.0.relay.get()?;
        let (agent, size) = service.published_file(filename)?;
        Some((
            UploadSource::Relay {
                service: service.clone(),
                agent,
            },
            size,
        ))
    }

    pub fn place_in_queue(&self, username: &str, filename: &str) -> Option<u32> {
        let cfg = self.0.core.options.get();
        self.runtime()
            .uploads
            .position(&self.0.users, &cfg.transfers, username, filename)
    }

    pub fn upload_info(&self) -> (u32, u32, bool) {
        let cfg = self.0.core.options.get();
        self.runtime()
            .uploads
            .info(self.upload_slots(&cfg.transfers))
    }

    pub fn upload_info_for(&self, username: &str) -> (u32, u32, bool) {
        let cfg = self.0.core.options.get();
        self.runtime().uploads.forecast(
            &self.0.users,
            &cfg.transfers,
            self.upload_slots(&cfg.transfers),
            username,
        )
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
                direction.queued.files += 1.0;
                direction.queued.bytes += t.size as f64;
            } else if matches!(
                t.state,
                TransferStates::INITIALIZING | TransferStates::IN_PROGRESS
            ) {
                in_progress[index].insert(t.username.as_str());
                direction.in_progress.files += 1.0;
                direction.in_progress.total_speed += t.average_speed;
            }
        }
        for (index, direction) in [(0, &mut metrics.downloads), (1, &mut metrics.uploads)] {
            direction.queued.users = queued[index].len() as f64;
            direction.in_progress.users = in_progress[index].len() as f64;
            if direction.in_progress.files > 0.0 {
                direction.in_progress.average_speed =
                    direction.in_progress.total_speed / direction.in_progress.files;
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
        files: Vec<QueueDownloadRequest>,
        context: DownloadContext,
    ) -> anyhow::Result<EnqueueDownloadsResponse> {
        self.ready().await?;
        let service = self.clone();
        tokio::spawn(async move {
            let context = Arc::new(context);
            let cancel = service.0.core.shutdown.child_token();
            let mut response = EnqueueDownloadsResponse::default();
            {
                let mut runtime = service.runtime();
                for file in files {
                    if runtime.has(TransferDirection::Download, &username, &file.filename) {
                        response.failed.push(EnqueueDownloadBatchResponseFailure {
                            filename: file.filename,
                            message: "Skipped: Already in progress".into(),
                        });
                        continue;
                    }
                    let transfer = new_transfer(
                        TransferDirection::Download,
                        &username,
                        file.filename,
                        file.size,
                        context.batch.as_ref().map(|b| b.id.clone()),
                    );
                    runtime.insert(transfer.clone(), cancel.child_token(), None, false);
                    response.enqueued.push(transfer);
                }
            }
            if response.enqueued.is_empty() {
                return Ok(response);
            }
            if let Err(error) = store::insert(service.database(), response.enqueued.clone()).await {
                let mut runtime = service.runtime();
                for transfer in &response.enqueued {
                    runtime.remove(&transfer.id);
                }
                return Err(error);
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
        source: UploadSource,
        cancel: CancellationToken,
        slot: oneshot::Receiver<()>,
    ) {
        let id = &transfer.id;
        if let Err(error) = store::insert(self.database(), vec![transfer.clone()]).await {
            tracing::warn!(%error, %id, "could not persist queued upload");
            self.runtime().remove(id);
            return;
        }
        if let Some(entry) = self.runtime().active.get_mut(id) {
            entry.stored = true;
        }
        let result = self.upload(&transfer, source, cancel, slot).await;
        self.finish(id, result).await;
    }

    async fn upload(
        &self,
        t: &Transfer,
        source: UploadSource,
        cancel: CancellationToken,
        slot: oneshot::Receiver<()>,
    ) -> soulseek::Result<PathBuf> {
        let size = t.size;
        let filename = t.filename.clone();
        let mut options = self.callbacks(&t.id);
        let (local, input) = match source {
            UploadSource::Local(local) => {
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
                (local, input)
            }
            UploadSource::Relay { service, agent } => {
                options.seek_input_stream_automatically = false;
                let local = PathBuf::from(&filename);
                let optimistic = self.0.core.options.get().flags.optimistic_relay_file_info;
                let input = UploadInput::Stream {
                    size,
                    factory: Box::new(move |offset| {
                        Box::pin(async move {
                            if !service
                                .published_file(&filename)
                                .is_some_and(|(owner, total)| owner == agent && total == size)
                            {
                                return Err(Error::TransferStream(
                                    "Published relay file changed before upload began".into(),
                                ));
                            }
                            if !optimistic
                                && service
                                    .file_info(&agent, &filename)
                                    .await
                                    .map_err(stream_error)?
                                    != Some(size)
                            {
                                return Err(Error::TransferStream(
                                    "Shared relay file changed before upload began".into(),
                                ));
                            }
                            let file = service
                                .file_stream(&agent, &filename, offset, size - offset)
                                .await
                                .map_err(stream_error)?;
                            Ok(Box::new(RelayInput(file)) as Box<dyn TransferInput>)
                        })
                    }),
                };
                (local, input)
            }
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
                runtime.uploads.dispatch(
                    &service.0.users,
                    &cfg.transfers,
                    service.upload_slots(&cfg.transfers),
                );
            }
            Box::pin(async move {
                slot.ok_or(Error::Cancelled)?
                    .await
                    .map_err(|_| Error::Cancelled)
            })
        }));
        if queue::has_group_speed_limit(&self.0.core.options.get().transfers) {
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
                    bandwidth.grant(&cfg.transfers, uploads.group(&id), bytes)
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
        let retry = &cfg.transfers.download.retry;
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
        t.exception = None;
        t.place_in_queue = None;
        t.started_at = None;
        t.average_speed = 0.0;
        entry.dirty = true;
        Some(t.size)
    }

    fn schedule_retry(
        &self,
        id: &str,
        retry: &DownloadRetryOptions,
        error: &Error,
    ) -> Option<DateTime<Utc>> {
        let mut runtime = self.runtime();
        let entry = runtime.active.get_mut(id)?;
        let t = &mut entry.transfer;
        if t.attempts >= retry.attempts.max(1) as u32 {
            return None;
        }
        let base = i64::from(retry.delay.max(0));
        let delay =
            (base << t.attempts.saturating_sub(1).min(30)).min(i64::from(retry.max_delay.max(0)));
        let at = Utc::now() + TimeDelta::milliseconds(delay);
        t.next_attempt_at = Some(at);
        t.state = LOCAL_QUEUE;
        t.exception = Some(exception_text(error));
        entry.dirty = true;
        Some(at)
    }

    async fn download(
        &self,
        t: &Transfer,
        expected: u64,
        cfg: &Options,
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
        if !self
            .0
            .core
            .options
            .get()
            .transfers
            .groups
            .blacklisted
            .cidrs
            .is_empty()
        {
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
        let settings = &cfg.transfers.download;
        let mode = destination::mode(settings.destination.permissions.mode.as_deref())?;
        let resume = settings.retry.partial.eq_ignore_ascii_case("resume");
        let root = PathBuf::from(&cfg.directories.incomplete);
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
        let downloads = PathBuf::from(&cfg.directories.downloads);
        let subdir = subdir.to_owned();
        let overwrite = settings
            .destination
            .exists
            .eq_ignore_ascii_case("overwrite");
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
        let state = TransferStates::from_bits(wire.state.0);
        let t = &mut entry.transfer;
        // A transport completion is not a daemon completion: downloads still
        // need to move the file, and failed attempts can be retried.
        if !state.contains(TransferStates::COMPLETED) {
            t.state = state;
        }
        if (state == REMOTE_QUEUE
            || t.direction == TransferDirection::Upload && state == LOCAL_QUEUE)
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
        entry.dirty = true;
    }

    async fn finish(&self, id: &str, result: soulseek::Result<PathBuf>) {
        let (transfer, last) = {
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
            let (disposition, reason) = match &result {
                Ok(_) => (TransferStates::SUCCEEDED, None),
                Err(error) => (failure_state(error), Some(exception_text(error))),
            };
            t.state = TransferStates::COMPLETED | disposition;
            t.exception = reason;
            t.ended_at = Some(Utc::now());
            t.next_attempt_at = None;
            t.refresh_derived(Utc::now());
            entry.dirty = true;
            let transfer = t.clone();
            if transfer.direction == TransferDirection::Upload {
                runtime.uploads.release(id);
                runtime.history.record(&transfer);
                runtime.uploads.dispatch(
                    &self.0.users,
                    &cfg.transfers,
                    self.upload_slots(&cfg.transfers),
                );
            }
            let completed = &mut runtime.completed[direction_index(transfer.direction)];
            if disposition == TransferStates::SUCCEEDED {
                completed.succeeded += 1.0;
                completed.bytes += transfer.bytes_transferred as f64;
            } else {
                completed.failed += 1.0;
            }
            let directory = destination::remote_directory(&transfer.filename);
            let last = transfer.direction == TransferDirection::Download
                && result.is_ok()
                && !runtime.active.values().any(|entry| {
                    let t = &entry.transfer;
                    t.direction == TransferDirection::Download
                        && !t.state.contains(TransferStates::COMPLETED)
                        && t.username == transfer.username
                        && destination::remote_directory(&t.filename) == directory
                });
            (transfer, last)
        };
        self.flush().await;
        match result {
            Ok(local) => self.raise_completed(transfer, local, last),
            Err(error) => tracing::debug!(%id, %error, "transfer ended unsuccessfully"),
        }
    }

    fn raise_completed(&self, transfer: Transfer, local: PathBuf, last: bool) {
        let timestamp = transfer.ended_at.unwrap_or_else(Utc::now);
        let header = |kind| EventHeader {
            id: Uuid::new_v4().to_string(),
            timestamp,
            kind,
            version: 1,
        };
        let value = serde_json::to_value(&transfer).unwrap_or_default();
        let local_filename = local.to_string_lossy().into_owned();
        match transfer.direction {
            TransferDirection::Upload => {
                self.0
                    .events
                    .raise(Event::UploadFileComplete(UploadFileCompleteEvent {
                        header: header(EventType::UploadFileComplete),
                        local_filename,
                        remote_filename: transfer.filename,
                        transfer: value,
                    }))
            }
            TransferDirection::Download => {
                self.0
                    .events
                    .raise(Event::DownloadFileComplete(DownloadFileCompleteEvent {
                        header: header(EventType::DownloadFileComplete),
                        local_filename,
                        remote_filename: transfer.filename.clone(),
                        transfer: value,
                    }));
                if last {
                    self.0.events.raise(Event::DownloadDirectoryComplete(
                        DownloadDirectoryCompleteEvent {
                            header: header(EventType::DownloadDirectoryComplete),
                            local_directory_name: local
                                .parent()
                                .unwrap_or(&local)
                                .to_string_lossy()
                                .into_owned(),
                            remote_directory_name: destination::remote_directory(
                                &transfer.filename,
                            )
                            .into(),
                            username: Some(transfer.username),
                        },
                    ));
                }
            }
        }
    }

    async fn flush(&self) {
        let _write = self.0.writes.lock().await;
        let mut finished = Vec::new();
        let updates = self
            .runtime()
            .active
            .values_mut()
            .filter_map(|entry| {
                if !entry.stored || !std::mem::take(&mut entry.dirty) {
                    return None;
                }
                let transfer = &entry.transfer;
                if transfer.state.contains(TransferStates::COMPLETED) {
                    finished.push(transfer.id.clone());
                }
                Some(Snapshot::new(transfer, entry.offset))
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return;
        }
        if let Err(error) = store::save(self.database(), updates).await {
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
            if runtime.active.get(&id).is_some_and(|entry| !entry.dirty) {
                runtime.remove(&id);
            }
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

    async fn list(
        &self,
        direction: TransferDirection,
        user: Option<String>,
        include_removed: bool,
    ) -> anyhow::Result<Vec<Transfer>> {
        self.ready().await?;
        let mut transfers = store::list(self.database(), direction, user, include_removed).await?;
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
        if let Some(transfers) = batch.as_mut().and_then(|b| b.transfers.as_mut()) {
            self.overlay(transfers);
        }
        Ok(batch)
    }

    async fn cancel(
        &self,
        direction: TransferDirection,
        id: String,
        remove: bool,
    ) -> anyhow::Result<bool> {
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
                    entry.dirty |= remove;
                    entry.done.clone()
                })
        };
        if let Some(done) = active {
            done.cancelled().await;
        } else if store::find(self.database(), direction, id.clone())
            .await?
            .is_none()
        {
            return Ok(false);
        }
        if remove {
            store::remove(self.database(), direction, Some(id)).await?;
        }
        Ok(true)
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
            .filter(|entry| !entry.transfer.state.contains(TransferStates::COMPLETED))
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
                    entry.transfer.attempts == attempt
                        && !entry.transfer.state.contains(TransferStates::COMPLETED)
                })
                .ok_or_else(inactive)?;
            entry.transfer.place_in_queue = Some(pos);
            entry.dirty = true;
        }
        store::set_position(self.database(), t.id.clone(), pos).await?;
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
        size,
        state: LOCAL_QUEUE,
        requested_at: Utc::now(),
        enqueued_at: None,
        started_at: None,
        ended_at: None,
        place_in_queue: None,
        exception: None,
        attempts: 0,
        next_attempt_at: None,
        removed: false,
        bytes_transferred: 0,
        average_speed: 0.0,
        bytes_remaining: size as i64,
        percent_complete: 0.0,
        elapsed_time: None,
        remaining_time: None,
    }
}

fn direction_index(direction: TransferDirection) -> usize {
    match direction {
        TransferDirection::Download => 0,
        TransferDirection::Upload => 1,
    }
}

fn is_queued(state: TransferStates) -> bool {
    state == TransferStates::REQUESTED || state.contains(TransferStates::QUEUED)
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

fn failure_state(error: &Error) -> TransferStates {
    match error {
        Error::Cancelled => TransferStates::CANCELLED,
        Error::Timeout(_) | Error::NoResponse(_) => TransferStates::TIMED_OUT,
        Error::TransferRejected(_) => TransferStates::REJECTED,
        Error::TransferSizeMismatch { .. } => TransferStates::ABORTED,
        _ => TransferStates::ERRORED,
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
