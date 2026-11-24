//! Server messages (client <-> Soulseek server).

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use md5::{Digest, Md5};

use crate::model::{
    PrivateMessage, PrivilegeNotification, Recommendation, RoomData, RoomInfo, RoomList,
    RoomMessage, RoomTicker, UserData, UserInterests, UserPresence, UserStatistics, UserStatus,
};
use crate::protocol::codec::{MessageBuilder, MessageReader, ProtocolError};
use crate::protocol::codes::ServerCode;

/// Protocol major version sent at login.
pub const MAJOR_VERSION: u32 = 170;

/// Username of the server administrator for private messages.
pub const SERVER_USERNAME: &str = "server";

/// A primitive that can be written to / read from a message.
trait Field: Sized {
    fn put(&self, b: &mut MessageBuilder);
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError>;
}

impl Field for String {
    fn put(&self, b: &mut MessageBuilder) {
        b.string(self);
    }
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.string()
    }
}

impl Field for u32 {
    fn put(&self, b: &mut MessageBuilder) {
        b.u32(*self);
    }
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.u32()
    }
}

impl Field for i32 {
    fn put(&self, b: &mut MessageBuilder) {
        b.i32(*self);
    }
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        r.i32()
    }
}

impl Field for bool {
    fn put(&self, b: &mut MessageBuilder) {
        b.u8(u8::from(*self));
    }
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(r.u8()? > 0)
    }
}

impl Field for UserPresence {
    fn put(&self, b: &mut MessageBuilder) {
        b.u32(self.as_u32());
    }
    fn get(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(UserPresence::from_u32(r.u32()?))
    }
}

fn builder(code: ServerCode) -> MessageBuilder {
    let mut b = MessageBuilder::new();
    b.code_u32(code as u32);
    b
}

fn read_strings(r: &mut MessageReader) -> Result<Vec<String>, ProtocolError> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        v.push(r.string()?);
    }
    Ok(v)
}

fn write_strings(b: &mut MessageBuilder, v: &[String]) {
    b.u32(v.len() as u32);
    for s in v {
        b.string(s);
    }
}

fn read_recommendations(r: &mut MessageReader) -> Result<Vec<Recommendation>, ProtocolError> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let item = r.string()?;
        let score = r.i32()?;
        v.push(Recommendation { item, score });
    }
    Ok(v)
}

fn write_recommendations(b: &mut MessageBuilder, v: &[Recommendation]) {
    b.u32(v.len() as u32);
    for rec in v {
        b.string(&rec.item).i32(rec.score);
    }
}

fn read_port(r: &mut MessageReader) -> Result<u16, ProtocolError> {
    let p = r.u32()?;
    u16::try_from(p).map_err(|_| ProtocolError::Invalid(format!("port out of range: {p}")))
}

/// Defines a message made of primitive fields with `to_bytes` and `decode`.
macro_rules! message {
    ($(#[$m:meta])* $name:ident = $code:ident { $($(#[$fm:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Default)]
        pub struct $name {
            $($(#[$fm])* pub $field: $ty,)*
        }

        impl $name {
            /// Wire code of this message.
            pub const CODE: ServerCode = ServerCode::$code;

            /// Serializes into a length-prefixed frame.
            pub fn to_bytes(&self) -> Bytes {
                #[allow(unused_mut)]
                let mut b = builder(ServerCode::$code);
                $(Field::put(&self.$field, &mut b);)*
                b.build()
            }

            /// Decodes from a reader positioned after the code.
            #[allow(unused_variables)]
            pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
                Ok(Self { $($field: <$ty as Field>::get(r)?,)* })
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Login / session
// ---------------------------------------------------------------------------

/// Login request (code 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    pub minor_version: u32,
}

impl LoginRequest {
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
        minor_version: u32,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            minor_version,
        }
    }

    /// Lowercase hex MD5 of `username + password`.
    pub fn hash(&self) -> String {
        let mut h = Md5::new();
        h.update(self.username.as_bytes());
        h.update(self.password.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::Login);
        b.string(&self.username)
            .string(&self.password)
            .u32(MAJOR_VERSION)
            .string(&self.hash())
            .u32(self.minor_version);
        b.build()
    }
}

/// Login response (code 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginResponse {
    pub succeeded: bool,
    pub message: String,
    pub ip_address: Option<Ipv4Addr>,
    pub hash: Option<String>,
    pub is_supporter: bool,
}

impl LoginResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let succeeded = r.u8()? == 1;
        let mut message = r.string()?;
        let mut ip_address = None;
        let mut hash = None;
        let mut is_supporter = false;
        if succeeded {
            ip_address = Some(r.ip()?);
            hash = Some(r.string()?);
            if r.has_remaining() {
                is_supporter = r.u8()? == 1;
            }
        } else if r.has_remaining() {
            let detail = r.string()?;
            if !detail.trim().is_empty() {
                message.push_str(": ");
                message.push_str(&detail);
            }
        }
        Ok(Self {
            succeeded,
            message,
            ip_address,
            hash,
            is_supporter,
        })
    }
}

message! {
    /// Sets the port we listen on (code 2).
    SetListenPortCommand = SetListenPort { port: u32 }
}
message! {
    /// Sets our online status (code 28).
    SetOnlineStatusCommand = SetOnlineStatus { status: UserPresence }
}
message! {
    /// Keepalive ping (code 32), both directions.
    ServerPing = Ping {}
}
message! {
    /// Reports shared directory and file counts (code 35).
    SetSharedCountsCommand = SharedFoldersAndFiles { directory_count: u32, file_count: u32 }
}
message! {
    /// Reports our upload speed (code 121).
    SendUploadSpeedCommand = SendUploadSpeed { speed: u32 }
}
message! {
    /// Change password request/confirmation (code 142).
    NewPassword = NewPassword { password: String }
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

message! {
    /// Requests a user's address (code 3).
    UserAddressRequest = GetPeerAddress { username: String }
}

/// A user's address (code 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAddressResponse {
    pub username: String,
    pub ip_address: Ipv4Addr,
    pub port: u16,
}

impl UserAddressResponse {
    pub fn endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip_address, self.port)
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let username = r.string()?;
        let ip_address = r.ip()?;
        let port = read_port(r)?;
        Ok(Self {
            username,
            ip_address,
            port,
        })
    }
}

message! {
    /// Starts watching a user (code 5).
    WatchUserRequest = WatchUser { username: String }
}

/// Watch user response (code 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchUserResponse {
    pub username: String,
    pub exists: bool,
    pub user_data: Option<UserData>,
}

impl WatchUserResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let username = r.string()?;
        let exists = r.u8()? > 0;
        let mut user_data = None;
        if exists {
            let status = UserPresence::get(r)?;
            let average_speed = r.u32()?;
            let upload_count = r.u64()?;
            let file_count = r.u32()?;
            let directory_count = r.u32()?;
            let country_code = if r.has_remaining() {
                Some(r.string()?)
            } else {
                None
            };
            user_data = Some(UserData {
                username: username.clone(),
                status,
                average_speed,
                upload_count,
                file_count,
                directory_count,
                country_code,
                slots_free: None,
            });
        }
        Ok(Self {
            username,
            exists,
            user_data,
        })
    }
}

message! {
    /// Stops watching a user (code 6).
    UnwatchUserCommand = UnwatchUser { username: String }
}
message! {
    /// Requests a user's status (code 7).
    UserStatusRequest = GetStatus { username: String }
}

impl UserStatus {
    /// Decodes a GetStatus (7) response.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let username = r.string()?;
        let presence = UserPresence::get(r)?;
        let is_privileged = if r.has_remaining() {
            r.u8()? > 0
        } else {
            false
        };
        Ok(Self {
            username,
            presence,
            is_privileged,
        })
    }
}

message! {
    /// Requests a user's statistics (code 36).
    UserStatisticsRequest = GetUserStats { username: String }
}

impl UserStatistics {
    /// Decodes a GetUserStats (36) response.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            username: r.string()?,
            average_speed: r.u32()?,
            upload_count: r.u64()?,
            file_count: r.u32()?,
            directory_count: r.u32()?,
        })
    }
}

message! {
    /// Asks whether a user is privileged (code 122).
    UserPrivilegesRequest = UserPrivileges { username: String }
}
message! {
    /// Whether a user is privileged (code 122).
    UserPrivilegeResponse = UserPrivileges { username: String, is_privileged: bool }
}

// ---------------------------------------------------------------------------
// Peer connections
// ---------------------------------------------------------------------------

message! {
    /// Asks the server to have a peer connect to us (code 18).
    ConnectToPeerRequest = ConnectToPeer {
        token: u32,
        username: String,
        /// Connection type: "P", "F" or "D".
        connection_type: String,
    }
}

/// A peer asks us to connect to it (code 18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectToPeerResponse {
    pub username: String,
    pub connection_type: String,
    pub ip_address: Ipv4Addr,
    pub port: u16,
    pub token: u32,
    pub is_privileged: bool,
}

impl ConnectToPeerResponse {
    pub fn endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip_address, self.port)
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let username = r.string()?;
        let connection_type = r.string()?;
        let ip_address = r.ip()?;
        let port = read_port(r)?;
        let token = r.u32()?;
        let is_privileged = if r.has_remaining() {
            r.u8()? > 0
        } else {
            false
        };
        Ok(Self {
            username,
            connection_type,
            ip_address,
            port,
            token,
            is_privileged,
        })
    }
}

