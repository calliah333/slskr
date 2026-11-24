//! [`SoulseekClient`]: the public facade of the Soulseek client.
//!
//! # Conventions (apply to every method unless stated otherwise)
//!
//! * String arguments (usernames, room names, filenames, messages, interests)
//!   must not be empty or whitespace-only → [`Error::Argument`].
//! * Server and peer operations require the client to be `CONNECTED | LOGGED_IN`
//!   → otherwise [`Error::InvalidState`] (message includes the current state,
//!   e.g. `"The server connection must be connected and logged in to ... (currently: Disconnected)"`).
//! * Request/response operations wait at most [`ClientOptions::message_timeout`]
//!   for the response → [`Error::Timeout`].
//! * Operations without a [`CancellationToken`] are cancelled by dropping the
//!   future. Long-running operations (searches, browse, peer requests,
//!   transfers) take a token; cancellation yields [`Error::Cancelled`].
//! * Peer operations resolve the user's endpoint through the server first; an
//!   offline user (endpoint `0.0.0.0:0`) → [`Error::UserOffline`]
//!   (`"User {username} appears to be offline"`). Peer connections are
//!   attempted directly and via firewall piercing (ConnectToPeer) concurrently.
//! * Unexpected failures are wrapped in [`Error::Other`] with the operation in the
//!   message (e.g. `"Failed to connect: ..."`); `Timeout`, `Cancelled`,
//!   `UserOffline` and the documented specific errors pass through unchanged.

mod distributed;
mod peer_handler;
mod peers;
mod search;
mod server;
mod server_ops;
mod transfers;

use std::collections::HashMap;
use std::fmt;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::events::Event;
use crate::model::{
    ClientState, DistributedNetworkInfo, Recommendation, Search, SearchQuery, SearchResponse,
    SearchScope, ServerInfo, Transfer,
};
use crate::net::rate::RateLimiter;
use crate::net::waiter::Waiter;
use crate::options::{ClientOptions, SearchOptions, TransferOptions};
use crate::resolvers::Resolvers;

/// Default server address.
pub const DEFAULT_ADDRESS: &str = "server.slsknet.org";
/// Default server port.
pub const DEFAULT_PORT: u16 = 2271;
/// Major client version sent at login.
pub const MAJOR_VERSION: u32 = 170;
/// Capacity of the event broadcast channel; slower subscribers observe
/// [`broadcast::error::RecvError::Lagged`].
pub const EVENT_CHANNEL_CAPACITY: usize = 16384;

/// Readable, seekable upload source.
pub trait TransferInput: AsyncRead + AsyncSeek + Send + Unpin {}
impl<T: AsyncRead + AsyncSeek + Send + Unpin> TransferInput for T {}

/// Writable download destination.
pub trait TransferOutput: AsyncWrite + AsyncSeek + Send + Unpin {}
impl<T: AsyncWrite + AsyncSeek + Send + Unpin> TransferOutput for T {}

/// Creates the download destination; invoked once, when data is about to arrive.
pub type OutputFactory =
    Box<dyn FnOnce() -> BoxFuture<'static, Result<Box<dyn TransferOutput>>> + Send>;
/// Creates the upload source given the start offset requested by the peer;
/// invoked once, when the transfer starts. The source is seeked to the offset
/// when [`TransferOptions::seek_input_stream_automatically`] is set.
pub type InputFactory =
    Box<dyn FnOnce(u64) -> BoxFuture<'static, Result<Box<dyn TransferInput>>> + Send>;

/// Destination of a download.
pub enum DownloadOutput {
    /// Local file: created or truncated when `start_offset` is zero. Resuming
    /// preserves the prefix, writes from the requested offset and truncates
    /// any stale tail on success.
    File(PathBuf),
    /// Caller-provided writer, seeked to `start_offset` when
    /// [`TransferOptions::seek_output_stream_automatically`]; flushed and dropped on completion.
    Stream(OutputFactory),
}

impl fmt::Debug for DownloadOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Stream(_) => f.write_str("Stream(<factory>)"),
        }
    }
}

/// Source of an upload.
pub enum UploadInput {
    /// Local file; the size is taken from its metadata (missing file → [`Error::Io`]).
    File(PathBuf),
    /// Caller-provided reader of `size` bytes.
    Stream { size: u64, factory: InputFactory },
}

