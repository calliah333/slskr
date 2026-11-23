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
use serde_json::json;
use slskr_api::{
    application::{
        DistributedNetworkState, ServerConnectionWatchdogState, UserStatisticsState, hub_methods,
    },
    events::{
        Event, EventHeader, EventType, SoulseekClientConnectedEvent,
        SoulseekClientDisconnectedEvent,
    },
    options::Options,
    server::{ServerState, SoulseekClientStates},
};
use soulseek::{ClientState, DistributedNetworkInfo, Event as ClientEvent};
use tokio::sync::{broadcast, watch};

use tokio_util::sync::CancellationToken;

use crate::{
    auth::Auth, config::OptionsStore, events::EventService, files::FileService, logging::LogBuffer,
    messaging::MessagingService, search::SearchService, shares::ShareService, signalr::Hub,
    state::StateStore, telemetry::TelemetryService, transfers::TransferService, users::UserService,
};

/// Filesystem locations resolved at startup.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Application directory (slskd `--app-dir`), holds config, data and logs.
    pub app_dir: PathBuf,
    /// YAML configuration file (slskd `--config`, default `{app_dir}/slskd.yml`).
    pub config_file: PathBuf,
    /// Sqlite databases (`{app_dir}/data`).
    pub data_dir: PathBuf,
    /// Log files (`{app_dir}/logs`).
    pub logs_dir: PathBuf,
    /// Static web UI root (`web.content_path` / `SLSKR_CONTENT_PATH`), if any.
    pub content_path: Option<PathBuf>,
}

/// SignalR hubs mounted at `/hub/{name}`.
#[derive(Clone)]
pub struct Hubs {
    pub application: Hub,
    pub logs: Hub,
    pub search: Hub,
    pub metrics: Hub,
}

/// Core handles every service may use. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    pub paths: Paths,
    pub options: OptionsStore,
    pub state: StateStore,
    pub client: soulseek::SoulseekClient,
    pub hubs: Hubs,
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
    connection: watch::Sender<ConnectionDirective>,
    restart: AtomicBool,
    pub(crate) search_filters: RwLock<Vec<regex::Regex>>,
    pub(crate) excluded_search_phrases: RwLock<Vec<String>>,
    pub(crate) upload_speed: AtomicU32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ConnectionDirective {
    enabled: bool,
    revision: u64,
}