/// Failed to connect to a peer (code 1001), both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CannotConnect {
    pub token: u32,
    pub username: Option<String>,
}

impl CannotConnect {
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::CannotConnect);
        b.u32(self.token);
        if let Some(u) = self.username.as_deref().filter(|u| !u.is_empty()) {
            b.string(u);
        }
        b.build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let token = r.u32()?;
        let username = if r.has_remaining() {
            Some(r.string()?)
        } else {
            None
        };
        Ok(Self { token, username })
    }
}

// ---------------------------------------------------------------------------
// Private messages
// ---------------------------------------------------------------------------

message! {
    /// Sends a private message (code 22).
    PrivateMessageCommand = PrivateMessage { username: String, message: String }
}
message! {
    /// Acknowledges a private message (code 23).
    AcknowledgePrivateMessageCommand = AcknowledgePrivateMessage { id: u32 }
}

impl PrivateMessage {
    /// Decodes an incoming PrivateMessage (22).
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let id = r.u32()?;
        let seconds = r.u32()?;
        let username = r.string()?;
        let message = r.string()?;
        let replayed = if r.has_remaining() {
            r.u8()? != 1
        } else {
            false
        };
        Ok(Self {
            id,
            timestamp: UNIX_EPOCH + Duration::from_secs(u64::from(seconds)),
            is_admin: username == SERVER_USERNAME,
            username,
            message,
            replayed,
        })
    }

    /// Encodes as the server would send it (for tests/tools).
    pub fn to_bytes(&self) -> Bytes {
        let secs = self
            .timestamp
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let mut b = builder(ServerCode::PrivateMessage);
        b.u32(self.id)
            .u32(secs)
            .string(&self.username)
            .string(&self.message)
            .u8(u8::from(!self.replayed));
        b.build()
    }
}

/// Text of a global admin message (code 66).
pub fn decode_global_admin_message(r: &mut MessageReader) -> Result<String, ProtocolError> {
    r.string()
}

// ---------------------------------------------------------------------------
// Rooms
// ---------------------------------------------------------------------------

message! {
    /// Says something in a chat room (code 13).
    RoomMessageCommand = SayInChatRoom { room_name: String, message: String }
}

impl RoomMessage {
    /// Decodes SayInChatRoom (13) or PublicChat (152).
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            room: r.string()?,
            username: r.string()?,
            message: r.string()?,
        })
    }
}

/// Joins a room (code 14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinRoomRequest {
    pub room_name: String,
    pub is_private: bool,
}

impl JoinRoomRequest {
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::JoinRoom);
        b.string(&self.room_name).u32(u32::from(self.is_private));
        b.build()
    }
}

impl RoomData {
    /// Decodes a JoinRoom (14) response.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let name = r.string()?;
        let names = read_strings(r)?;

        let n = r.u32()? as usize;
        let mut statuses = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            statuses.push(UserPresence::get(r)?);
        }

        let n = r.u32()? as usize;
        let mut stats = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            stats.push((r.u32()?, r.u64()?, r.u32()?, r.u32()?));
        }

        let n = r.u32()? as usize;
        let mut slots = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            slots.push(r.u32()?);
        }

        let countries = read_strings(r)?;

        let users = names
            .into_iter()
            .enumerate()
            .map(|(i, username)| {
                let (average_speed, upload_count, file_count, directory_count) =
                    stats.get(i).copied().unwrap_or_default();
                UserData {
                    username,
                    status: statuses.get(i).copied().unwrap_or_default(),
                    average_speed,
                    upload_count,
                    file_count,
                    directory_count,
                    country_code: countries.get(i).cloned(),
                    slots_free: slots.get(i).copied(),
                }
            })
            .collect();

        let (owner, operators) = if r.has_remaining() {
            (Some(r.string()?), read_strings(r)?)
        } else {
            (None, Vec::new())
        };

        Ok(Self {
            name,
            users,
            is_private: owner.is_some(),
            owner,
            operators,
        })
    }

    /// Encodes as the server would send it (for tests/tools).
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::JoinRoom);
        b.string(&self.name);
        b.u32(self.users.len() as u32);
        for u in &self.users {
            b.string(&u.username);
        }
        b.u32(self.users.len() as u32);
        for u in &self.users {
            b.u32(u.status.as_u32());
        }
        b.u32(self.users.len() as u32);
        for u in &self.users {
            b.u32(u.average_speed)
                .u64(u.upload_count)
                .u32(u.file_count)
                .u32(u.directory_count);
        }
        b.u32(self.users.len() as u32);
        for u in &self.users {
            b.u32(u.slots_free.unwrap_or(0));
        }
        b.u32(self.users.len() as u32);
        for u in &self.users {
            b.string(u.country_code.as_deref().unwrap_or(""));
        }
        if let Some(owner) = &self.owner {
            b.string(owner);
            write_strings(&mut b, &self.operators);
        }
        b.build()
    }
}

message! {
    /// Leaves a room (code 15).
    LeaveRoomRequest = LeaveRoom { room_name: String }
}
message! {
    /// Confirmation of leaving a room (code 15).
    LeaveRoomResponse = LeaveRoom { room_name: String }
}

/// A user joined a room (code 16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserJoinedRoomNotification {
    pub room_name: String,
    pub user_data: UserData,
}

impl UserJoinedRoomNotification {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let room_name = r.string()?;
        let username = r.string()?;
        let status = UserPresence::get(r)?;
        let average_speed = r.u32()?;
        let upload_count = r.u64()?;
        let file_count = r.u32()?;
        let directory_count = r.u32()?;
        let slots_free = r.u32()?;
        let country_code = if r.has_remaining() {
            Some(r.string()?)
        } else {
            None
        };
        Ok(Self {
            room_name,
            user_data: UserData {
                username,
                status,
                average_speed,
                upload_count,
                file_count,
                directory_count,
                country_code,
                slots_free: Some(slots_free),
            },
        })
    }
}

message! {
    /// A user left a room (code 17).
    UserLeftRoomNotification = UserLeftRoom { room_name: String, username: String }
}
message! {
    /// Requests the room list (code 64).
    RoomListRequest = RoomList {}
}

fn read_room_info_list(r: &mut MessageReader) -> Result<Vec<RoomInfo>, ProtocolError> {
    let names = read_strings(r)?;
    let n = r.u32()? as usize;
    let mut counts = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        counts.push(r.u32()?);
    }
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(i, name)| RoomInfo::with_count(name, counts.get(i).copied().unwrap_or(0)))
        .collect())
}

fn write_room_info_list(b: &mut MessageBuilder, rooms: &[RoomInfo]) {
    b.u32(rooms.len() as u32);
    for room in rooms {
        b.string(&room.name);
    }
    b.u32(rooms.len() as u32);
    for room in rooms {
        b.u32(room.user_count);
    }
}

impl RoomList {
    /// Decodes a RoomList (64) response.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let public = read_room_info_list(r)?;
        let owned = if r.has_remaining() {
            read_room_info_list(r)?
        } else {
            Vec::new()
        };
        let private = if r.has_remaining() {
            read_room_info_list(r)?
        } else {
            Vec::new()
        };
        let moderated_rooms = if r.has_remaining() {
            read_strings(r)?
                .into_iter()
                .map(|n| RoomInfo::with_count(n, 0))
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            public,
            private,
            owned,
            moderated_rooms,
        })
    }

    /// Encodes as the server would send it (for tests/tools).
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::RoomList);
        write_room_info_list(&mut b, &self.public);
        write_room_info_list(&mut b, &self.owned);
        write_room_info_list(&mut b, &self.private);
        b.u32(self.moderated_rooms.len() as u32);
        for room in &self.moderated_rooms {
            b.string(&room.name);
        }
        b.build()
    }
}

/// List of rooms' tickers (code 113).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTickerListNotification {
    pub room_name: String,
    pub tickers: Vec<RoomTicker>,
}

impl RoomTickerListNotification {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let room_name = r.string()?;
        let n = r.u32()? as usize;
        let mut tickers = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            tickers.push(RoomTicker {
                username: r.string()?,
                message: r.string()?,
            });
        }
        Ok(Self { room_name, tickers })
    }
}

/// A ticker was added to a room (code 114).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTickerAddedNotification {
    pub room_name: String,
    pub ticker: RoomTicker,
}

impl RoomTickerAddedNotification {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let room_name = r.string()?;
        let ticker = RoomTicker {
            username: r.string()?,
            message: r.string()?,
        };
        Ok(Self { room_name, ticker })
    }
}