impl fmt::Debug for UploadInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Stream { size, .. } => f
                .debug_struct("Stream")
                .field("size", size)
                .finish_non_exhaustive(),
        }
    }
}

/// An enqueued transfer; resolves to the final transfer snapshot.
///
/// Dropping the handle does not cancel the transfer; use the
/// [`CancellationToken`] passed when enqueueing.
#[derive(Debug)]
pub struct TransferHandle {
    token: u32,
    completion: oneshot::Receiver<Result<Transfer>>,
}

impl TransferHandle {
    /// Handle for the transfer with `token` whose task sends its outcome on `completion`.
    pub(crate) fn new(token: u32, completion: oneshot::Receiver<Result<Transfer>>) -> Self {
        Self { token, completion }
    }

    /// Local token of the transfer.
    pub fn token(&self) -> u32 {
        self.token
    }

    /// Waits for the transfer to finish. `Ok` carries a snapshot in state
    /// `COMPLETED | SUCCEEDED`; failures carry the error that ended it.
    pub async fn wait(self) -> Result<Transfer> {
        self.completion.await.unwrap_or_else(|_| {
            Err(Error::Transfer(format!(
                "Transfer {} ended without reporting a result",
                self.token
            )))
        })
    }
}

impl IntoFuture for TransferHandle {
    type Output = Result<Transfer>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<Transfer>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait())
    }
}

/// Recommended and not recommended interests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recommendations {
    pub recommended: Vec<Recommendation>,
    pub not_recommended: Vec<Recommendation>,
}

/// Recommendations related to one interest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterestRecommendations {
    pub interest: String,
    pub recommended: Vec<Recommendation>,
    pub not_recommended: Vec<Recommendation>,
}

/// Users sharing an interest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterestSimilarUsers {
    pub interest: String,
    pub usernames: Vec<String>,
}

/// A user with interests similar to ours and the similarity rating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarUser {
    pub username: String,
    pub rating: i32,
}

/// Server session state exposed through the client accessors.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) state: ClientState,
    /// Unresolved server address of the current/last connection.
    pub(crate) address: Option<String>,
    /// Resolved server endpoint of the current/last connection.
    pub(crate) ip_endpoint: Option<SocketAddr>,
    /// Logged in username.
    pub(crate) username: Option<String>,
    pub(crate) server_info: ServerInfo,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: ClientState::DISCONNECTED,
            address: None,
            ip_endpoint: None,
            username: None,
            server_info: ServerInfo::default(),
        }
    }
}

/// Shared client state and runtime managers.
pub(crate) struct ClientInner {
    pub(crate) options: RwLock<ClientOptions>,
    pub(crate) resolvers: Arc<dyn Resolvers>,
    pub(crate) events: broadcast::Sender<Event>,
    pub(crate) session: RwLock<Session>,
    pub(crate) next_token: AtomicU32,
    pub(crate) waiter: Waiter,
    pub(crate) server: server::ServerConnection,
    pub(crate) peers: peers::PeerManager,
    pub(crate) transfers: transfers::TransferManager,
    pub(crate) searches: search::SearchManager,
    pub(crate) distributed: distributed::DistributedManager,
    pub(crate) upload_limiter: RateLimiter,
    pub(crate) download_limiter: RateLimiter,
    /// Snapshots of active and queued downloads by token, updated on every change.
    pub(crate) downloads: RwLock<HashMap<u32, Transfer>>,
    /// Snapshots of active and queued uploads by token, updated on every change.
    pub(crate) uploads: RwLock<HashMap<u32, Transfer>>,
}

impl ClientInner {
    /// Sends a framed message on the logged-in server connection.
    pub(crate) async fn send_server(&self, frame: Bytes) -> Result<()> {
        self.server.logged_in_transport(self)?.send(frame).await
    }

    /// Broadcasts `event`; having no subscribers is not an error.
    pub(crate) fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Current client state.
    pub(crate) fn state(&self) -> ClientState {
        self.session.read().unwrap_or_else(|e| e.into_inner()).state
    }

