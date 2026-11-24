# soulseek crate runtime internals (phase 2 contract)

Public API: `crates/soulseek/src/client/mod.rs` (signatures + rustdoc contracts).
Protocol: `crates/soulseek/src/protocol/*`. This document fixes the *internal*
crate-private interfaces so four modules can be written in parallel.

## Ownership

| owner | files | public `SoulseekClient` methods it implements |
|---|---|---|
| **Core** | `net/mod.rs`, `net/connection.rs`, `net/message_connection.rs`, `net/listener.rs`, `net/waiter.rs`, `net/rate.rs`, `client/mod.rs` (ClientInner, delegation cleanup), `client/server.rs` (server connection + `ServerIncoming` dispatch), `client/server_ops.rs` | connect, disconnect, reconfigure, ping_server, change_password, set_status, set_shared_counts, send_upload_speed, start/stop_public_chat, get_user_endpoint, get_user_status, get_user_statistics, get_user_privileged, watch_user, unwatch_user, privileges, private messages, rooms, private rooms, interests/recommendations |
| **Peers** | `client/peers.rs` (PeerManager), `client/peer_handler.rs` | browse, get_directory_contents, get_user_info, connect_to_user |
| **Transfers** | `client/transfers.rs` (TransferManager) | enqueue_download, enqueue_upload, get_download_place_in_queue |
| **Search** | `client/search.rs` (SearchManager + responder), `client/distributed.rs` (DistributedManager) | search |

Public methods are implemented as `impl SoulseekClient { pub async fn ... }` blocks **inside the
owner's own file**, copying the signature + rustdoc from `client/mod.rs`. Core deletes every
`todo!("phase 2")` method from `client/mod.rs` (all of them move out; Core moves its own to
`client/server_ops.rs`) and removes the `#![allow(unused_variables, dead_code)]`.
`client/mod.rs` declares: `mod server; mod server_ops; mod peers; mod peer_handler; mod transfers; mod search; mod distributed;`
and `crate::lib.rs` gets `pub(crate) mod net;` (Core may edit lib.rs for this one line).

## Shared internal types (Core)

```rust
// net/connection.rs — one TCP connection with Soulseek framing helpers
pub(crate) struct Connection { .. }
impl Connection {
    pub async fn connect(addr: SocketAddr, opts: &ConnectionOptions) -> Result<Self>; // connect_timeout, optional SOCKS5 proxy
    pub fn from_accepted(stream: TcpStream, remote: SocketAddr, opts: &ConnectionOptions) -> Self;
    pub fn remote(&self) -> SocketAddr;
    pub async fn read_frame(&mut self) -> Result<Bytes>;           // u32 len + body; returns body; inactivity timeout
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<()>;
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;
    pub fn into_parts(self) -> (TcpStream, BytesMut);              // raw stream + already-buffered bytes (file transfer)
}

// net/message_connection.rs — framed connection driven by tasks
#[derive(Clone)] pub(crate) struct MessageConnection { .. }
impl MessageConnection {
    /// Spawns reader + writer tasks. Incoming frame bodies (code + payload, no length) are
    /// delivered on the receiver, which ends when the connection closes.
    pub fn spawn(conn: Connection, username: Option<String>) -> (Self, mpsc::Receiver<Bytes>);
    pub async fn send(&self, frame: Bytes) -> Result<()>;  // frame = MessageBuilder::build() output
    pub fn close(&self);
    pub fn is_closed(&self) -> bool;
    pub async fn closed(&self);
    pub fn remote(&self) -> SocketAddr;
    pub fn id(&self) -> u64;                                // unique per connection
    pub fn username(&self) -> Option<String>;
}

// net/waiter.rs — request/response correlation
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WaitKey(pub &'static str, pub String);  // (message kind, discriminator e.g. username/token)
pub(crate) struct Waiter { .. }
impl Waiter {
    pub fn wait<T: Send + 'static>(&self, key: WaitKey, timeout: Option<Duration>) -> impl Future<Output = Result<T>> + Send + 'static; // registers immediately
    pub fn wait_indefinitely<T: Send + 'static>(&self, key: WaitKey) -> impl Future<Output = Result<T>> + Send + 'static;
    pub fn complete<T: Send + 'static>(&self, key: &WaitKey, value: T) -> bool;  // completes oldest waiter for key
    pub fn throw(&self, key: &WaitKey, error: Error) -> bool;
    pub fn cancel_all(&self);                                // on disconnect: Error::Connection
}

// net/rate.rs — shared token buckets for speed limits (bytes/sec, 0 = unlimited)
pub(crate) struct RateLimiter { .. }
impl RateLimiter {
    pub fn new(bytes_per_second: u64) -> Self;
    pub fn set_rate(&self, bytes_per_second: u64);
    pub async fn acquire(&self, wanted: usize) -> usize;     // grants 1..=wanted bytes
}

// client/mod.rs — ClientInner (Core owns the struct; other modules own their field types)
pub(crate) struct ClientInner {
    // existing fields (options, resolvers, events sender, session, token counter, transfer snapshots) +
    pub(crate) waiter: Waiter,
    pub(crate) server: crate::client::server::ServerConnection, // Core
    pub(crate) peers: crate::client::peers::PeerManager,                 // Peers: PeerManager::new()
    pub(crate) transfers: crate::client::transfers::TransferManager,     // Transfers: TransferManager::new()
    pub(crate) searches: crate::client::search::SearchManager,           // Search: SearchManager::new()
    pub(crate) distributed: crate::client::distributed::DistributedManager, // Search: DistributedManager::new()
    pub(crate) upload_limiter: RateLimiter,
    pub(crate) download_limiter: RateLimiter,
}
impl ClientInner { pub fn emit(&self, e: Event); pub async fn send_server(&self, frame: Bytes) -> Result<()>; /* errors if not connected */ }
```
`SoulseekClient` exposes `pub(crate) fn inner(&self) -> &Arc<ClientInner>` and
`pub(crate) fn from_inner(Arc<ClientInner>) -> Self`. Managers' methods take `&SoulseekClient`
(clone it into spawned tasks). `get_user_endpoint` (Core) is used by Peers.

