//! Client events delivered over [`crate::SoulseekClient::subscribe`].

use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::Error;
use crate::model::{
    ClientState, DistributedNetworkInfo, PrivateMessage, PrivilegeNotification, RoomInfo, RoomList,
    RoomMessage, RoomTicker, Search, SearchResponse, SearchState, ServerInfo, Transfer,
    TransferState, UserData, UserStatistics, UserStatus,
};
use crate::options::DiagnosticLevel;

/// One variant per Soulseek.NET `ISoulseekClient` event (plus diagnostics).
#[derive(Clone, Debug)]
pub enum Event {
    /// Browse response data received.
    BrowseProgressUpdated {
        username: String,
        bytes_transferred: u64,
        size: u64,
    },
    /// The server connection was established.
    Connected,
    /// The client stopped being a distributed branch root.
    DemotedFromDistributedBranchRoot,
    /// The server connection was closed; `error` is the cause, if any
    /// (e.g. [`Error::KickedFromServer`]).
    Disconnected {
        message: String,
        error: Option<Arc<Error>>,
    },
    DistributedChildAdded {
        username: String,
        ip_endpoint: SocketAddr,
    },
    DistributedChildDisconnected {
        username: String,
        ip_endpoint: SocketAddr,
    },
    /// The server requested a distributed network reset.
    DistributedNetworkReset,
    DistributedNetworkStateChanged(DistributedNetworkInfo),
    DistributedParentAdopted {
        username: String,
        ip_endpoint: SocketAddr,
        branch_level: u32,
        branch_root: String,
    },
    DistributedParentDisconnected {
        username: String,
        ip_endpoint: SocketAddr,
        branch_level: u32,
        branch_root: String,
    },
    /// A peer denied a download (UploadDenied).
    DownloadDenied {
        username: String,
        filename: String,
        message: String,
    },
    /// A peer reported a download as failed (UploadFailed).
    DownloadFailed {
        username: String,
        filename: String,
    },
    ExcludedSearchPhrasesReceived(Vec<String>),
    GlobalMessageReceived(String),
    /// Another client logged in with the same username.
    KickedFromServer,
    LoggedIn,
    PrivateMessageReceived(PrivateMessage),
    PrivateRoomMembershipAdded(String),
    PrivateRoomMembershipRemoved(String),
    PrivateRoomModeratedUserListReceived(RoomInfo),
    PrivateRoomModerationAdded(String),
    PrivateRoomModerationRemoved(String),
    PrivateRoomUserListReceived(RoomInfo),
    PrivilegedUserListReceived(Vec<String>),
    PrivilegeNotificationReceived(PrivilegeNotification),
    PromotedToDistributedBranchRoot,
    PublicChatMessageReceived {
        room: String,
        username: String,
        message: String,
    },
    /// A user (possibly us) joined a room.
    RoomJoined {
        room: String,
        username: String,
        user_data: Option<UserData>,
    },
    RoomLeft {
        room: String,
        username: String,
    },
    RoomListReceived(RoomList),
    RoomMessageReceived(RoomMessage),
    RoomTickerAdded {
        room: String,
        ticker: RoomTicker,
    },
    RoomTickerListReceived {
        room: String,
        tickers: Vec<RoomTicker>,
    },
    RoomTickerRemoved {
        room: String,
        username: String,
    },
    /// A search request was received (from the server, a distributed parent or a peer).
    SearchRequestReceived {
        username: String,
        token: u32,
        query: String,
    },
    SearchResponseDelivered {
        username: String,
        token: u32,
        query: String,
        response: SearchResponse,
    },
    SearchResponseDeliveryFailed {
        username: String,
        token: u32,
        query: String,
        response: SearchResponse,
    },
    /// An accepted response to our search with token `token`.
    SearchResponseReceived {
        token: u32,
        response: SearchResponse,
    },
    SearchStateChanged {
        previous: SearchState,
        search: Search,
    },
    ServerInfoReceived(ServerInfo),
    StateChanged {
        previous: ClientState,
        state: ClientState,
        message: Option<String>,
        error: Option<Arc<Error>>,
    },
    TransferProgressUpdated {
        previous_bytes: u64,
        transfer: Transfer,
    },
    TransferStateChanged {
        previous: TransferState,
        transfer: Transfer,
    },
    /// The server reports that `username` could not connect to us.
    UserCannotConnect {
        username: String,
        token: u32,
    },
    UserStatisticsChanged(UserStatistics),
    /// Status of a watched user changed.
    UserStatusChanged(UserStatus),
    /// Diagnostic message at or below the configured minimum level.
    DiagnosticGenerated {
        level: DiagnosticLevel,
        message: String,
    },
}