message! {
    /// A ticker was removed from a room (code 115).
    RoomTickerRemovedNotification = RoomTickerRemove { room_name: String, username: String }
}
message! {
    /// Sets our ticker in a room (code 116).
    SetRoomTickerCommand = SetRoomTicker { room_name: String, message: String }
}
message! {
    /// Adds a member to a private room (code 134), both directions.
    PrivateRoomAddUser = PrivateRoomAddUser { room_name: String, username: String }
}
message! {
    /// Removes a member from a private room (code 135), both directions.
    PrivateRoomRemoveUser = PrivateRoomRemoveUser { room_name: String, username: String }
}
message! {
    /// Drops our membership of a private room (code 136).
    PrivateRoomDropMembershipCommand = PrivateRoomDropMembership { room_name: String }
}
message! {
    /// Drops our ownership of a private room (code 137).
    PrivateRoomDropOwnershipCommand = PrivateRoomDropOwnership { room_name: String }
}
message! {
    /// Toggles acceptance of private room invitations (code 141), both directions.
    PrivateRoomToggle = PrivateRoomToggle { accept_invitations: bool }
}
message! {
    /// Adds an operator to a private room (code 143), both directions.
    PrivateRoomAddOperator = PrivateRoomAddOperator { room_name: String, username: String }
}
message! {
    /// Removes an operator from a private room (code 144), both directions.
    PrivateRoomRemoveOperator = PrivateRoomRemoveOperator { room_name: String, username: String }
}
message! {
    /// Starts receiving public chat (code 150).
    StartPublicChatCommand = AskPublicChat {}
}
message! {
    /// Stops receiving public chat (code 151).
    StopPublicChatCommand = StopPublicChat {}
}
message! {
    /// We could not join a room (code 1003).
    CannotJoinRoomNotification = CannotJoinRoom { room_name: String }
}

impl RoomInfo {
    /// Decodes PrivateRoomUsers (133) or PrivateRoomOwned (148): name + user list.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let name = r.string()?;
        let users = read_strings(r)?;
        Ok(Self::with_users(name, users))
    }
}

// ---------------------------------------------------------------------------
// Searches
// ---------------------------------------------------------------------------

message! {
    /// Network-wide file search (code 26).
    SearchRequest = FileSearch { token: u32, search_text: String }
}
message! {
    /// Incoming file search from the server (code 26).
    ServerSearchRequest = FileSearch { username: String, token: u32, query: String }
}
message! {
    /// Search a single user's shares (code 42).
    UserSearchRequest = UserSearch { username: String, token: u32, search_text: String }
}
message! {
    /// Wishlist search (code 103).
    WishlistSearchRequest = WishlistSearch { token: u32, search_text: String }
}
message! {
    /// Search within a room (code 120).
    RoomSearchRequest = RoomSearch { room_name: String, token: u32, search_text: String }
}

/// Phrases excluded from search results (code 160).
pub fn decode_excluded_search_phrases(r: &mut MessageReader) -> Result<Vec<String>, ProtocolError> {
    read_strings(r)
}

// ---------------------------------------------------------------------------
// Interests & recommendations
// ---------------------------------------------------------------------------

message! {
    /// Adds a liked interest (code 51).
    LikedInterestAddCommand = LikedInterestAdd { interest: String }
}
message! {
    /// Removes a liked interest (code 52).
    LikedInterestRemoveCommand = LikedInterestRemove { interest: String }
}
message! {
    /// Requests personal recommendations (code 54).
    PersonalRecommendationsRequest = GetPersonalRecommendations {}
}
message! {
    /// Requests global recommendations (code 56).
    GlobalRecommendationsRequest = GetGlobalRecommendations {}
}
message! {
    /// Requests a user's interests (code 57).
    UserInterestsRequest = GetUserInterests { username: String }
}
message! {
    /// Requests similar users (code 110).
    PersonalSimilarUsersRequest = GetPersonalSimilarUsers {}
}
message! {
    /// Requests recommendations for an interest (code 111).
    InterestRecommendationsRequest = GetInterestRecommendations { interest: String }
}
message! {
    /// Requests users sharing an interest (code 112).
    InterestSimilarUsersRequest = GetInterestSimilarUsers { interest: String }
}
message! {
    /// Adds a hated interest (code 117).
    HatedInterestAddCommand = HatedInterestAdd { interest: String }
}
message! {
    /// Removes a hated interest (code 118).
    HatedInterestRemoveCommand = HatedInterestRemove { interest: String }
}

/// Personal (54) or global (56) recommendations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecommendationsResponse {
    pub recommendations: Vec<Recommendation>,
    pub unrecommendations: Vec<Recommendation>,
}

impl RecommendationsResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let recommendations = read_recommendations(r)?;
        let unrecommendations = read_recommendations(r)?;
        Ok(Self {
            recommendations,
            unrecommendations,
        })
    }

    /// Encodes with the given code (`GetPersonalRecommendations` or `GetGlobalRecommendations`).
    pub fn to_bytes(&self, code: ServerCode) -> Bytes {
        let mut b = builder(code);
        write_recommendations(&mut b, &self.recommendations);
        write_recommendations(&mut b, &self.unrecommendations);
        b.build()
    }
}

impl UserInterests {
    /// Decodes a GetUserInterests (57) response.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let username = r.string()?;
        let liked = read_strings(r)?;
        let hated = read_strings(r)?;
        Ok(Self {
            username,
            liked,
            hated,
        })
    }

    /// Encodes as the server would send it (for tests/tools).
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::GetUserInterests);
        b.string(&self.username);
        write_strings(&mut b, &self.liked);
        write_strings(&mut b, &self.hated);
        b.build()
    }
}

/// A user and similarity rating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarUser {
    pub username: String,
    pub rating: i32,
}

/// Similar users (code 110).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PersonalSimilarUsersResponse {
    pub users: Vec<SimilarUser>,
}

impl PersonalSimilarUsersResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let n = r.u32()? as usize;
        let mut users = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            users.push(SimilarUser {
                username: r.string()?,
                rating: r.i32()?,
            });
        }
        Ok(Self { users })
    }
}

/// Recommendations for an interest (code 111).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterestRecommendationsResponse {
    pub interest: String,
    pub recommendations: Vec<Recommendation>,
}

impl InterestRecommendationsResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let interest = r.string()?;
        let recommendations = read_recommendations(r)?;
        Ok(Self {
            interest,
            recommendations,
        })
    }
}

/// Users sharing an interest (code 112).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterestSimilarUsersResponse {
    pub interest: String,
    pub usernames: Vec<String>,
}

impl InterestSimilarUsersResponse {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let interest = r.string()?;
        let usernames = read_strings(r)?;
        Ok(Self {
            interest,
            usernames,
        })
    }
}

// ---------------------------------------------------------------------------
// Privileges
// ---------------------------------------------------------------------------

message! {
    /// Asks for our remaining privilege time (code 92); response is seconds (u32).
    CheckPrivilegesRequest = CheckPrivileges {}
}
message! {
    /// Gives privilege days to a user (code 123).
    GivePrivilegesCommand = GivePrivileges { username: String, days: u32 }
}
message! {
    /// Acknowledges a privilege notification (code 125).
    AcknowledgePrivilegeNotificationCommand = AcknowledgeNotifyPrivileges { id: u32 }
}

impl PrivilegeNotification {
    /// Decodes a NotifyPrivileges (124) message.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let id = r.u32()?;
        let username = r.string()?;
        Ok(Self {
            id: Some(id),
            username,
        })
    }
}

// ---------------------------------------------------------------------------
// Distributed network
// ---------------------------------------------------------------------------

message! {
    /// Whether we have no distributed parent (code 71).
    HaveNoParentsCommand = HaveNoParents { have_no_parents: bool }
}
message! {
    /// Whether we accept distributed children (code 100).
    AcceptChildrenCommand = AcceptChildren { accepted: bool }
}
message! {
    /// Our distributed branch level (code 126).
    BranchLevelCommand = BranchLevel { level: u32 }
}
message! {
    /// Our distributed branch root (code 127).
    BranchRootCommand = BranchRoot { username: String }
}
message! {
    /// Depth of our distributed children (code 129).
    ChildDepthCommand = ChildDepth { depth: u32 }
}

/// Reports our parent's IP (code 73); no IP writes an empty body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParentsIpCommand {
    pub ip_address: Option<Ipv4Addr>,
}

impl ParentsIpCommand {
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::ParentsIP);
        if let Some(ip) = self.ip_address {
            b.ip(ip);
        }
        b.build()
    }
}

/// A candidate distributed parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInfoParent {
    pub username: String,
    pub ip_address: Ipv4Addr,
    pub port: u16,
}

impl NetInfoParent {
    pub fn endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip_address, self.port)
    }
}

/// Candidate distributed parents (code 102).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetInfoNotification {
    pub parents: Vec<NetInfoParent>,
}

impl NetInfoNotification {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let n = r.u32()? as usize;
        let mut parents = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let username = r.string()?;
            let ip_address = r.ip()?;
            let port = read_port(r)?;
            parents.push(NetInfoParent {
                username,
                ip_address,
                port,
            });
        }
        Ok(Self { parents })
    }

    /// Encodes as the server would send it (for tests/tools).
    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::NetInfo);
        b.u32(self.parents.len() as u32);
        for p in &self.parents {
            b.string(&p.username)
                .ip(p.ip_address)
                .u32(u32::from(p.port));
        }
        b.build()
    }
}

/// A distributed message embedded in a server message (code 93).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedMessage {
    pub distributed_code: u8,
    /// Distributed message body after its code.
    pub payload: Bytes,
}

impl EmbeddedMessage {
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let distributed_code = r.u8()?;
        let len = r.remaining();
        let payload = r.bytes(len)?;
        Ok(Self {
            distributed_code,
            payload,
        })
    }

    /// Distributed message body (u8 code + payload, no length prefix).
    pub fn distributed_body(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(1 + self.payload.len());
        buf.put_u8(self.distributed_code);
        buf.put_slice(&self.payload);
        buf.freeze()
    }

    /// Length-prefixed distributed frame, ready to forward to children.
    pub fn distributed_frame(&self) -> Bytes {
        let mut b = MessageBuilder::new();
        b.code_u8(self.distributed_code).bytes(&self.payload);
        b.build()
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = builder(ServerCode::EmbeddedMessage);
        b.u8(self.distributed_code).bytes(&self.payload);
        b.build()
    }
}