    /// Sets the client state and raises [`Event::StateChanged`] when it changed.
    pub(crate) fn set_state(
        &self,
        state: ClientState,
        message: Option<String>,
        error: Option<Arc<Error>>,
    ) {
        let previous = {
            let mut session = self.session.write().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut session.state, state)
        };
        if previous != state {
            self.emit(Event::StateChanged {
                previous,
                state,
                message,
                error,
            });
        }
    }

    /// Errors with [`Error::InvalidState`] unless `CONNECTED | LOGGED_IN`;
    /// `action` completes "... must be connected and logged in to {action}".
    pub(crate) fn ensure_logged_in(&self, action: &str) -> Result<()> {
        let state = self.state();
        if state.is_logged_in() {
            Ok(())
        } else {
            Err(Error::InvalidState(format!(
                "The server connection must be connected and logged in to {action} (currently: {state})"
            )))
        }
    }
}

/// Errors with [`Error::Argument`] when `value` is empty or whitespace-only.
pub(crate) fn require_non_blank(value: &str, name: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(Error::Argument(format!(
            "The {name} must not be a null or empty string, or one consisting of only whitespace"
        )))
    } else {
        Ok(())
    }
}

/// Async Soulseek client. Cheap to clone; clones share the same session.
#[derive(Clone)]
pub struct SoulseekClient {
    pub(crate) inner: Arc<ClientInner>,
}

impl fmt::Debug for SoulseekClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session = self.inner.session.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("SoulseekClient")
            .field("state", &session.state)
            .field("username", &session.username)
            .field("ip_endpoint", &session.ip_endpoint)
            .finish_non_exhaustive()
    }
}

impl SoulseekClient {
    // ----------------------------------------------------------------------------
    // Construction and properties
    // ----------------------------------------------------------------------------

    /// Creates a disconnected client. Nothing is started until [`Self::connect`].
    pub fn new(options: ClientOptions, resolvers: Arc<dyn Resolvers>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let next_token = AtomicU32::new(options.starting_token);
        let upload_limiter = RateLimiter::new(u64::from(options.maximum_upload_speed) * 1024);
        let download_limiter = RateLimiter::new(u64::from(options.maximum_download_speed) * 1024);
        Self {
            inner: Arc::new(ClientInner {
                options: RwLock::new(options),
                resolvers,
                events,
                session: RwLock::new(Session::default()),
                next_token,
                waiter: Waiter::new(),
                server: server::ServerConnection::default(),
                peers: peers::PeerManager::new(),
                transfers: transfers::TransferManager::new(),
                searches: search::SearchManager::new(),
                distributed: distributed::DistributedManager::new(),
                upload_limiter,
                download_limiter,
                downloads: RwLock::new(HashMap::new()),
                uploads: RwLock::new(HashMap::new()),
            }),
        }
    }

    pub(crate) fn inner(&self) -> &Arc<ClientInner> {
        &self.inner
    }

    pub(crate) fn from_inner(inner: Arc<ClientInner>) -> Self {
        Self { inner }
    }