## Hooks (caller -> callee)

Core server dispatch (`client/server.rs`):
* `ConnectToPeerResponse` -> `peers.handle_connect_to_peer(&client, resp: ConnectToPeerResponse)` (all types P/F/D)
* `CannotConnect` -> `peers.handle_cannot_connect(&client, token: u32, username: Option<String>)`
* `ServerSearchRequest`/`FileSearch` received from server, `EmbeddedMessage`, `NetInfo`, `ParentMinSpeed`, `ParentSpeedRatio`,
  `ParentInactivityTimeout`, `SearchInactivityTimeout`, `MinimumParentsInCache`, `DistributedAliveInterval`,
  `DistributedReset`, `WishlistInterval` -> `distributed.handle_server_message(&client, msg: &ServerIncoming)`
  (DistributedManager forwards search requests to `searches.respond(&client, username, token, query)`)
* On login success: `distributed.on_logged_in(&client)`; on disconnect: `peers.on_disconnected()`, `distributed.on_disconnected()`, `searches.on_disconnected()`, `transfers.on_disconnected()` (sync fns).

Core listener (`net/listener.rs`), after reading the first init frame:
* `PeerInit { connection_type: "P" }` -> `peers.handle_incoming_peer_init(&client, username, token, conn: Connection)`
* `PeerInit { "F" }` -> `transfers.handle_incoming_transfer_connection(&client, username, conn: Connection)` (reads the u32 token itself)
* `PeerInit { "D" }` -> `distributed.handle_incoming_child(&client, username, conn: Connection)`
* `PierceFirewall { token }` -> `peers.handle_pierce_firewall(&client, token, conn: Connection)` (PeerManager owns the solicitation registry for P, F and D)

Peers provides:
* `peers.get_message_connection(&client, username: &str, cancel: &CancellationToken) -> Result<MessageConnection>`
  (cached per user; direct connect to `get_user_endpoint`, with the indirect ConnectToPeer solicitation + PierceFirewall started only if the direct dial fails or has not connected within a 1 s head start (at once for users whose last direct dial failed); sends PeerInit P on direct)
* `peers.get_transfer_connection(&client, username: &str, token: u32, cancel: &CancellationToken) -> Result<Connection>` (outgoing 'F', after PeerInit F / PierceFirewall; caller writes/reads the token + offset)
* `peers.get_distributed_connection(&client, username: &str, endpoint: SocketAddr, cancel: &CancellationToken) -> Result<Connection>` (outgoing 'D' to a candidate parent)
* `peers.send_to(&client, username: &str, frame: Bytes, cancel: &CancellationToken) -> Result<()>` convenience
* Peer message dispatch in `peer_handler.rs`: TransferRequest, TransferResponse, QueueDownload, PlaceInQueueRequest,
  PlaceInQueueResponse, UploadFailed, UploadDenied -> `transfers.handle_peer_message(&client, username: &str, remote: SocketAddr, msg: PeerIncoming)`;
  SearchResponse -> `searches.handle_search_response(&client, response: SearchResponse)`;
  Browse/Info/FolderContents requests answered via `Resolvers`; responses complete waiter keys.

Transfers provides: `handle_incoming_transfer_connection`, `handle_peer_message`, `on_disconnected`, `enqueue_download`, `enqueue_upload`, `get_download_place_in_queue`; keeps `ClientInner` transfer snapshot maps (used by `downloads()`/`uploads()`) up to date and raises TransferStateChanged/TransferProgressUpdated/DownloadDenied/DownloadFailed events; uses `upload_limiter`/`download_limiter` and `TransferOptions.governor`.

Search provides: `search` (public), `handle_search_response`, `respond` (incoming search -> `Resolvers::search` -> deliver `SearchResponse` frame via `peers.send_to`; raise SearchRequestReceived / SearchResponseDelivered / SearchResponseDeliveryFailed; dedupe per `deduplicate_search_requests`), DistributedManager (parent selection from NetInfo candidates, branch level/root/child depth, HaveNoParents/AcceptChildren/BranchLevel/BranchRoot/ChildDepth server commands, children up to `distributed_child_limit`, forwarding search requests to children, ping, Distributed* events, `DistributedNetworkInfo` snapshot via `distributed.info()`).

## Testing

Each module gets unit tests with an in-process fake (tokio `TcpListener` on 127.0.0.1:0 acting as
server/peer). Core additionally provides `#[cfg(test)] pub(crate) mod test_support` in `net/mod.rs`
with a fake server helper (accept, read frames, write frames) usable by other modules' tests.