// ---------------------------------------------------------------------------
// Incoming dispatch
// ---------------------------------------------------------------------------

/// Every message the server can send us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerIncoming {
    Login(LoginResponse),
    PeerAddress(UserAddressResponse),
    WatchUser(WatchUserResponse),
    UserStatus(UserStatus),
    RoomMessage(RoomMessage),
    JoinRoom(RoomData),
    LeaveRoom(LeaveRoomResponse),
    UserJoinedRoom(UserJoinedRoomNotification),
    UserLeftRoom(UserLeftRoomNotification),
    ConnectToPeer(ConnectToPeerResponse),
    PrivateMessage(PrivateMessage),
    FileSearch(ServerSearchRequest),
    Ping,
    UserStatistics(UserStatistics),
    KickedFromServer,
    PersonalRecommendations(RecommendationsResponse),
    GlobalRecommendations(RecommendationsResponse),
    UserInterests(UserInterests),
    RoomList(RoomList),
    GlobalAdminMessage(String),
    PrivilegedUsers(Vec<String>),
    ParentMinSpeed(u32),
    ParentSpeedRatio(u32),
    ParentInactivityTimeout(u32),
    SearchInactivityTimeout(u32),
    MinimumParentsInCache(u32),
    DistributedAliveInterval(u32),
    AddPrivilegedUser(String),
    /// Remaining privilege time in seconds.
    CheckPrivileges(u32),
    EmbeddedMessage(EmbeddedMessage),
    NetInfo(NetInfoNotification),
    /// Wishlist search interval in seconds.
    WishlistInterval(u32),
    PersonalSimilarUsers(PersonalSimilarUsersResponse),
    InterestRecommendations(InterestRecommendationsResponse),
    InterestSimilarUsers(InterestSimilarUsersResponse),
    RoomTickers(RoomTickerListNotification),
    RoomTickerAdd(RoomTickerAddedNotification),
    RoomTickerRemove(RoomTickerRemovedNotification),
    UserPrivileges(UserPrivilegeResponse),
    NotifyPrivileges(PrivilegeNotification),
    DistributedReset,
    PrivateRoomUsers(RoomInfo),
    PrivateRoomAddUser(PrivateRoomAddUser),
    PrivateRoomRemoveUser(PrivateRoomRemoveUser),
    /// We were added as a member of the named private room.
    PrivateRoomAdded(String),
    /// We were removed from the named private room.
    PrivateRoomRemoved(String),
    PrivateRoomToggle(PrivateRoomToggle),
    NewPassword(NewPassword),
    PrivateRoomAddOperator(PrivateRoomAddOperator),
    PrivateRoomRemoveOperator(PrivateRoomRemoveOperator),
    /// We were made operator of the named private room.
    PrivateRoomOperatorAdded(String),
    /// We are no longer operator of the named private room.
    PrivateRoomOperatorRemoved(String),
    PrivateRoomOwned(RoomInfo),
    PublicChat(RoomMessage),
    ExcludedSearchPhrases(Vec<String>),
    CannotConnect(CannotConnect),
    CannotJoinRoom(CannotJoinRoomNotification),
    Unknown {
        code: u32,
        body: Bytes,
    },
}

mod c {
    use super::ServerCode as S;
    pub const LOGIN: u32 = S::Login as u32;
    pub const GET_PEER_ADDRESS: u32 = S::GetPeerAddress as u32;
    pub const WATCH_USER: u32 = S::WatchUser as u32;
    pub const GET_STATUS: u32 = S::GetStatus as u32;
    pub const SAY_IN_CHAT_ROOM: u32 = S::SayInChatRoom as u32;
    pub const JOIN_ROOM: u32 = S::JoinRoom as u32;
    pub const LEAVE_ROOM: u32 = S::LeaveRoom as u32;
    pub const USER_JOINED_ROOM: u32 = S::UserJoinedRoom as u32;
    pub const USER_LEFT_ROOM: u32 = S::UserLeftRoom as u32;
    pub const CONNECT_TO_PEER: u32 = S::ConnectToPeer as u32;
    pub const PRIVATE_MESSAGE: u32 = S::PrivateMessage as u32;
    pub const FILE_SEARCH: u32 = S::FileSearch as u32;
    pub const PING: u32 = S::Ping as u32;
    pub const GET_USER_STATS: u32 = S::GetUserStats as u32;
    pub const KICKED_FROM_SERVER: u32 = S::KickedFromServer as u32;
    pub const GET_PERSONAL_RECOMMENDATIONS: u32 = S::GetPersonalRecommendations as u32;
    pub const GET_GLOBAL_RECOMMENDATIONS: u32 = S::GetGlobalRecommendations as u32;
    pub const GET_USER_INTERESTS: u32 = S::GetUserInterests as u32;
    pub const ROOM_LIST: u32 = S::RoomList as u32;
    pub const GLOBAL_ADMIN_MESSAGE: u32 = S::GlobalAdminMessage as u32;
    pub const PRIVILEGED_USERS: u32 = S::PrivilegedUsers as u32;
    pub const PARENT_MIN_SPEED: u32 = S::ParentMinSpeed as u32;
    pub const PARENT_SPEED_RATIO: u32 = S::ParentSpeedRatio as u32;
    pub const PARENT_INACTIVITY_TIMEOUT: u32 = S::ParentInactivityTimeout as u32;
    pub const SEARCH_INACTIVITY_TIMEOUT: u32 = S::SearchInactivityTimeout as u32;
    pub const MINIMUM_PARENTS_IN_CACHE: u32 = S::MinimumParentsInCache as u32;
    pub const DISTRIBUTED_ALIVE_INTERVAL: u32 = S::DistributedAliveInterval as u32;
    pub const ADD_PRIVILEGED_USER: u32 = S::AddPrivilegedUser as u32;
    pub const CHECK_PRIVILEGES: u32 = S::CheckPrivileges as u32;
    pub const EMBEDDED_MESSAGE: u32 = S::EmbeddedMessage as u32;
    pub const NET_INFO: u32 = S::NetInfo as u32;
    pub const WISHLIST_INTERVAL: u32 = S::WishlistInterval as u32;
    pub const GET_PERSONAL_SIMILAR_USERS: u32 = S::GetPersonalSimilarUsers as u32;
    pub const GET_INTEREST_RECOMMENDATIONS: u32 = S::GetInterestRecommendations as u32;
    pub const GET_INTEREST_SIMILAR_USERS: u32 = S::GetInterestSimilarUsers as u32;
    pub const ROOM_TICKERS: u32 = S::RoomTickers as u32;
    pub const ROOM_TICKER_ADD: u32 = S::RoomTickerAdd as u32;
    pub const ROOM_TICKER_REMOVE: u32 = S::RoomTickerRemove as u32;
    pub const USER_PRIVILEGES: u32 = S::UserPrivileges as u32;
    pub const NOTIFY_PRIVILEGES: u32 = S::NotifyPrivileges as u32;
    pub const DISTRIBUTED_RESET: u32 = S::DistributedReset as u32;
    pub const PRIVATE_ROOM_USERS: u32 = S::PrivateRoomUsers as u32;
    pub const PRIVATE_ROOM_ADD_USER: u32 = S::PrivateRoomAddUser as u32;
    pub const PRIVATE_ROOM_REMOVE_USER: u32 = S::PrivateRoomRemoveUser as u32;
    pub const PRIVATE_ROOM_ADDED: u32 = S::PrivateRoomAdded as u32;
    pub const PRIVATE_ROOM_REMOVED: u32 = S::PrivateRoomRemoved as u32;
    pub const PRIVATE_ROOM_TOGGLE: u32 = S::PrivateRoomToggle as u32;
    pub const NEW_PASSWORD: u32 = S::NewPassword as u32;
    pub const PRIVATE_ROOM_ADD_OPERATOR: u32 = S::PrivateRoomAddOperator as u32;
    pub const PRIVATE_ROOM_REMOVE_OPERATOR: u32 = S::PrivateRoomRemoveOperator as u32;
    pub const PRIVATE_ROOM_OPERATOR_ADDED: u32 = S::PrivateRoomOperatorAdded as u32;
    pub const PRIVATE_ROOM_OPERATOR_REMOVED: u32 = S::PrivateRoomOperatorRemoved as u32;
    pub const PRIVATE_ROOM_OWNED: u32 = S::PrivateRoomOwned as u32;
    pub const PUBLIC_CHAT: u32 = S::PublicChat as u32;
    pub const EXCLUDED_SEARCH_PHRASES: u32 = S::ExcludedSearchPhrases as u32;
    pub const CANNOT_CONNECT: u32 = S::CannotConnect as u32;
    pub const CANNOT_JOIN_ROOM: u32 = S::CannotJoinRoom as u32;
}