    fn read_session<T>(&self, f: impl FnOnce(&Session) -> T) -> T {
        f(&self.inner.session.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Subscribes to client events. Events raised before subscribing are not delivered.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Current client state.
    pub fn state(&self) -> ClientState {
        self.inner.state()
    }

    /// Logged in username (kept after disconnect until the next login).
    pub fn username(&self) -> Option<String> {
        self.read_session(|s| s.username.clone())
    }

    /// Unresolved server address of the current/last connection.
    pub fn address(&self) -> Option<String> {
        self.read_session(|s| s.address.clone())
    }

    /// Resolved server endpoint of the current/last connection.
    pub fn ip_endpoint(&self) -> Option<SocketAddr> {
        self.read_session(|s| s.ip_endpoint)
    }

    /// Server port of the current/last connection.
    pub fn port(&self) -> Option<u16> {
        self.read_session(|s| s.ip_endpoint.map(|e| e.port()))
    }

    /// Information sent by the server upon login.
    pub fn server_info(&self) -> ServerInfo {
        self.read_session(|s| s.server_info.clone())
    }

    /// Snapshot of the distributed network state.
    pub fn distributed_network(&self) -> DistributedNetworkInfo {
        self.inner.distributed.info()
    }

    /// Current options.
    pub fn options(&self) -> ClientOptions {
        self.inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Resolvers answering incoming peer requests.
    pub fn resolvers(&self) -> Arc<dyn Resolvers> {
        self.inner.resolvers.clone()
    }

    /// Major client version ([`MAJOR_VERSION`]).
    pub fn major_version(&self) -> u32 {
        MAJOR_VERSION
    }

    /// Minor client version ([`ClientOptions::minor_version`]).
    pub fn minor_version(&self) -> u32 {
        self.inner
            .options
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .minor_version
    }

    /// Snapshot of active and queued downloads.
    pub fn downloads(&self) -> Vec<Transfer> {
        self.inner
            .downloads
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Snapshot of active and queued uploads.
    pub fn uploads(&self) -> Vec<Transfer> {
        self.inner
            .uploads
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Next token for client operations, starting at
    /// [`ClientOptions::starting_token`] and wrapping to 0 after `i32::MAX`.
    pub fn get_next_token(&self) -> u32 {
        let counter = &self.inner.next_token;
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = if current >= i32::MAX as u32 {
                0
            } else {
                current + 1
            };
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(token) => return token,
                Err(actual) => current = actual,
            }
        }
    }

    /// [`Self::connect`] to [`DEFAULT_ADDRESS`]:[`DEFAULT_PORT`].
    pub async fn connect_default(&self, username: &str, password: &str) -> Result<()> {
        self.connect(DEFAULT_ADDRESS, DEFAULT_PORT, username, password)
            .await
    }

    /// [`Self::search`], collecting the accepted responses.
    pub async fn search_collect(
        &self,
        query: SearchQuery,
        scope: SearchScope,
        token: Option<u32>,
        options: SearchOptions,
        cancel: CancellationToken,
    ) -> Result<(Search, Vec<SearchResponse>)> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let search = self
            .search(query, scope, token, options, tx, cancel)
            .await?;
        let mut responses = Vec::new();
        while let Ok(response) = rx.try_recv() {
            responses.push(response);
        }
        Ok((search, responses))
    }

    /// [`Self::enqueue_download`] and wait for completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn download(
        &self,
        username: &str,
        filename: &str,
        output: DownloadOutput,
        size: Option<u64>,
        start_offset: u64,
        token: Option<u32>,
        options: TransferOptions,
        cancel: CancellationToken,
    ) -> Result<Transfer> {
        self.enqueue_download(
            username,
            filename,
            output,
            size,
            start_offset,
            token,
            options,
            cancel,
        )
        .await?
        .wait()
        .await
    }

    /// [`Self::enqueue_upload`] and wait for completion.
    pub async fn upload(
        &self,
        username: &str,
        filename: &str,
        input: UploadInput,
        token: Option<u32>,
        options: TransferOptions,
        cancel: CancellationToken,
    ) -> Result<Transfer> {
        self.enqueue_upload(username, filename, input, token, options, cancel)
            .await?
            .wait()
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolvers::DefaultResolvers;

    fn client(starting_token: u32) -> SoulseekClient {
        let options = ClientOptions {
            starting_token,
            ..Default::default()
        };
        SoulseekClient::new(options, Arc::new(DefaultResolvers))
    }

    #[test]
    fn initial_state() {
        let c = client(0);
        assert_eq!(c.state(), ClientState::DISCONNECTED);
        assert_eq!(c.username(), None);
        assert!(c.downloads().is_empty());
        assert_eq!(c.major_version(), MAJOR_VERSION);
    }

    #[test]
    fn tokens_increment_and_wrap() {
        let c = client(5);
        assert_eq!(c.get_next_token(), 5);
        assert_eq!(c.get_next_token(), 6);
        let c = client(i32::MAX as u32);
        assert_eq!(c.get_next_token(), i32::MAX as u32);
        assert_eq!(c.get_next_token(), 0);
    }

    #[tokio::test]
    async fn state_changes_are_broadcast() {
        let c = client(0);
        let mut rx = c.subscribe();
        c.inner
            .set_state(ClientState::CONNECTING, Some("Connecting".into()), None);
        match rx.recv().await.unwrap() {
            Event::StateChanged {
                previous, state, ..
            } => {
                assert_eq!(previous, ClientState::DISCONNECTED);
                assert_eq!(state, ClientState::CONNECTING);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(c.inner.ensure_logged_in("search").is_err());
        assert!(require_non_blank(" ", "username").is_err());
    }

    #[tokio::test]
    async fn transfer_handle_reports_dropped_sender() {
        let (tx, rx) = oneshot::channel::<Result<Transfer>>();
        drop(tx);
        assert!(TransferHandle::new(1, rx).await.is_err());
    }
}
