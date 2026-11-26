//! Client, connection, search, browse and transfer options (Soulseek.NET `Options/*`).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::error::{Error, Result};
use crate::model::{File, Search, SearchResponse, SearchState, Transfer, TransferState};

/// Severity of diagnostic messages; a message is emitted when its level is at
/// most [`ClientOptions::minimum_diagnostic_level`] (`None` disables all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum DiagnosticLevel {
    None = 0,
    Warning = 1,
    #[default]
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl DiagnosticLevel {
    /// Name as rendered by .NET.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Warning => "Warning",
            Self::Info => "Info",
            Self::Debug => "Debug",
            Self::Trace => "Trace",
        }
    }

    /// Case-insensitive parse of the .NET name.
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::None,
            Self::Warning,
            Self::Info,
            Self::Debug,
            Self::Trace,
        ]
        .into_iter()
        .find(|l| l.as_str().eq_ignore_ascii_case(s.trim()))
    }
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SOCKS5 proxy used for outgoing connections.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyOptions {
    /// Host name or IP address, resolved at connect time.
    pub address: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxyOptions {
    /// Validates and builds proxy options; credentials are 1–255 bytes each.
    pub fn new(
        address: impl Into<String>,
        port: u16,
        credentials: Option<(String, String)>,
    ) -> Result<Self> {
        let address = address.into();
        if address.trim().is_empty() {
            return Err(Error::Argument(
                "Address must not be a null or empty string, or one consisting only of whitespace"
                    .into(),
            ));
        }
        if port == 0 {
            return Err(Error::Argument(
                "The port must be within the range 1-65535".into(),
            ));
        }
        let (username, password) = match credentials {
            Some((username, password)) => {
                if username.is_empty() || username.len() > 255 {
                    return Err(Error::Argument(
                        "The username length must be greater than or equal to 1 and less than or equal to 255"
                            .into(),
                    ));
                }
                if password.is_empty() || password.len() > 255 {
                    return Err(Error::Argument(
                        "The password length must be greater than or equal to 1 and less than or equal to 255"
                            .into(),
                    ));
                }
                (Some(username), Some(password))
            }
            None => (None, None),
        };
        Ok(Self {
            address,
            port,
            username,
            password,
        })
    }
}

impl fmt::Debug for ProxyOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyOptions")
            .field("address", &self.address)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Options of a TCP connection (Soulseek.NET `ConnectionOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOptions {
    /// Read buffer size in bytes.
    pub read_buffer_size: usize,
    /// Write buffer size in bytes.
    pub write_buffer_size: usize,
    /// Maximum number of queued outgoing messages before writes are dropped.
    pub write_queue_size: usize,
    /// Timeout for establishing the connection.
    pub connect_timeout: Duration,
    /// Disconnect after this long without any read or write; `None` disables.
    pub inactivity_timeout: Option<Duration>,
    /// Proxy for outgoing connections.
    pub proxy: Option<ProxyOptions>,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            read_buffer_size: 16384,
            write_buffer_size: 16384,
            write_queue_size: 250,
            connect_timeout: Duration::from_millis(10000),
            inactivity_timeout: Some(Duration::from_millis(15000)),
            proxy: None,
        }
    }
}

impl ConnectionOptions {
    /// Copy with the inactivity timeout disabled (used for the server connection).
    pub fn without_inactivity_timeout(mut self) -> Self {
        self.inactivity_timeout = None;
        self
    }
}

/// Client-wide options (Soulseek.NET `SoulseekClientOptions`).
///
/// Incoming-request resolvers are supplied separately via [`crate::Resolvers`].
#[derive(Debug, Clone, PartialEq)]
pub struct ClientOptions {
    /// Minor client version sent at login.
    pub minor_version: u32,
    /// Whether to accept incoming connections.
    pub enable_listener: bool,
    /// Address the listener binds to.
    pub listen_ip_address: IpAddr,
    /// Port the listener binds to and that is advertised to the server; `None`
    /// means no port is configured and the listener is not started.
    pub listen_port: Option<u16>,
    /// Listen backlog.
    pub listen_backlog: u32,
    /// Whether to participate in the distributed network.
    pub enable_distributed_network: bool,
    /// Whether to accept distributed child connections.
    pub accept_distributed_children: bool,
    /// Maximum number of distributed children.
    pub distributed_child_limit: u32,
    /// Maximum number of concurrently executing searches.
    pub maximum_concurrent_searches: usize,
    /// Maximum number of concurrent uploads.
    pub maximum_concurrent_uploads: usize,
    /// Total upload speed limit in KiB/s; 0 = unlimited.
    pub maximum_upload_speed: u32,
    /// Maximum number of concurrent downloads.
    pub maximum_concurrent_downloads: usize,
    /// Total download speed limit in KiB/s; 0 = unlimited.
    pub maximum_download_speed: u32,
    /// Whether duplicate incoming search requests are dropped.
    pub deduplicate_search_requests: bool,
    /// Timeout for server and peer responses.
    pub message_timeout: Duration,
    /// Whether received private messages are acknowledged automatically.
    pub auto_acknowledge_private_messages: bool,
    /// Whether privilege notifications are acknowledged automatically.
    pub auto_acknowledge_privilege_notifications: bool,
    /// Whether private room invitations are accepted.
    pub accept_private_room_invitations: bool,
    /// Most verbose diagnostic level emitted as [`crate::Event::DiagnosticGenerated`].
    pub minimum_diagnostic_level: DiagnosticLevel,
    /// `None` (the default): [`crate::SoulseekClient::get_next_token`] returns random
    /// tokens. `Some(n)`: tokens count up from `n` (deterministic tests only). Tokens
    /// authenticate connections a peer makes back to us (`PierceFirewall`), and search
    /// tokens are visible to the whole distributed network, so counting up would let
    /// anyone predict the next solicitation and connect in the real peer's place.
    pub starting_token: Option<u32>,
    /// Server connection (inactivity timeout is always disabled).
    pub server_connection_options: ConnectionOptions,
    /// Outgoing peer message connections.
    pub peer_connection_options: ConnectionOptions,
    /// Transfer connections.
    pub transfer_connection_options: ConnectionOptions,
    /// Incoming connections accepted by the listener.
    pub incoming_connection_options: ConnectionOptions,
    /// Distributed parent/child connections.
    pub distributed_connection_options: ConnectionOptions,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            // Identifies slskr at login; the major version remains the protocol version.
            minor_version: 8_291_371,
            enable_listener: true,
            listen_ip_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            listen_port: Some(50000),
            listen_backlog: 1024,
            enable_distributed_network: true,
            accept_distributed_children: true,
            distributed_child_limit: 25,
            maximum_concurrent_searches: 2,
            maximum_concurrent_uploads: 10,
            maximum_upload_speed: 0,
            maximum_concurrent_downloads: usize::MAX,
            maximum_download_speed: 0,
            deduplicate_search_requests: true,
            message_timeout: Duration::from_millis(5000),
            auto_acknowledge_private_messages: true,
            auto_acknowledge_privilege_notifications: true,
            accept_private_room_invitations: false,
            minimum_diagnostic_level: DiagnosticLevel::Info,
            starting_token: None,
            server_connection_options: ConnectionOptions::default().without_inactivity_timeout(),
            peer_connection_options: ConnectionOptions::default(),
            transfer_connection_options: ConnectionOptions::default(),
            incoming_connection_options: ConnectionOptions::default(),
            distributed_connection_options: ConnectionOptions::default(),
        }
    }
}

impl ClientOptions {
    /// Validates value ranges (as the Soulseek.NET constructor does).
    pub fn validate(&self) -> Result<()> {
        if let Some(port) = self.listen_port
            && port < 1024
        {
            return Err(Error::Argument("Must be between 1024 and 65535".into()));
        }
        if self.listen_backlog < 1 {
            return Err(Error::Argument(
                "The listen backlog must be at least 1".into(),
            ));
        }
        if self.maximum_concurrent_searches < 1 {
            return Err(Error::Argument(
                "Must be greater than or equal to one".into(),
            ));
        }
        if self.maximum_concurrent_uploads < 1 {
            return Err(Error::Argument(
                "Must be greater than or equal to one".into(),
            ));
        }
        if self.maximum_concurrent_downloads < 1 {
            return Err(Error::Argument(
                "Must be greater than or equal to one".into(),
            ));
        }
        Ok(())
    }

    /// Returns a copy with every `Some` field of `patch` applied; the server
    /// connection options never get an inactivity timeout.
    pub fn with_patch(&self, patch: &ClientOptionsPatch) -> Self {
        let mut o = self.clone();
        macro_rules! apply {
            ($($field:ident),* $(,)?) => {
                $(if let Some(v) = &patch.$field { o.$field = Clone::clone(v); })*
            };
        }
        apply!(
            enable_listener,
            listen_ip_address,
            listen_backlog,
            enable_distributed_network,
            accept_distributed_children,
            distributed_child_limit,
            maximum_upload_speed,
            maximum_download_speed,
            deduplicate_search_requests,
            auto_acknowledge_private_messages,
            auto_acknowledge_privilege_notifications,
            accept_private_room_invitations,
            peer_connection_options,
            transfer_connection_options,
            incoming_connection_options,
            distributed_connection_options,
        );
        if let Some(port) = patch.listen_port {
            o.listen_port = Some(port);
        }
        if let Some(server) = &patch.server_connection_options {
            o.server_connection_options = server.clone().without_inactivity_timeout();
        }
        o
    }

    /// Whether applying `patch` to these options requires a server reconnect
    /// while logged in: distributed network or children being disabled, or the
    /// server/distributed connection options changing.
    pub fn reconnect_required_for(&self, patch: &ClientOptionsPatch) -> bool {
        let distributed_disabled =
            self.enable_distributed_network && patch.enable_distributed_network == Some(false);
        let children_disabled =
            self.accept_distributed_children && patch.accept_distributed_children == Some(false);
        let distributed_options_changed = patch
            .distributed_connection_options
            .as_ref()
            .is_some_and(|o| *o != self.distributed_connection_options);
        let server_options_changed = patch.server_connection_options.as_ref().is_some_and(|o| {
            o.clone().without_inactivity_timeout() != self.server_connection_options
        });
        distributed_disabled
            || children_disabled
            || distributed_options_changed
            || server_options_changed
    }

    /// Whether applying `patch` changes listener settings (requires a listener restart).
    pub fn listener_changed_by(&self, patch: &ClientOptionsPatch) -> bool {
        patch
            .enable_listener
            .is_some_and(|v| v != self.enable_listener)
            || patch
                .listen_ip_address
                .is_some_and(|v| v != self.listen_ip_address)
            || patch
                .listen_port
                .is_some_and(|v| Some(v) != self.listen_port)
            || patch
                .listen_backlog
                .is_some_and(|v| v != self.listen_backlog)
            || patch
                .incoming_connection_options
                .as_ref()
                .is_some_and(|o| *o != self.incoming_connection_options)
    }
}

/// Runtime-changeable subset of [`ClientOptions`] (Soulseek.NET `SoulseekClientOptionsPatch`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientOptionsPatch {
    pub enable_listener: Option<bool>,
    pub listen_ip_address: Option<IpAddr>,
    pub listen_port: Option<u16>,
    pub listen_backlog: Option<u32>,
    pub enable_distributed_network: Option<bool>,
    pub accept_distributed_children: Option<bool>,
    pub distributed_child_limit: Option<u32>,
    /// KiB/s; 0 = unlimited.
    pub maximum_upload_speed: Option<u32>,
    /// KiB/s; 0 = unlimited.
    pub maximum_download_speed: Option<u32>,
    pub deduplicate_search_requests: Option<bool>,
    pub auto_acknowledge_private_messages: Option<bool>,
    pub auto_acknowledge_privilege_notifications: Option<bool>,
    pub accept_private_room_invitations: Option<bool>,
    pub server_connection_options: Option<ConnectionOptions>,
    pub peer_connection_options: Option<ConnectionOptions>,
    pub transfer_connection_options: Option<ConnectionOptions>,
    pub incoming_connection_options: Option<ConnectionOptions>,
    pub distributed_connection_options: Option<ConnectionOptions>,
}

/// Predicate applied to search responses.
pub type SearchResponseFilter = Arc<dyn Fn(&SearchResponse) -> bool + Send + Sync>;
/// Predicate applied to files within search responses.
pub type SearchFileFilter = Arc<dyn Fn(&File) -> bool + Send + Sync>;
/// Callback invoked with `(previous state, search)` when a search changes state.
pub type SearchStateChangedCallback = Arc<dyn Fn(SearchState, &Search) + Send + Sync>;

/// Options of a single search (Soulseek.NET `SearchOptions`).
#[derive(Clone)]
pub struct SearchOptions {
    /// Search completes after this long without a new response.
    pub search_timeout: Duration,
    /// Search completes after this many accepted responses.
    pub response_limit: usize,
    /// Search completes after this many accepted files.
    pub file_limit: usize,
    /// Whether the filters below are applied.
    pub filter_responses: bool,
    /// Minimum number of (filtered) files in an accepted response.
    pub minimum_response_file_count: usize,
    /// Minimum upload speed (bytes/s) of an accepted response.
    pub minimum_peer_upload_speed: u32,
    /// Maximum queue length of an accepted response.
    pub maximum_peer_queue_length: u64,
    /// Whether single-character terms are removed from the query.
    pub remove_single_character_search_terms: bool,
    /// Extra response predicate.
    pub response_filter: Option<SearchResponseFilter>,
    /// Extra file predicate.
    pub file_filter: Option<SearchFileFilter>,
    /// Per-search state change callback (also raised as [`crate::Event::SearchStateChanged`]).
    pub state_changed: Option<SearchStateChangedCallback>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            search_timeout: Duration::from_millis(15000),
            response_limit: 250,
            file_limit: 25000,
            filter_responses: true,
            minimum_response_file_count: 1,
            minimum_peer_upload_speed: 0,
            maximum_peer_queue_length: i32::MAX as u64,
            remove_single_character_search_terms: true,
            response_filter: None,
            file_filter: None,
            state_changed: None,
        }
    }
}

impl fmt::Debug for SearchOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchOptions")
            .field("search_timeout", &self.search_timeout)
            .field("response_limit", &self.response_limit)
            .field("file_limit", &self.file_limit)
            .field("filter_responses", &self.filter_responses)
            .field(
                "minimum_response_file_count",
                &self.minimum_response_file_count,
            )
            .field("minimum_peer_upload_speed", &self.minimum_peer_upload_speed)
            .field("maximum_peer_queue_length", &self.maximum_peer_queue_length)
            .field(
                "remove_single_character_search_terms",
                &self.remove_single_character_search_terms,
            )
            .field(
                "response_filter",
                &self.response_filter.as_ref().map(|_| "<fn>"),
            )
            .field("file_filter", &self.file_filter.as_ref().map(|_| "<fn>"))
            .field(
                "state_changed",
                &self.state_changed.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
}

/// Options of a browse request (Soulseek.NET `BrowseOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseOptions {
    /// Maximum wait for the peer to start sending the response.
    pub response_timeout: Duration,
}

impl Default for BrowseOptions {
    fn default() -> Self {
        Self {
            response_timeout: Duration::from_millis(60000),
        }
    }
}

/// Asked for permission to transfer `bytes`; resolves to the number of bytes granted.
pub type TransferGovernor =
    Arc<dyn Fn(&Transfer, usize) -> BoxFuture<'static, usize> + Send + Sync>;
/// Resolves when an upload slot is available; an error fails the upload.
pub type SlotAwaiter = Arc<dyn Fn(&Transfer) -> BoxFuture<'static, Result<()>> + Send + Sync>;
/// Invoked once when an upload slot acquired via the [`SlotAwaiter`] is released.
pub type SlotReleased = Arc<dyn Fn(&Transfer) + Send + Sync>;
/// Invoked with `(previous state, transfer)` on each state change.
pub type TransferStateChangedCallback = Arc<dyn Fn(TransferState, &Transfer) + Send + Sync>;
/// Invoked with `(previous bytes transferred, transfer)` on progress.
pub type TransferProgressCallback = Arc<dyn Fn(u64, &Transfer) + Send + Sync>;
/// Invoked after each chunk with `(transfer, attempted, granted, actual)` byte counts.
pub type TransferReporter = Arc<dyn Fn(&Transfer, usize, usize, usize) + Send + Sync>;
/// Given the transfer and the remote size, returns the size to use, or `None` to
/// abort with [`Error::TransferSizeMismatch`].
pub type SizeMismatchResolver = Arc<dyn Fn(&Transfer, u64) -> Option<u64> + Send + Sync>;

/// Options of a single transfer (Soulseek.NET `TransferOptions`).
#[derive(Clone)]
pub struct TransferOptions {
    /// After an upload completes, how long to wait for the downloader to close the connection.
    pub maximum_linger_time: Duration,
    /// Rate governor; `None` grants every request in full.
    pub governor: Option<TransferGovernor>,
    /// Upload slot awaiter; `None` means a slot is always available.
    pub slot_awaiter: Option<SlotAwaiter>,
    /// Upload slot release callback.
    pub slot_released: Option<SlotReleased>,
    /// Per-transfer state change callback (also raised as [`crate::Event::TransferStateChanged`]).
    pub state_changed: Option<TransferStateChangedCallback>,
    /// Per-transfer progress callback (also raised as [`crate::Event::TransferProgressUpdated`]).
    pub progress_updated: Option<TransferProgressCallback>,
    /// Throughput reporter.
    pub reporter: Option<TransferReporter>,
    /// Size mismatch policy. Without a resolver a mismatch aborts the download,
    /// except when the remote size is 0 and the local size is ≥ `i32::MAX`
    /// (remote 32-bit overflow), in which case the local size is kept.
    pub size_mismatch_resolver: Option<SizeMismatchResolver>,
    /// Seek the upload input to `start_offset` before sending.
    pub seek_input_stream_automatically: bool,
    /// Seek the download output to `start_offset` before writing.
    pub seek_output_stream_automatically: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            maximum_linger_time: Duration::from_millis(3000),
            governor: None,
            slot_awaiter: None,
            slot_released: None,
            state_changed: None,
            progress_updated: None,
            reporter: None,
            size_mismatch_resolver: None,
            seek_input_stream_automatically: true,
            seek_output_stream_automatically: true,
        }
    }
}