impl ServerIncoming {
    /// Decodes a frame body (no length prefix) starting with the u32 code.
    pub fn decode(body: Bytes) -> Result<Self, ProtocolError> {
        let mut r = MessageReader::new(body);
        let code = r.code_u32()?;
        let r = &mut r;
        Ok(match code {
            c::LOGIN => Self::Login(LoginResponse::decode(r)?),
            c::GET_PEER_ADDRESS => Self::PeerAddress(UserAddressResponse::decode(r)?),
            c::WATCH_USER => Self::WatchUser(WatchUserResponse::decode(r)?),
            c::GET_STATUS => Self::UserStatus(UserStatus::decode(r)?),
            c::SAY_IN_CHAT_ROOM => Self::RoomMessage(RoomMessage::decode(r)?),
            c::JOIN_ROOM => Self::JoinRoom(RoomData::decode(r)?),
            c::LEAVE_ROOM => Self::LeaveRoom(LeaveRoomResponse::decode(r)?),
            c::USER_JOINED_ROOM => Self::UserJoinedRoom(UserJoinedRoomNotification::decode(r)?),
            c::USER_LEFT_ROOM => Self::UserLeftRoom(UserLeftRoomNotification::decode(r)?),
            c::CONNECT_TO_PEER => Self::ConnectToPeer(ConnectToPeerResponse::decode(r)?),
            c::PRIVATE_MESSAGE => Self::PrivateMessage(PrivateMessage::decode(r)?),
            c::FILE_SEARCH => Self::FileSearch(ServerSearchRequest::decode(r)?),
            c::PING => Self::Ping,
            c::GET_USER_STATS => Self::UserStatistics(UserStatistics::decode(r)?),
            c::KICKED_FROM_SERVER => Self::KickedFromServer,
            c::GET_PERSONAL_RECOMMENDATIONS => {
                Self::PersonalRecommendations(RecommendationsResponse::decode(r)?)
            }
            c::GET_GLOBAL_RECOMMENDATIONS => {
                Self::GlobalRecommendations(RecommendationsResponse::decode(r)?)
            }
            c::GET_USER_INTERESTS => Self::UserInterests(UserInterests::decode(r)?),
            c::ROOM_LIST => Self::RoomList(RoomList::decode(r)?),
            c::GLOBAL_ADMIN_MESSAGE => Self::GlobalAdminMessage(decode_global_admin_message(r)?),
            c::PRIVILEGED_USERS => Self::PrivilegedUsers(read_strings(r)?),
            c::PARENT_MIN_SPEED => Self::ParentMinSpeed(r.u32()?),
            c::PARENT_SPEED_RATIO => Self::ParentSpeedRatio(r.u32()?),
            c::PARENT_INACTIVITY_TIMEOUT => Self::ParentInactivityTimeout(r.u32()?),
            c::SEARCH_INACTIVITY_TIMEOUT => Self::SearchInactivityTimeout(r.u32()?),
            c::MINIMUM_PARENTS_IN_CACHE => Self::MinimumParentsInCache(r.u32()?),
            c::DISTRIBUTED_ALIVE_INTERVAL => Self::DistributedAliveInterval(r.u32()?),
            c::ADD_PRIVILEGED_USER => Self::AddPrivilegedUser(r.string()?),
            c::CHECK_PRIVILEGES => Self::CheckPrivileges(r.u32()?),
            c::EMBEDDED_MESSAGE => Self::EmbeddedMessage(EmbeddedMessage::decode(r)?),
            c::NET_INFO => Self::NetInfo(NetInfoNotification::decode(r)?),
            c::WISHLIST_INTERVAL => Self::WishlistInterval(r.u32()?),
            c::GET_PERSONAL_SIMILAR_USERS => {
                Self::PersonalSimilarUsers(PersonalSimilarUsersResponse::decode(r)?)
            }
            c::GET_INTEREST_RECOMMENDATIONS => {
                Self::InterestRecommendations(InterestRecommendationsResponse::decode(r)?)
            }
            c::GET_INTEREST_SIMILAR_USERS => {
                Self::InterestSimilarUsers(InterestSimilarUsersResponse::decode(r)?)
            }
            c::ROOM_TICKERS => Self::RoomTickers(RoomTickerListNotification::decode(r)?),
            c::ROOM_TICKER_ADD => Self::RoomTickerAdd(RoomTickerAddedNotification::decode(r)?),
            c::ROOM_TICKER_REMOVE => {
                Self::RoomTickerRemove(RoomTickerRemovedNotification::decode(r)?)
            }
            c::USER_PRIVILEGES => Self::UserPrivileges(UserPrivilegeResponse::decode(r)?),
            c::NOTIFY_PRIVILEGES => Self::NotifyPrivileges(PrivilegeNotification::decode(r)?),
            c::DISTRIBUTED_RESET => Self::DistributedReset,
            c::PRIVATE_ROOM_USERS => Self::PrivateRoomUsers(RoomInfo::decode(r)?),
            c::PRIVATE_ROOM_ADD_USER => Self::PrivateRoomAddUser(PrivateRoomAddUser::decode(r)?),
            c::PRIVATE_ROOM_REMOVE_USER => {
                Self::PrivateRoomRemoveUser(PrivateRoomRemoveUser::decode(r)?)
            }
            c::PRIVATE_ROOM_ADDED => Self::PrivateRoomAdded(r.string()?),
            c::PRIVATE_ROOM_REMOVED => Self::PrivateRoomRemoved(r.string()?),
            c::PRIVATE_ROOM_TOGGLE => Self::PrivateRoomToggle(PrivateRoomToggle::decode(r)?),
            c::NEW_PASSWORD => Self::NewPassword(NewPassword::decode(r)?),
            c::PRIVATE_ROOM_ADD_OPERATOR => {
                Self::PrivateRoomAddOperator(PrivateRoomAddOperator::decode(r)?)
            }
            c::PRIVATE_ROOM_REMOVE_OPERATOR => {
                Self::PrivateRoomRemoveOperator(PrivateRoomRemoveOperator::decode(r)?)
            }
            c::PRIVATE_ROOM_OPERATOR_ADDED => Self::PrivateRoomOperatorAdded(r.string()?),
            c::PRIVATE_ROOM_OPERATOR_REMOVED => Self::PrivateRoomOperatorRemoved(r.string()?),
            c::PRIVATE_ROOM_OWNED => Self::PrivateRoomOwned(RoomInfo::decode(r)?),
            c::PUBLIC_CHAT => Self::PublicChat(RoomMessage::decode(r)?),
            c::EXCLUDED_SEARCH_PHRASES => {
                Self::ExcludedSearchPhrases(decode_excluded_search_phrases(r)?)
            }
            c::CANNOT_CONNECT => Self::CannotConnect(CannotConnect::decode(r)?),
            c::CANNOT_JOIN_ROOM => Self::CannotJoinRoom(CannotJoinRoomNotification::decode(r)?),
            code => {
                let len = r.remaining();
                Self::Unknown {
                    code,
                    body: r.bytes(len)?,
                }
            }
        })
    }

    /// Wishlist interval as a duration, if this is a `WishlistInterval` message.
    pub fn wishlist_interval(&self) -> Option<Duration> {
        match self {
            Self::WishlistInterval(s) => Some(Duration::from_secs(u64::from(*s))),
            _ => None,
        }
    }
}