impl Event {
    /// Variant name (the Soulseek.NET event name).
    pub fn name(&self) -> &'static str {
        match self {
            Event::BrowseProgressUpdated { .. } => "BrowseProgressUpdated",
            Event::Connected => "Connected",
            Event::DemotedFromDistributedBranchRoot => "DemotedFromDistributedBranchRoot",
            Event::Disconnected { .. } => "Disconnected",
            Event::DistributedChildAdded { .. } => "DistributedChildAdded",
            Event::DistributedChildDisconnected { .. } => "DistributedChildDisconnected",
            Event::DistributedNetworkReset => "DistributedNetworkReset",
            Event::DistributedNetworkStateChanged(_) => "DistributedNetworkStateChanged",
            Event::DistributedParentAdopted { .. } => "DistributedParentAdopted",
            Event::DistributedParentDisconnected { .. } => "DistributedParentDisconnected",
            Event::DownloadDenied { .. } => "DownloadDenied",
            Event::DownloadFailed { .. } => "DownloadFailed",
            Event::ExcludedSearchPhrasesReceived(_) => "ExcludedSearchPhrasesReceived",
            Event::GlobalMessageReceived(_) => "GlobalMessageReceived",
            Event::KickedFromServer => "KickedFromServer",
            Event::LoggedIn => "LoggedIn",
            Event::PrivateMessageReceived(_) => "PrivateMessageReceived",
            Event::PrivateRoomMembershipAdded(_) => "PrivateRoomMembershipAdded",
            Event::PrivateRoomMembershipRemoved(_) => "PrivateRoomMembershipRemoved",
            Event::PrivateRoomModeratedUserListReceived(_) => {
                "PrivateRoomModeratedUserListReceived"
            }
            Event::PrivateRoomModerationAdded(_) => "PrivateRoomModerationAdded",
            Event::PrivateRoomModerationRemoved(_) => "PrivateRoomModerationRemoved",
            Event::PrivateRoomUserListReceived(_) => "PrivateRoomUserListReceived",
            Event::PrivilegedUserListReceived(_) => "PrivilegedUserListReceived",
            Event::PrivilegeNotificationReceived(_) => "PrivilegeNotificationReceived",
            Event::PromotedToDistributedBranchRoot => "PromotedToDistributedBranchRoot",
            Event::PublicChatMessageReceived { .. } => "PublicChatMessageReceived",
            Event::RoomJoined { .. } => "RoomJoined",
            Event::RoomLeft { .. } => "RoomLeft",
            Event::RoomListReceived(_) => "RoomListReceived",
            Event::RoomMessageReceived(_) => "RoomMessageReceived",
            Event::RoomTickerAdded { .. } => "RoomTickerAdded",
            Event::RoomTickerListReceived { .. } => "RoomTickerListReceived",
            Event::RoomTickerRemoved { .. } => "RoomTickerRemoved",
            Event::SearchRequestReceived { .. } => "SearchRequestReceived",
            Event::SearchResponseDelivered { .. } => "SearchResponseDelivered",
            Event::SearchResponseDeliveryFailed { .. } => "SearchResponseDeliveryFailed",
            Event::SearchResponseReceived { .. } => "SearchResponseReceived",
            Event::SearchStateChanged { .. } => "SearchStateChanged",
            Event::ServerInfoReceived(_) => "ServerInfoReceived",
            Event::StateChanged { .. } => "StateChanged",
            Event::TransferProgressUpdated { .. } => "TransferProgressUpdated",
            Event::TransferStateChanged { .. } => "TransferStateChanged",
            Event::UserCannotConnect { .. } => "UserCannotConnect",
            Event::UserStatisticsChanged(_) => "UserStatisticsChanged",
            Event::UserStatusChanged(_) => "UserStatusChanged",
            Event::DiagnosticGenerated { .. } => "DiagnosticGenerated",
        }
    }
}