impl fmt::Debug for TransferOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn cb<T>(o: &Option<T>) -> Option<&'static str> {
            o.as_ref().map(|_| "<fn>")
        }
        f.debug_struct("TransferOptions")
            .field("maximum_linger_time", &self.maximum_linger_time)
            .field("governor", &cb(&self.governor))
            .field("slot_awaiter", &cb(&self.slot_awaiter))
            .field("slot_released", &cb(&self.slot_released))
            .field("state_changed", &cb(&self.state_changed))
            .field("progress_updated", &cb(&self.progress_updated))
            .field("reporter", &cb(&self.reporter))
            .field("size_mismatch_resolver", &cb(&self.size_mismatch_resolver))
            .field(
                "seek_input_stream_automatically",
                &self.seek_input_stream_automatically,
            )
            .field(
                "seek_output_stream_automatically",
                &self.seek_output_stream_automatically,
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_defaults_match_soulseek_net() {
        let o = ClientOptions::default();
        assert_eq!(o.listen_port, Some(50000));
        assert_eq!(o.server_connection_options.inactivity_timeout, None);
        assert_eq!(
            o.peer_connection_options.inactivity_timeout,
            Some(Duration::from_secs(15))
        );
        assert!(o.validate().is_ok());
    }

    #[test]
    fn patch_applies_and_detects_reconnect() {
        let o = ClientOptions::default();
        let patch = ClientOptionsPatch {
            listen_port: Some(50001),
            enable_distributed_network: Some(false),
            server_connection_options: Some(ConnectionOptions::default()),
            ..Default::default()
        };
        let n = o.with_patch(&patch);
        assert_eq!(n.listen_port, Some(50001));
        assert!(!n.enable_distributed_network);
        assert_eq!(n.server_connection_options.inactivity_timeout, None);
        assert!(o.reconnect_required_for(&patch));
        assert!(o.listener_changed_by(&patch));
        assert!(!o.reconnect_required_for(&ClientOptionsPatch::default()));
    }

    #[test]
    fn proxy_validation() {
        assert!(ProxyOptions::new(" ", 1080, None).is_err());
        assert!(ProxyOptions::new("h", 1080, Some((String::new(), "p".into()))).is_err());
        let p = ProxyOptions::new("h", 1080, Some(("u".into(), "p".into()))).unwrap();
        assert!(!format!("{p:?}").contains("\"p\""));
    }
}