/// Converts a server UNIX timestamp (seconds) to `SystemTime`.
pub fn timestamp_from_secs(secs: u32) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::from(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent little-endian frame writer for byte-exact checks.
    #[derive(Default)]
    struct W(Vec<u8>);

    impl W {
        fn code(code: ServerCode) -> Self {
            let mut w = W::default();
            w.u32(code as u32);
            w
        }
        fn u8(&mut self, v: u8) -> &mut Self {
            self.0.push(v);
            self
        }
        fn u32(&mut self, v: u32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn i32(&mut self, v: i32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn u64(&mut self, v: u64) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn s(&mut self, v: &str) -> &mut Self {
            self.u32(v.len() as u32);
            self.0.extend_from_slice(v.as_bytes());
            self
        }
        fn ip(&mut self, ip: Ipv4Addr) -> &mut Self {
            let mut o = ip.octets();
            o.reverse();
            self.0.extend_from_slice(&o);
            self
        }
        fn body(&self) -> Bytes {
            Bytes::copy_from_slice(&self.0)
        }
        fn frame(&self) -> Bytes {
            let mut v = (self.0.len() as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&self.0);
            Bytes::from(v)
        }
    }

    fn incoming(w: &W) -> ServerIncoming {
        ServerIncoming::decode(w.body()).expect("decode")
    }

    /// Decodes an outgoing frame back via the macro-generated decoder.
    fn reader_after_code(frame: &Bytes) -> MessageReader {
        let mut r = MessageReader::new(frame.slice(4..));
        r.code_u32().unwrap();
        r
    }

    #[test]
    fn login_request_bytes_and_hash() {
        let req = LoginRequest::new("username", "password", 100);
        assert_eq!(req.hash(), "d51c9a7e9353746a6020f9602d452929");
        let expected = W::code(ServerCode::Login)
            .s("username")
            .s("password")
            .u32(170)
            .s("d51c9a7e9353746a6020f9602d452929")
            .u32(100)
            .frame();
        assert_eq!(req.to_bytes(), expected);
        assert_eq!(
            LoginRequest::new("foo", "bar", 0).hash(),
            "3858f62230ac3c915f300c664312c63f"
        );
    }

    #[test]
    fn login_response_success() {
        let ip = Ipv4Addr::new(1, 2, 3, 4);
        let mut w = W::code(ServerCode::Login);
        w.u8(1).s("welcome").ip(ip).s("hash").u8(1);
        match incoming(&w) {
            ServerIncoming::Login(l) => {
                assert!(l.succeeded);
                assert_eq!(l.message, "welcome");
                assert_eq!(l.ip_address, Some(ip));
                assert_eq!(l.hash.as_deref(), Some("hash"));
                assert!(l.is_supporter);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn login_response_failure_with_detail() {
        let mut w = W::code(ServerCode::Login);
        w.u8(0).s("INVALIDUSERNAME").s("too long");
        match incoming(&w) {
            ServerIncoming::Login(l) => {
                assert!(!l.succeeded);
                assert_eq!(l.message, "INVALIDUSERNAME: too long");
                assert_eq!(l.ip_address, None);
            }
            other => panic!("{other:?}"),
        }
        let mut w = W::code(ServerCode::Login);
        w.u8(0).s("INVALIDUSERNAME").s("  ");
        match incoming(&w) {
            ServerIncoming::Login(l) => assert_eq!(l.message, "INVALIDUSERNAME"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn simple_outgoing_bytes() {
        let cases: Vec<(Bytes, Bytes)> = vec![
            (
                SetListenPortCommand { port: 50000 }.to_bytes(),
                W::code(ServerCode::SetListenPort).u32(50000).frame(),
            ),
            (
                UserAddressRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetPeerAddress).s("u").frame(),
            ),
            (
                WatchUserRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::WatchUser).s("u").frame(),
            ),
            (
                UnwatchUserCommand {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::UnwatchUser).s("u").frame(),
            ),
            (
                UserStatusRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetStatus).s("u").frame(),
            ),
            (
                RoomMessageCommand {
                    room_name: "r".into(),
                    message: "m".into(),
                }
                .to_bytes(),
                W::code(ServerCode::SayInChatRoom).s("r").s("m").frame(),
            ),
            (
                JoinRoomRequest {
                    room_name: "r".into(),
                    is_private: true,
                }
                .to_bytes(),
                W::code(ServerCode::JoinRoom).s("r").u32(1).frame(),
            ),
            (
                LeaveRoomRequest {
                    room_name: "r".into(),
                }
                .to_bytes(),
                W::code(ServerCode::LeaveRoom).s("r").frame(),
            ),
            (
                ConnectToPeerRequest {
                    token: 7,
                    username: "u".into(),
                    connection_type: "P".into(),
                }
                .to_bytes(),
                W::code(ServerCode::ConnectToPeer)
                    .u32(7)
                    .s("u")
                    .s("P")
                    .frame(),
            ),
            (
                PrivateMessageCommand {
                    username: "u".into(),
                    message: "hi".into(),
                }
                .to_bytes(),
                W::code(ServerCode::PrivateMessage).s("u").s("hi").frame(),
            ),
            (
                AcknowledgePrivateMessageCommand { id: 9 }.to_bytes(),
                W::code(ServerCode::AcknowledgePrivateMessage)
                    .u32(9)
                    .frame(),
            ),
            (
                SearchRequest {
                    token: 5,
                    search_text: "q".into(),
                }
                .to_bytes(),
                W::code(ServerCode::FileSearch).u32(5).s("q").frame(),
            ),
            (
                SetOnlineStatusCommand {
                    status: UserPresence::Away,
                }
                .to_bytes(),
                W::code(ServerCode::SetOnlineStatus).u32(1).frame(),
            ),
            (ServerPing {}.to_bytes(), W::code(ServerCode::Ping).frame()),
            (
                SetSharedCountsCommand {
                    directory_count: 3,
                    file_count: 4,
                }
                .to_bytes(),
                W::code(ServerCode::SharedFoldersAndFiles)
                    .u32(3)
                    .u32(4)
                    .frame(),
            ),
            (
                UserStatisticsRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetUserStats).s("u").frame(),
            ),
            (
                UserSearchRequest {
                    username: "u".into(),
                    token: 2,
                    search_text: "q".into(),
                }
                .to_bytes(),
                W::code(ServerCode::UserSearch).s("u").u32(2).s("q").frame(),
            ),
            (
                LikedInterestAddCommand {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::LikedInterestAdd).s("i").frame(),
            ),
            (
                LikedInterestRemoveCommand {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::LikedInterestRemove).s("i").frame(),
            ),
            (
                PersonalRecommendationsRequest {}.to_bytes(),
                W::code(ServerCode::GetPersonalRecommendations).frame(),
            ),
            (
                GlobalRecommendationsRequest {}.to_bytes(),
                W::code(ServerCode::GetGlobalRecommendations).frame(),
            ),
            (
                UserInterestsRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetUserInterests).s("u").frame(),
            ),
            (
                RoomListRequest {}.to_bytes(),
                W::code(ServerCode::RoomList).frame(),
            ),
            (
                HaveNoParentsCommand {
                    have_no_parents: true,
                }
                .to_bytes(),
                W::code(ServerCode::HaveNoParents).u8(1).frame(),
            ),
            (
                ParentsIpCommand {
                    ip_address: Some(Ipv4Addr::new(1, 2, 3, 4)),
                }
                .to_bytes(),
                W::code(ServerCode::ParentsIP)
                    .ip(Ipv4Addr::new(1, 2, 3, 4))
                    .frame(),
            ),
            (
                ParentsIpCommand { ip_address: None }.to_bytes(),
                W::code(ServerCode::ParentsIP).frame(),
            ),
            (
                CheckPrivilegesRequest {}.to_bytes(),
                W::code(ServerCode::CheckPrivileges).frame(),
            ),
            (
                AcceptChildrenCommand { accepted: false }.to_bytes(),
                W::code(ServerCode::AcceptChildren).u8(0).frame(),
            ),
            (
                WishlistSearchRequest {
                    token: 1,
                    search_text: "w".into(),
                }
                .to_bytes(),
                W::code(ServerCode::WishlistSearch).u32(1).s("w").frame(),
            ),
            (
                PersonalSimilarUsersRequest {}.to_bytes(),
                W::code(ServerCode::GetPersonalSimilarUsers).frame(),
            ),
            (
                InterestRecommendationsRequest {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetInterestRecommendations)
                    .s("i")
                    .frame(),
            ),
            (
                InterestSimilarUsersRequest {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::GetInterestSimilarUsers).s("i").frame(),
            ),
            (
                SetRoomTickerCommand {
                    room_name: "r".into(),
                    message: "t".into(),
                }
                .to_bytes(),
                W::code(ServerCode::SetRoomTicker).s("r").s("t").frame(),
            ),
            (
                HatedInterestAddCommand {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::HatedInterestAdd).s("i").frame(),
            ),
            (
                HatedInterestRemoveCommand {
                    interest: "i".into(),
                }
                .to_bytes(),
                W::code(ServerCode::HatedInterestRemove).s("i").frame(),
            ),
            (
                RoomSearchRequest {
                    room_name: "r".into(),
                    token: 3,
                    search_text: "q".into(),
                }
                .to_bytes(),
                W::code(ServerCode::RoomSearch).s("r").u32(3).s("q").frame(),
            ),
            (
                SendUploadSpeedCommand { speed: 1000 }.to_bytes(),
                W::code(ServerCode::SendUploadSpeed).u32(1000).frame(),
            ),
            (
                UserPrivilegesRequest {
                    username: "u".into(),
                }
                .to_bytes(),
                W::code(ServerCode::UserPrivileges).s("u").frame(),
            ),
            (
                GivePrivilegesCommand {
                    username: "u".into(),
                    days: 30,
                }
                .to_bytes(),
                W::code(ServerCode::GivePrivileges).s("u").u32(30).frame(),
            ),
            (
                AcknowledgePrivilegeNotificationCommand { id: 4 }.to_bytes(),
                W::code(ServerCode::AcknowledgeNotifyPrivileges)
                    .u32(4)
                    .frame(),
            ),
            (
                BranchLevelCommand { level: 2 }.to_bytes(),
                W::code(ServerCode::BranchLevel).u32(2).frame(),
            ),
            (
                BranchRootCommand {
                    username: "root".into(),
                }
                .to_bytes(),
                W::code(ServerCode::BranchRoot).s("root").frame(),
            ),
            (
                ChildDepthCommand { depth: 6 }.to_bytes(),
                W::code(ServerCode::ChildDepth).u32(6).frame(),
            ),
            (
                PrivateRoomDropMembershipCommand {
                    room_name: "r".into(),
                }
                .to_bytes(),
                W::code(ServerCode::PrivateRoomDropMembership)
                    .s("r")
                    .frame(),
            ),
            (
                PrivateRoomDropOwnershipCommand {
                    room_name: "r".into(),
                }
                .to_bytes(),
                W::code(ServerCode::PrivateRoomDropOwnership).s("r").frame(),
            ),
            (
                StartPublicChatCommand {}.to_bytes(),
                W::code(ServerCode::AskPublicChat).frame(),
            ),
            (
                StopPublicChatCommand {}.to_bytes(),
                W::code(ServerCode::StopPublicChat).frame(),
            ),
            (
                CannotConnect {
                    token: 8,
                    username: Some("u".into()),
                }
                .to_bytes(),
                W::code(ServerCode::CannotConnect).u32(8).s("u").frame(),
            ),
            (
                CannotConnect {
                    token: 8,
                    username: None,
                }
                .to_bytes(),
                W::code(ServerCode::CannotConnect).u32(8).frame(),
            ),
        ];
        for (i, (actual, expected)) in cases.into_iter().enumerate() {
            assert_eq!(actual, expected, "case {i}");
        }
    }

    #[test]
    fn bidirectional_round_trips() {
        let m = PrivateRoomAddUser {
            room_name: "r".into(),
            username: "u".into(),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::PrivateRoomAddUser(m)
        );
        let m = PrivateRoomRemoveUser {
            room_name: "r".into(),
            username: "u".into(),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::PrivateRoomRemoveUser(m)
        );
        let m = PrivateRoomAddOperator {
            room_name: "r".into(),
            username: "u".into(),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::PrivateRoomAddOperator(m)
        );
        let m = PrivateRoomRemoveOperator {
            room_name: "r".into(),
            username: "u".into(),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::PrivateRoomRemoveOperator(m)
        );
        let m = PrivateRoomToggle {
            accept_invitations: true,
        };
        assert_eq!(
            m.to_bytes(),
            W::code(ServerCode::PrivateRoomToggle).u8(1).frame()
        );
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::PrivateRoomToggle(m)
        );
        let m = NewPassword {
            password: "pw".into(),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::NewPassword(m)
        );
        assert_eq!(
            ServerIncoming::decode(ServerPing {}.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::Ping
        );
        let m = CannotConnect {
            token: 1,
            username: Some("u".into()),
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::CannotConnect(m)
        );
        let m = CannotConnect {
            token: 1,
            username: None,
        };
        assert_eq!(
            ServerIncoming::decode(m.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::CannotConnect(m)
        );

        let frame = GivePrivilegesCommand {
            username: "u".into(),
            days: 3,
        }
        .to_bytes();
        let mut r = reader_after_code(&frame);
        assert_eq!(GivePrivilegesCommand::decode(&mut r).unwrap().days, 3);
    }

    #[test]
    fn peer_address_and_connect_to_peer() {
        let ip = Ipv4Addr::new(127, 0, 0, 1);
        let mut w = W::code(ServerCode::GetPeerAddress);
        w.s("bob").ip(ip).u32(2234);
        assert_eq!(
            incoming(&w),
            ServerIncoming::PeerAddress(UserAddressResponse {
                username: "bob".into(),
                ip_address: ip,
                port: 2234
            })
        );

        let mut w = W::code(ServerCode::ConnectToPeer);
        w.s("bob").s("F").ip(ip).u32(2234).u32(42).u8(1);
        match incoming(&w) {
            ServerIncoming::ConnectToPeer(c) => {
                assert_eq!(c.username, "bob");
                assert_eq!(c.connection_type, "F");
                assert_eq!(c.endpoint(), SocketAddrV4::new(ip, 2234));
                assert_eq!(c.token, 42);
                assert!(c.is_privileged);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn watch_user_status_stats() {
        let mut w = W::code(ServerCode::WatchUser);
        w.s("bob")
            .u8(1)
            .u32(2)
            .u32(100)
            .u64(5)
            .u32(6)
            .u32(7)
            .s("US");
        match incoming(&w) {
            ServerIncoming::WatchUser(r) => {
                assert!(r.exists);
                let d = r.user_data.unwrap();
                assert_eq!(d.status, UserPresence::Online);
                assert_eq!(
                    (
                        d.average_speed,
                        d.upload_count,
                        d.file_count,
                        d.directory_count
                    ),
                    (100, 5, 6, 7)
                );
                assert_eq!(d.country_code.as_deref(), Some("US"));
            }
            other => panic!("{other:?}"),
        }
        let mut w = W::code(ServerCode::WatchUser);
        w.s("bob").u8(0);
        assert_eq!(
            incoming(&w),
            ServerIncoming::WatchUser(WatchUserResponse {
                username: "bob".into(),
                exists: false,
                user_data: None
            })
        );

        let mut w = W::code(ServerCode::GetStatus);
        w.s("bob").u32(1).u8(1);
        assert_eq!(
            incoming(&w),
            ServerIncoming::UserStatus(UserStatus {
                username: "bob".into(),
                presence: UserPresence::Away,
                is_privileged: true
            })
        );

        let mut w = W::code(ServerCode::GetUserStats);
        w.s("bob").u32(10).u64(11).u32(12).u32(13);
        assert_eq!(
            incoming(&w),
            ServerIncoming::UserStatistics(UserStatistics {
                username: "bob".into(),
                average_speed: 10,
                upload_count: 11,
                file_count: 12,
                directory_count: 13
            })
        );

        let mut w = W::code(ServerCode::UserPrivileges);
        w.s("bob").u8(1);
        assert_eq!(
            incoming(&w),
            ServerIncoming::UserPrivileges(UserPrivilegeResponse {
                username: "bob".into(),
                is_privileged: true
            })
        );
    }

    #[test]
    fn private_message() {
        let mut w = W::code(ServerCode::PrivateMessage);
        w.u32(5).u32(1_600_000_000).s("server").s("hello").u8(0);
        match incoming(&w) {
            ServerIncoming::PrivateMessage(pm) => {
                assert_eq!(pm.id, 5);
                assert_eq!(pm.timestamp, timestamp_from_secs(1_600_000_000));
                assert!(pm.is_admin);
                assert!(pm.replayed);
                assert_eq!(pm.message, "hello");
                assert_eq!(pm.to_bytes(), w.frame());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rooms() {
        let room = RoomData {
            name: "room".into(),
            users: vec![
                UserData {
                    username: "a".into(),
                    status: UserPresence::Online,
                    average_speed: 1,
                    upload_count: 2,
                    file_count: 3,
                    directory_count: 4,
                    country_code: Some("US".into()),
                    slots_free: Some(1),
                },
                UserData {
                    username: "b".into(),
                    status: UserPresence::Away,
                    average_speed: 5,
                    upload_count: 6,
                    file_count: 7,
                    directory_count: 8,
                    country_code: Some("DE".into()),
                    slots_free: Some(0),
                },
            ],
            is_private: true,
            owner: Some("a".into()),
            operators: vec!["b".into()],
        };
        assert_eq!(
            ServerIncoming::decode(room.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::JoinRoom(room.clone())
        );
        let public = RoomData {
            is_private: false,
            owner: None,
            operators: vec![],
            ..room
        };
        assert_eq!(
            ServerIncoming::decode(public.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::JoinRoom(public)
        );

        let mut w = W::code(ServerCode::UserJoinedRoom);
        w.s("room")
            .s("c")
            .u32(2)
            .u32(9)
            .u64(10)
            .u32(11)
            .u32(12)
            .u32(1)
            .s("FR");
        match incoming(&w) {
            ServerIncoming::UserJoinedRoom(n) => {
                assert_eq!(n.room_name, "room");
                assert_eq!(n.user_data.username, "c");
                assert_eq!(n.user_data.slots_free, Some(1));
                assert_eq!(n.user_data.country_code.as_deref(), Some("FR"));
                assert_eq!(n.user_data.upload_count, 10);
            }
            other => panic!("{other:?}"),
        }

        let mut w = W::code(ServerCode::UserLeftRoom);
        w.s("room").s("c");
        assert_eq!(
            incoming(&w),
            ServerIncoming::UserLeftRoom(UserLeftRoomNotification {
                room_name: "room".into(),
                username: "c".into()
            })
        );

        let mut w = W::code(ServerCode::LeaveRoom);
        w.s("room");
        assert_eq!(
            incoming(&w),
            ServerIncoming::LeaveRoom(LeaveRoomResponse {
                room_name: "room".into()
            })
        );

        let mut w = W::code(ServerCode::SayInChatRoom);
        w.s("room").s("a").s("msg");
        let msg = RoomMessage {
            room: "room".into(),
            username: "a".into(),
            message: "msg".into(),
        };
        assert_eq!(incoming(&w), ServerIncoming::RoomMessage(msg.clone()));
        let mut w = W::code(ServerCode::PublicChat);
        w.s("room").s("a").s("msg");
        assert_eq!(incoming(&w), ServerIncoming::PublicChat(msg));

        let mut w = W::code(ServerCode::CannotJoinRoom);
        w.s("room");
        assert_eq!(
            incoming(&w),
            ServerIncoming::CannotJoinRoom(CannotJoinRoomNotification {
                room_name: "room".into()
            })
        );
    }

    #[test]
    fn room_list() {
        let mut w = W::code(ServerCode::RoomList);
        // public
        w.u32(2).s("a").s("b").u32(2).u32(10).u32(20);
        // owned
        w.u32(1).s("o").u32(1).u32(3);
        // private
        w.u32(1).s("p").u32(1).u32(4);
        // moderated names
        w.u32(1).s("m");
        let expected = RoomList {
            public: vec![RoomInfo::with_count("a", 10), RoomInfo::with_count("b", 20)],
            owned: vec![RoomInfo::with_count("o", 3)],
            private: vec![RoomInfo::with_count("p", 4)],
            moderated_rooms: vec![RoomInfo::with_count("m", 0)],
        };
        assert_eq!(incoming(&w), ServerIncoming::RoomList(expected.clone()));
        assert_eq!(expected.to_bytes(), w.frame());
    }

    #[test]
    fn private_room_lists_and_strings() {
        let mut w = W::code(ServerCode::PrivateRoomUsers);
        w.s("p").u32(2).s("a").s("b");
        assert_eq!(
            incoming(&w),
            ServerIncoming::PrivateRoomUsers(RoomInfo::with_users(
                "p",
                vec!["a".into(), "b".into()]
            ))
        );
        let mut w = W::code(ServerCode::PrivateRoomOwned);
        w.s("p").u32(1).s("op");
        assert_eq!(
            incoming(&w),
            ServerIncoming::PrivateRoomOwned(RoomInfo::with_users("p", vec!["op".into()]))
        );

        for (code, make) in [
            (
                ServerCode::PrivateRoomAdded,
                ServerIncoming::PrivateRoomAdded as fn(String) -> ServerIncoming,
            ),
            (
                ServerCode::PrivateRoomRemoved,
                ServerIncoming::PrivateRoomRemoved,
            ),
            (
                ServerCode::PrivateRoomOperatorAdded,
                ServerIncoming::PrivateRoomOperatorAdded,
            ),
            (
                ServerCode::PrivateRoomOperatorRemoved,
                ServerIncoming::PrivateRoomOperatorRemoved,
            ),
            (
                ServerCode::GlobalAdminMessage,
                ServerIncoming::GlobalAdminMessage,
            ),
            (
                ServerCode::AddPrivilegedUser,
                ServerIncoming::AddPrivilegedUser,
            ),
        ] {
            let mut w = W::code(code);
            w.s("x");
            assert_eq!(incoming(&w), make("x".into()));
        }
    }

    #[test]
    fn tickers() {
        let mut w = W::code(ServerCode::RoomTickers);
        w.s("r").u32(2).s("a").s("t1").s("b").s("t2");
        match incoming(&w) {
            ServerIncoming::RoomTickers(t) => {
                assert_eq!(t.room_name, "r");
                assert_eq!(
                    t.tickers[1],
                    RoomTicker {
                        username: "b".into(),
                        message: "t2".into()
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        let mut w = W::code(ServerCode::RoomTickerAdd);
        w.s("r").s("a").s("t");
        assert_eq!(
            incoming(&w),
            ServerIncoming::RoomTickerAdd(RoomTickerAddedNotification {
                room_name: "r".into(),
                ticker: RoomTicker {
                    username: "a".into(),
                    message: "t".into()
                }
            })
        );
        let mut w = W::code(ServerCode::RoomTickerRemove);
        w.s("r").s("a");
        assert_eq!(
            incoming(&w),
            ServerIncoming::RoomTickerRemove(RoomTickerRemovedNotification {
                room_name: "r".into(),
                username: "a".into()
            })
        );
    }

    #[test]
    fn interests_and_recommendations() {
        let mut w = W::code(ServerCode::GetPersonalRecommendations);
        w.u32(1).s("rock").i32(5).u32(1).s("pop").i32(-2);
        let expected = RecommendationsResponse {
            recommendations: vec![Recommendation {
                item: "rock".into(),
                score: 5,
            }],
            unrecommendations: vec![Recommendation {
                item: "pop".into(),
                score: -2,
            }],
        };
        assert_eq!(
            incoming(&w),
            ServerIncoming::PersonalRecommendations(expected.clone())
        );
        assert_eq!(
            expected.to_bytes(ServerCode::GetPersonalRecommendations),
            w.frame()
        );
        let global = expected.to_bytes(ServerCode::GetGlobalRecommendations);
        assert_eq!(
            ServerIncoming::decode(global.slice(4..)).unwrap(),
            ServerIncoming::GlobalRecommendations(expected)
        );

        let interests = UserInterests {
            username: "u".into(),
            liked: vec!["a".into()],
            hated: vec!["b".into(), "c".into()],
        };
        assert_eq!(
            ServerIncoming::decode(interests.to_bytes().slice(4..)).unwrap(),
            ServerIncoming::UserInterests(interests)
        );

        let mut w = W::code(ServerCode::GetPersonalSimilarUsers);
        w.u32(1).s("u").i32(3);
        assert_eq!(
            incoming(&w),
            ServerIncoming::PersonalSimilarUsers(PersonalSimilarUsersResponse {
                users: vec![SimilarUser {
                    username: "u".into(),
                    rating: 3
                }]
            })
        );

        let mut w = W::code(ServerCode::GetInterestRecommendations);
        w.s("rock").u32(1).s("metal").i32(7);
        assert_eq!(
            incoming(&w),
            ServerIncoming::InterestRecommendations(InterestRecommendationsResponse {
                interest: "rock".into(),
                recommendations: vec![Recommendation {
                    item: "metal".into(),
                    score: 7
                }]
            })
        );

        let mut w = W::code(ServerCode::GetInterestSimilarUsers);
        w.s("rock").u32(2).s("a").s("b");
        assert_eq!(
            incoming(&w),
            ServerIncoming::InterestSimilarUsers(InterestSimilarUsersResponse {
                interest: "rock".into(),
                usernames: vec!["a".into(), "b".into()]
            })
        );
    }

    #[test]
    fn integer_messages() {
        type Decoded = fn(u32) -> ServerIncoming;
        let cases: [(ServerCode, Decoded); 8] = [
            (ServerCode::ParentMinSpeed, ServerIncoming::ParentMinSpeed),
            (
                ServerCode::ParentSpeedRatio,
                ServerIncoming::ParentSpeedRatio,
            ),
            (
                ServerCode::ParentInactivityTimeout,
                ServerIncoming::ParentInactivityTimeout,
            ),
            (
                ServerCode::SearchInactivityTimeout,
                ServerIncoming::SearchInactivityTimeout,
            ),
            (
                ServerCode::MinimumParentsInCache,
                ServerIncoming::MinimumParentsInCache,
            ),
            (
                ServerCode::DistributedAliveInterval,
                ServerIncoming::DistributedAliveInterval,
            ),
            (ServerCode::CheckPrivileges, ServerIncoming::CheckPrivileges),
            (
                ServerCode::WishlistInterval,
                ServerIncoming::WishlistInterval,
            ),
        ];
        for (code, make) in cases {
            let mut w = W::code(code);
            w.u32(720);
            assert_eq!(incoming(&w), make(720));
        }
        let mut w = W::code(ServerCode::WishlistInterval);
        w.u32(720);
        assert_eq!(
            incoming(&w).wishlist_interval(),
            Some(Duration::from_secs(720))
        );
    }

    #[test]
    fn privileges_and_lists() {
        let mut w = W::code(ServerCode::NotifyPrivileges);
        w.u32(3).s("u");
        assert_eq!(
            incoming(&w),
            ServerIncoming::NotifyPrivileges(PrivilegeNotification {
                id: Some(3),
                username: "u".into()
            })
        );
        let mut w = W::code(ServerCode::PrivilegedUsers);
        w.u32(2).s("a").s("b");
        assert_eq!(
            incoming(&w),
            ServerIncoming::PrivilegedUsers(vec!["a".into(), "b".into()])
        );
        let mut w = W::code(ServerCode::ExcludedSearchPhrases);
        w.u32(1).s("bad");
        assert_eq!(
            incoming(&w),
            ServerIncoming::ExcludedSearchPhrases(vec!["bad".into()])
        );
    }

    #[test]
    fn distributed() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let mut w = W::code(ServerCode::NetInfo);
        w.u32(1).s("parent").ip(ip).u32(4000);
        let expected = NetInfoNotification {
            parents: vec![NetInfoParent {
                username: "parent".into(),
                ip_address: ip,
                port: 4000,
            }],
        };
        assert_eq!(incoming(&w), ServerIncoming::NetInfo(expected.clone()));
        assert_eq!(expected.to_bytes(), w.frame());

        let mut w = W::code(ServerCode::EmbeddedMessage);
        w.u8(3).s("user").u32(9).s("query");
        match incoming(&w) {
            ServerIncoming::EmbeddedMessage(e) => {
                assert_eq!(e.distributed_code, 3);
                let mut inner = W::default();
                inner.u8(3).s("user").u32(9).s("query");
                assert_eq!(e.distributed_body(), inner.body());
                assert_eq!(e.distributed_frame(), inner.frame());
                assert_eq!(e.to_bytes(), w.frame());
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(
            incoming(&W::code(ServerCode::DistributedReset)),
            ServerIncoming::DistributedReset
        );
        assert_eq!(
            incoming(&W::code(ServerCode::KickedFromServer)),
            ServerIncoming::KickedFromServer
        );
    }

    #[test]
    fn server_search_request() {
        let mut w = W::code(ServerCode::FileSearch);
        w.s("bob").u32(77).s("some query");
        assert_eq!(
            incoming(&w),
            ServerIncoming::FileSearch(ServerSearchRequest {
                username: "bob".into(),
                token: 77,
                query: "some query".into()
            })
        );
    }

    #[test]
    fn unknown_code() {
        let mut w = W::default();
        w.u32(9999).u8(1).u8(2);
        assert_eq!(
            incoming(&w),
            ServerIncoming::Unknown {
                code: 9999,
                body: Bytes::from_static(&[1, 2])
            }
        );
    }

    #[test]
    fn truncated_is_error() {
        let mut w = W::code(ServerCode::GetPeerAddress);
        w.s("bob");
        assert!(ServerIncoming::decode(w.body()).is_err());
    }
}
