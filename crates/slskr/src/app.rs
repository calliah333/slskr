//! Application wiring shared by every HTTP handler and service.
//!
//! Construction order (see `bootstrap`): core handles first, then services in
//! dependency order, then the Soulseek client resolvers are bound to the finished
//! application. Services receive the [`Core`] plus the services they depend on;
//! HTTP handlers receive [`AppState`] (cheap clone of an `Arc`).

use std::{
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use slskr_types::{
    application::{ConnectionWatchdogState, DistributedNetworkState, UserStatisticsState},
    config::Config,
    events::{EventData, ServerDisconnected},
    realtime::{Event as LiveEvent, Topic},
    server::{ConnectionState, ServerState},
};
use soulseek::{ClientState, DistributedNetworkInfo, Event as ClientEvent};
use tokio::sync::{broadcast, watch};

use tokio_util::sync::CancellationToken;

use crate::{
    auth::Auth, config::ConfigStore, events::EventService, files::FileService, logging::LogBuffer,
    messaging::MessagingService, realtime::EventBus, search::SearchService, shares::ShareService,
    state::StateStore, telemetry::TelemetryService, transfers::TransferService, users::UserService,
};

/// Filesystem locations resolved at startup.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Data directory (`--data-dir`); relative configuration paths resolve against it.
    pub data_dir: PathBuf,
    /// Configuration file (`--config`, default `{data_dir}/slskr.toml`).
    pub config_file: PathBuf,
    /// Sqlite databases and the share cache (`{data_dir}/db`).
    pub db_dir: PathBuf,
    /// Log files (`{data_dir}/logs`).
    pub logs_dir: PathBuf,
    /// Web UI directory from `--web-dir`; `None` serves the UI embedded in the binary.
    pub web_dir: Option<PathBuf>,
}

/// Core handles every service may use. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    pub paths: Paths,
    pub options: ConfigStore,
    pub state: StateStore,
    pub client: soulseek::SoulseekClient,
    /// Live updates for `/api/events`.
    pub bus: EventBus,
    pub auth: Auth,
    pub logs: LogBuffer,
    pub shutdown: CancellationToken,
}

/// Every service, built in dependency order.
pub struct Services {
    pub events: EventService,
    pub users: UserService,
    pub shares: ShareService,
    pub files: FileService,
    pub telemetry: TelemetryService,
    pub transfers: TransferService,
    pub search: SearchService,
    pub messaging: MessagingService,
}

pub struct AppInner {
    pub core: Core,
    pub services: Services,
    /// First-run setup: the one-time setup code and its attempt limits.
    pub setup: crate::setup::Setup,
    connection: watch::Sender<ConnectionDirective>,
    restart: AtomicBool,
    pub(crate) search_filters: RwLock<Vec<regex::Regex>>,
    pub(crate) excluded_search_phrases: RwLock<Vec<String>>,
    pub(crate) upload_speed: AtomicU32,
    /// When our statistics were last re-read after reporting an upload speed.
    statistics_refreshed: std::sync::Mutex<Option<tokio::time::Instant>>,
}

/// How often our own statistics are re-read after reporting an upload speed.
const STATISTICS_REFRESH: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
struct ConnectionDirective {
    enabled: bool,
    revision: u64,
}

impl AppInner {
    pub(crate) fn new(core: Core, services: Services) -> Self {
        let startup = core.options.startup();
        let wants_connection = startup.soulseek.connect;
        let enabled = wants_connection && credentials(startup).is_some();
        if wants_connection && !enabled {
            tracing::warn!(
                "Soulseek credentials are not configured; automatic connection is disabled"
            );
        }
        let search_filters = RwLock::new(compile_search_filters(startup));
        core.state.update(|state| {
            state.server = ServerState::default();
            state.connection_watchdog.is_enabled = enabled;
        });
        Self {
            core,
            services,
            setup: crate::setup::Setup::new(),
            connection: watch::channel(ConnectionDirective {
                enabled,
                revision: 0,
            })
            .0,
            restart: AtomicBool::new(false),
            search_filters,
            excluded_search_phrases: RwLock::new(Vec::new()),
            upload_speed: AtomicU32::new(0),
            statistics_refreshed: std::sync::Mutex::new(None),
        }
    }
}