impl AppInner {
    pub(crate) fn new(core: Core, services: Services) -> Self {
        let startup = core.options.startup();
        let wants_connection = !startup.flags.no_connect;
        let enabled = wants_connection && credentials(startup).is_some();
        if wants_connection && !enabled {
            tracing::warn!(
                "Soulseek credentials are not configured; automatic connection is disabled"
            );
        }
        let search_filters = RwLock::new(compile_search_filters(startup));
        core.state.update(|state| {
            state.server = ServerState::new(None, None, SoulseekClientStates::DISCONNECTED);
            state.connection_watchdog.is_enabled = enabled;
        });
        Self {
            core,
            services,
            connection: watch::channel(ConnectionDirective {
                enabled,
                revision: 0,
            })
            .0,
            restart: AtomicBool::new(false),
            search_filters,
            excluded_search_phrases: RwLock::new(Vec::new()),
            upload_speed: AtomicU32::new(0),
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
        startup_key: [u8; 32],
        client_events_done: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.0);
        self.hubs.application.on_connected(move || {
            let Some(app) = weak.upgrade() else {
                return Vec::new();
            };
            vec![
                (hub_methods::STATE.into(), vec![json!(app.state.get())]),
                (
                    hub_methods::OPTIONS.into(),
                    vec![json!(app.options.get().redact())],
                ),
            ]
        });

        self.services.users.start(self.clone());
        self.services.shares.start();
        self.services.files.start(self.clone());
        self.services.transfers.start(self.clone());
        self.services.telemetry.start(&self.core);
        self.services.search.start(self.clone());
        self.services.messaging.start(self.clone());

        let events = self.client.subscribe();
        let watchdog_events = self.client.subscribe();
        let connection = self.connection.subscribe();
        let options = self.options.subscribe();
        self.options.watch_file(self.shutdown.clone());
        tokio::spawn(self.clone().watch_options(options, startup_key));
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
            let watchdog = ServerConnectionWatchdogState {
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
                    let s = &options.soulseek;
                    let (username, password) = credentials(&options).context("Soulseek credentials are not configured")?;
                    self.client.connect(&s.address, s.port.try_into()?, username, password).await?;
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
                Ok(ClientEvent::UserStatisticsChanged(statistics))
                    if self.client.username().as_deref() == Some(statistics.username.as_str()) =>
                {
                    self.update_statistics(statistics)
                }
                Ok(ClientEvent::Connected) => {
                    tracing::info!("Connected to the Soulseek server");
                    self.services.events.raise(Event::SoulseekClientConnected(
                        SoulseekClientConnectedEvent {
                            header: event_header(EventType::SoulseekClientConnected),
                        },
                    ));
                }
                Ok(ClientEvent::LoggedIn) => {
                    self.state.update(|state| state.pending_reconnect = false);
                    tokio::spawn(self.clone().initialize_session());
                }
                Ok(ClientEvent::Disconnected { message, error }) => {
                    self.state.update(|state| state.pending_reconnect = false);
                    self.services
                        .events
                        .raise(Event::SoulseekClientDisconnected(
                            SoulseekClientDisconnectedEvent {
                                header: event_header(EventType::SoulseekClientDisconnected),
                                message: Some(message),
                                exception: error.map_or(
                                    serde_json::Value::Null,
                                    |error| json!({"message": error.to_string()}),
                                ),
                            },
                        ));
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
        let server = ServerState::new(
            if disconnected {
                None
            } else {
                self.client.address()
            },
            if disconnected {
                None
            } else {
                self.client
                    .ip_endpoint()
                    .map(|endpoint| endpoint.to_string())
            },
            SoulseekClientStates(current.0),
        );
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
                state.user.privileges.privileges_remaining = seconds.try_into().unwrap_or(i32::MAX);
            });
        }
    }

    fn update_statistics(&self, statistics: soulseek::UserStatistics) {
        let stats = UserStatisticsState {
            average_speed: statistics.average_speed.try_into().unwrap_or(i32::MAX),
            directory_count: statistics.directory_count.try_into().unwrap_or(i32::MAX),
            file_count: statistics.file_count.try_into().unwrap_or(i32::MAX),
            upload_count: statistics.upload_count.try_into().unwrap_or(i64::MAX),
        };
        self.upload_speed
            .store(statistics.average_speed, Ordering::Relaxed);
        self.state.update(|state| state.user.statistics = stats);
    }

    async fn watch_options(
        self,
        mut receiver: watch::Receiver<Arc<Options>>,
        startup_key: [u8; 32],
    ) {
        let mut previous = receiver.borrow().clone();
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                change = receiver.changed() => if change.is_err() { break },
            }
            let current = receiver.borrow_and_update().clone();
            match crate::config::auth_config(&current, &startup_key) {
                Ok(auth) => self.auth.update(&auth),
                Err(error) => {
                    tracing::warn!(%error, "ignoring invalid updated authentication configuration")
                }
            }
            if previous.filters.search.request != current.filters.search.request
                || previous.flags.case_sensitive_reg_ex != current.flags.case_sensitive_reg_ex
            {
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
            let credentials_changed = old.address != new.address
                || old.port != new.port
                || old.username != new.username
                || old.password != new.password;
            let startup = self.options.startup();
            let web = &current.web;
            let initial_web = &startup.web;
            let restart = startup.headless != current.headless
                || startup.flags != current.flags
                || startup.logger != current.logger
                || startup.soulseek.diagnostic_level != current.soulseek.diagnostic_level
                || startup.transfers.upload.slots != current.transfers.upload.slots
                || startup.transfers.download.slots != current.transfers.download.slots
                || startup.throttling.search.incoming.concurrency
                    != current.throttling.search.incoming.concurrency
                || startup.metrics.enabled != current.metrics.enabled
                || startup.metrics.url != current.metrics.url
                || initial_web.port != web.port
                || initial_web.ip_address != web.ip_address
                || initial_web.url_base != web.url_base
                || initial_web.content_path != web.content_path
                || initial_web.logging != web.logging
                // Auth::update reloads credentials and API keys only, as slskd marks these
                // [RequiresRestart].
                || initial_web.authentication.disabled != web.authentication.disabled
                || initial_web.authentication.jwt != web.authentication.jwt;
            self.state.update(|state| {
                state.pending_reconnect |=
                    reconnect || (state.server.is_connected && credentials_changed);
                state.pending_restart = restart;
            });
            if self.hubs.application.connection_count() > 0 {
                self.hubs
                    .application
                    .broadcast(hub_methods::OPTIONS, vec![json!(current.redact())]);
            }
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

fn compile_search_filters(options: &Options) -> Vec<regex::Regex> {
    options
        .filters
        .search
        .request
        .iter()
        .filter_map(|pattern| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(!options.flags.case_sensitive_reg_ex)
                .build()
                .map_err(|error| tracing::warn!(%error, "ignoring invalid search request filter"))
                .ok()
        })
        .collect()
}

fn event_header(kind: EventType) -> EventHeader {
    EventHeader {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        kind,
        version: 1,
    }
}

fn credentials(options: &Options) -> Option<(&str, &str)> {
    let username = options
        .soulseek
        .username
        .as_deref()
        .filter(|value| !value.trim().is_empty())?;
    let password = options
        .soulseek
        .password
        .as_deref()
        .filter(|value| !value.trim().is_empty())?;
    Some((username, password))
}