/// Axum state: the whole application.
#[derive(Clone)]
pub struct AppState(pub Arc<AppInner>);

impl Deref for AppState {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.0
    }
}

impl Deref for AppInner {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

impl AppState {
    /// Starts the watchdog, or resets its delay if it is already enabled.
    pub async fn connect(&self) -> Result<bool> {
        ensure!(
            !self.shutdown.is_cancelled(),
            "application is shutting down"
        );
        let was_enabled = self.set_connection(true);
        let waiting = self.client.state() == ClientState::DISCONNECTED;
        self.state
            .update(|state| state.connection_watchdog.is_enabled = waiting);
        let was_enabled = was_enabled && waiting;
        Ok(was_enabled)
    }

    /// Stops retries and cancels any in-flight connection.
    pub async fn disconnect(&self, message: &str) {
        self.set_connection(false);
        self.state
            .update(|state| state.connection_watchdog = Default::default());
        self.client.disconnect(message, None).await;
    }

    fn set_connection(&self, enabled: bool) -> bool {
        let mut was_enabled = false;
        self.connection.send_modify(|request| {
            was_enabled = request.enabled;
            request.enabled = enabled;
            request.revision = request.revision.wrapping_add(1);
        });
        was_enabled
    }

    fn stop_retrying(&self, attempted: ConnectionDirective) {
        self.connection.send_if_modified(|request| {
            if *request != attempted || !request.enabled {
                return false;
            }
            request.enabled = false;
            true
        });
    }

    pub fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn request_restart(&self) {
        self.restart.store(true, Ordering::Release);
        self.request_shutdown();
    }

    pub(crate) fn restart_requested(&self) -> bool {
        self.restart.load(Ordering::Acquire)
    }

    pub(crate) fn start(
        &self,
        client_events_done: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        self.state
            .spawn_publisher(self.bus.clone(), self.shutdown.clone());
        crate::logging::start_retention(&self.core);
        self.auth.sessions().spawn_pruner(self.shutdown.clone());
        crate::setup::sync(self);
        self.services.users.start(self.clone());
        self.services.shares.start();
        self.services.files.start(self.clone());
        self.services.transfers.start(self.clone());
        self.services.telemetry.start(&self.core);
        crate::profiling::start(self.shutdown.clone());
        self.services.search.start(self.clone());
        self.services.messaging.start(self.clone());

        let events = self.client.subscribe();
        let watchdog_events = self.client.subscribe();
        let connection = self.connection.subscribe();
        let options = self.options.subscribe();
        self.options.watch_file(self.shutdown.clone());
        tokio::spawn(self.clone().watch_options(options));
        tokio::spawn(self.clone().watch_connection(connection, watchdog_events));
        tokio::spawn(self.clone().watch_client(events, client_events_done))
    }

    async fn watch_connection(
        self,
        mut desired: watch::Receiver<ConnectionDirective>,
        mut events: broadcast::Receiver<ClientEvent>,
    ) {
        let mut states = self.state.subscribe();
        let mut delay = Duration::ZERO;
        loop {
            let attempted = *desired.borrow_and_update();
            let enabled = attempted.enabled && self.client.state() == ClientState::DISCONNECTED;
            let before = states.borrow_and_update().connection_watchdog.clone();
            let watchdog = ConnectionWatchdogState {
                is_enabled: enabled,
                is_attempting_connection: enabled,
                next_attempt_at: enabled.then(|| Utc::now() + delay),
            };
            if before != watchdog {
                self.state
                    .update(|state| state.connection_watchdog = watchdog);
            }

            if !enabled {
                tokio::select! {
                    _ = self.shutdown.cancelled() => break,
                    result = desired.changed() => if result.is_err() { break },
                    disconnected = next_disconnect(&mut events) => match disconnected {
                        Some(true) => self.stop_retrying(attempted),
                        Some(false) => {}
                        None => break,
                    },
                }
                delay = Duration::ZERO;
                continue;
            }

            // Only user commands interrupt an attempt. Client state events from this attempt
            // are consumed after it returns; reacting to them here would cancel our own login.
            let result = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                changed = desired.changed() => {
                    if changed.is_err() { break; }
                    delay = Duration::ZERO;
                    continue;
                }
                result = async {
                    tokio::time::sleep(delay).await;
                    let options = self.options.get();
                    let (username, password) = credentials(&options).context("Soulseek credentials are not configured")?;
                    let (host, port) = crate::config::server_endpoint(&options.soulseek.server)?;
                    self.client.connect(&host, port, username, password).await?;
                    Ok::<_, anyhow::Error>(())
                } => result,
            };
            // Work through queued events before considering another attempt: a kicked
            // client can be disconnected immediately after a successful login.
            let mut stop = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<soulseek::Error>())
                .is_some_and(terminal_disconnect);
            loop {
                match events.try_recv() {
                    Ok(ClientEvent::Disconnected { error, .. }) => {
                        stop |= error.as_deref().is_some_and(terminal_disconnect)
                    }
                    Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            if stop {
                self.stop_retrying(attempted);
            }
            if let Err(error) = result {
                tracing::warn!(%error, "Soulseek connection attempt failed");
                delay = if delay.is_zero() {
                    Duration::from_secs(1)
                } else {
                    (delay * 2).min(Duration::from_secs(300))
                };
            } else {
                delay = Duration::ZERO;
            }
        }
    }

    async fn watch_client(
        self,
        mut events: broadcast::Receiver<ClientEvent>,
        done: CancellationToken,
    ) {
        loop {
            let event = tokio::select! {
                biased;
                event = events.recv() => event,
                _ = done.cancelled() => break,
            };
            match event {
                Ok(ClientEvent::StateChanged { state, .. }) => self.update_server(state),
                Ok(ClientEvent::DistributedNetworkStateChanged(info)) => {
                    self.update_distributed(info)
                }
                Ok(ClientEvent::ExcludedSearchPhrasesReceived(phrases)) => {
                    *self
                        .excluded_search_phrases
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = phrases;
                }
                Ok(ClientEvent::TransferStateChanged { transfer, .. })
                    if transfer.direction == soulseek::TransferDirection::Upload
                        && transfer.state.contains(
                            soulseek::TransferState::COMPLETED | soulseek::TransferState::SUCCEEDED,
                        ) =>
                {
                    self.report_upload_speed(transfer.average_speed)
                }
                Ok(ClientEvent::UserStatisticsChanged(statistics))
                    if self.client.username().as_deref() == Some(statistics.username.as_str()) =>
                {
                    self.update_statistics(statistics)
                }
                Ok(ClientEvent::Connected) => {
                    tracing::info!("Connected to the Soulseek server");
                    self.services.events.raise(EventData::ServerConnected);
                }
                Ok(ClientEvent::LoggedIn) => {
                    self.state.update(|state| state.pending_reconnect = false);
                    tokio::spawn(self.clone().initialize_session());
                }
                Ok(ClientEvent::Disconnected { message, error }) => {
                    self.state.update(|state| state.pending_reconnect = false);
                    self.services
                        .events
                        .raise(EventData::ServerDisconnected(ServerDisconnected {
                            message: Some(message),
                            error: error.map(|error| error.to_string()),
                        }));
                }
                Ok(ClientEvent::DiagnosticGenerated { level, message }) => {
                    use soulseek::DiagnosticLevel::{Debug, Info, None, Trace, Warning};
                    match level {
                        None => {}
                        Warning => tracing::warn!(target: "soulseek", "{message}"),
                        Info => tracing::info!(target: "soulseek", "{message}"),
                        Debug => tracing::debug!(target: "soulseek", "{message}"),
                        Trace => tracing::trace!(target: "soulseek", "{message}"),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.update_server(self.client.state());
                    self.update_distributed(self.client.distributed_network());
                }
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    }

    fn update_server(&self, current: ClientState) {
        let disconnected = current == ClientState::DISCONNECTED;
        let server = ServerState {
            address: if disconnected {
                None
            } else {
                self.client.address()
            },
            endpoint: if disconnected {
                None
            } else {
                self.client
                    .ip_endpoint()
                    .map(|endpoint| endpoint.to_string())
            },
            state: connection_state(current),
        };
        self.state.update(|state| {
            state.server = server;
            state.user.username = self.client.username();
            if disconnected {
                state.user.privileges = Default::default();
            }
        });
    }

    fn update_distributed(&self, info: DistributedNetworkInfo) {
        self.state.update(|state| {
            state.distributed_network = DistributedNetworkState {
                branch_level: info.branch_level.try_into().unwrap_or(i32::MAX),
                branch_root: (!info.branch_root.is_empty()).then_some(info.branch_root),
                can_accept_children: info.can_accept_children,
                child_limit: info.child_limit.try_into().unwrap_or(i32::MAX),
                children: info
                    .children
                    .into_iter()
                    .map(|peer| peer.username)
                    .collect(),
                has_parent: info.has_parent,
                is_branch_root: info.is_branch_root,
                parent: info.parent.map(|peer| peer.username),
            }
        });
    }

    async fn initialize_session(self) {
        let Some(username) = self.client.username() else {
            return;
        };
        let (statistics, privileges) = tokio::select! {
            _ = self.shutdown.cancelled() => return,
            result = async { tokio::join!(self.client.get_user_statistics(&username), self.client.get_privileges()) } => result,
        };
        if !self.client.state().is_logged_in()
            || self.client.username().as_deref() != Some(&username)
        {
            return;
        }
        if let Ok(statistics) = statistics {
            self.update_statistics(statistics);
        }
        if let Ok(seconds) = privileges {
            self.state.update(|state| {
                state.user.privileges.is_privileged = seconds > 0;
                state.user.privileges.privileges_remaining = seconds;
            });
        }
    }

    /// Reports a finished upload's speed, as other clients do. The server averages
    /// these into our statistics, whose average speed other clients see and our search
    /// responses advertise; clients rank sources by it. Re-reads the statistics at most
    /// every [`STATISTICS_REFRESH`] so the advertised speed follows.
    fn report_upload_speed(&self, bytes_per_second: f64) {
        if !bytes_per_second.is_finite() || bytes_per_second < 1.0 {
            return;
        }
        let speed = bytes_per_second.ceil().min(f64::from(u32::MAX)) as u32;
        let refresh = {
            let now = tokio::time::Instant::now();
            let mut last = self
                .statistics_refreshed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let due = last.is_none_or(|at| now.duration_since(at) >= STATISTICS_REFRESH);
            if due {
                *last = Some(now);
            }
            due
        };
        let app = self.clone();
        tokio::spawn(async move {
            if let Err(error) = app.client.send_upload_speed(speed).await {
                tracing::debug!(%error, "could not report upload speed");
                return;
            }
            let Some(username) = app.client.username().filter(|_| refresh) else {
                return;
            };
            if let Ok(statistics) = app.client.get_user_statistics(&username).await
                && app.client.username().as_deref() == Some(username.as_str())
            {
                app.update_statistics(statistics);
            }
        });
    }

    fn update_statistics(&self, statistics: soulseek::UserStatistics) {
        let stats = UserStatisticsState {
            average_speed: statistics.average_speed,
            directory_count: statistics.directory_count,
            file_count: statistics.file_count,
            upload_count: statistics.upload_count,
        };
        self.upload_speed
            .store(statistics.average_speed, Ordering::Relaxed);
        self.state.update(|state| state.user.statistics = stats);
    }

    async fn watch_options(self, mut receiver: watch::Receiver<Arc<Config>>) {
        let mut previous = receiver.borrow().clone();
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                change = receiver.changed() => if change.is_err() { break },
            }
            let current = receiver.borrow_and_update().clone();
            self.auth.update(&crate::config::auth_config(&current));
            crate::setup::sync(&self);
            if previous.search.ignore_queries != current.search.ignore_queries {
                *self
                    .search_filters
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    compile_search_filters(&current);
            }

            let reconnect = match crate::config::client_patch(&self.client.options(), &current) {
                Ok(patch) if patch == soulseek::ClientOptionsPatch::default() => false,
                Ok(patch) => match self.client.reconfigure(patch).await {
                    Ok(reconnect) => reconnect,
                    Err(error) => {
                        tracing::warn!(%error, "could not update Soulseek client options");
                        true
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "invalid updated Soulseek client options");
                    true
                }
            };
            let old = &previous.soulseek;
            let new = &current.soulseek;
            let credentials_changed = old.server != new.server
                || old.username != new.username
                || old.password != new.password;
            let restart = crate::config::restart_required(self.options.startup(), &current);
            self.state.update(|state| {
                state.pending_reconnect |=
                    reconnect || (state.server.state.is_connected() && credentials_changed);
                state.pending_restart = restart;
            });
            self.bus.publish_with(Topic::Settings, || {
                LiveEvent::Settings(Box::new(current.redact()))
            });
            previous = current;
        }
    }
}

async fn next_disconnect(events: &mut broadcast::Receiver<ClientEvent>) -> Option<bool> {
    loop {
        match events.recv().await {
            Ok(ClientEvent::Disconnected { error, .. }) => {
                return Some(error.as_deref().is_some_and(terminal_disconnect));
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return Some(false),
            Err(broadcast::error::RecvError::Closed) => return None,
            Ok(_) => {}
        }
    }
}

fn terminal_disconnect(error: &soulseek::Error) -> bool {
    matches!(
        error,
        soulseek::Error::LoginRejected(_) | soulseek::Error::KickedFromServer
    )
}

fn compile_search_filters(options: &Config) -> Vec<regex::Regex> {
    options
        .search
        .ignore_queries
        .iter()
        .filter_map(|pattern| {
            crate::config::regex(pattern)
                .map_err(|error| tracing::warn!(%error, "ignoring invalid search request filter"))
                .ok()
        })
        .collect()
}

/// The API connection state of the client's state flags.
pub(crate) fn connection_state(state: ClientState) -> ConnectionState {
    if state.contains(ClientState::LOGGED_IN) {
        ConnectionState::LoggedIn
    } else if state.contains(ClientState::LOGGING_IN) {
        ConnectionState::LoggingIn
    } else if state.contains(ClientState::CONNECTED) {
        ConnectionState::Connected
    } else if state.contains(ClientState::CONNECTING) {
        ConnectionState::Connecting
    } else if state.contains(ClientState::DISCONNECTING) {
        ConnectionState::Disconnecting
    } else {
        ConnectionState::Disconnected
    }
}

pub(crate) fn credentials(options: &Config) -> Option<(&str, &str)> {
    let s = &options.soulseek;
    let username = Some(s.username.as_str()).filter(|value| !value.trim().is_empty())?;
    let password = Some(s.password.as_str()).filter(|value| !value.trim().is_empty())?;
    Some((username, password))
}

impl crate::realtime::Snapshots for AppState {
    fn snapshot<'a>(
        &'a self,
        topic: Topic,
        bus: &'a EventBus,
    ) -> futures::future::BoxFuture<'a, (u64, LiveEvent)> {
        Box::pin(async move {
            // Read the sequence number first: the data then includes every event up to it.
            let seq = bus.seq();
            match topic {
                Topic::State => (seq, LiveEvent::State(Box::new(self.state.get()))),
                Topic::Settings => (
                    seq,
                    LiveEvent::Settings(Box::new(self.options.get().redact())),
                ),
                Topic::Metrics => (seq, LiveEvent::Metrics(self.services.telemetry.metrics())),
                Topic::Searches => {
                    let (seq, searches) = self.services.search.snapshot();
                    (seq, LiveEvent::SearchList(searches))
                }
                Topic::Logs => {
                    let (seq, records) = self.logs.snapshot_with_seq();
                    (seq, LiveEvent::LogBuffer(records))
                }
                Topic::Transfers => {
                    let (seq, transfers) = self.services.transfers.snapshot();
                    (seq, LiveEvent::TransferList(transfers))
                }
                Topic::Conversations => {
                    let (seq, conversations) =
                        self.services.messaging.conversations.snapshot().await;
                    (seq, LiveEvent::ConversationList(conversations))
                }
                Topic::Rooms => {
                    let (seq, rooms) = self.services.messaging.rooms.snapshot();
                    (seq, LiveEvent::RoomList(rooms))
                }
            }
        })
    }
}
