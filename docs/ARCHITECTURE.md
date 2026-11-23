# slskr architecture

slskr is a Soulseek client daemon with a web UI, functionally equivalent to
[slskd](https://github.com/slskd/slskd). The Soulseek network library is a Rust
port of [Soulseek.NET](https://github.com/jpdillingham/Soulseek.NET) behaviour
(wire-compatible, not file-by-file). Reference sources are cloned (read-only,
git-ignored) under `ref/Soulseek.NET` and `ref/slskd`.

Toolchain: `nix develop` (flake.nix) provides stable Rust (edition 2024) with the
`wasm32-unknown-unknown` target, `trunk`, `wasm-bindgen-cli` 0.2.130.

## Crates

| crate | kind | purpose |
|---|---|---|
| `crates/soulseek` | lib | async (tokio) Soulseek network client |
| `crates/slskr-api` | lib | serde DTOs of the slskd HTTP/hub API; shared by daemon and UI |
| `crates/slskr` | bin `slskr` (+ lib) | the daemon: axum HTTP API, SignalR hubs, services, sqlite |
| `crates/slskr-web` | bin (wasm) | Leptos 0.8 CSR port of the slskd React UI, built with trunk |

## soulseek crate layout

```
src/lib.rs            pub mod declarations + re-exports
src/error.rs          Error (thiserror), Result<T>
src/model/            public model types (see ownership below)
src/protocol/         wire format: codec, message codes, message structs
  codec.rs            MessageBuilder / MessageReader / ProtocolError, zlib
  codes.rs            ServerCode, PeerCode, InitCode, DistributedCode
  init.rs             peer init messages (PierceFirewall, PeerInit)
  peer.rs             peer messages
  server.rs           server messages
  distributed.rs      distributed messages
src/options.rs        ClientOptions, ConnectionOptions, SearchOptions, TransferOptions, patch
src/events.rs         Event enum delivered over tokio broadcast
src/resolvers.rs      Resolvers trait (browse, directory, user info, search, enqueue, place in queue)
src/net/              tcp connections, framing, listener, peer + distributed managers
src/client/           SoulseekClient facade and its operations
```

### Codec contract (`protocol::codec`)

All integers little-endian. A frame is `u32 length` followed by `length` bytes.
Server and peer messages start with a `u32` code; init and distributed messages
start with a `u8` code. Strings are `u32 byte length` + bytes; decode as UTF-8,
falling back to ISO-8859-1 (Windows-1252 via `encoding_rs`) when invalid.

```rust
pub struct MessageBuilder { /* BytesMut */ }
impl MessageBuilder {
    pub fn new() -> Self;
    pub fn code_u32(&mut self, code: u32) -> &mut Self;   // server/peer
    pub fn code_u8(&mut self, code: u8) -> &mut Self;     // init/distributed
    pub fn u8(&mut self, v: u8) -> &mut Self;
    pub fn bool(&mut self, v: bool) -> &mut Self;          // 1 byte
    pub fn u16(&mut self, v: u16) -> &mut Self;
    pub fn u32(&mut self, v: u32) -> &mut Self;
    pub fn i32(&mut self, v: i32) -> &mut Self;
    pub fn u64(&mut self, v: u64) -> &mut Self;
    pub fn string(&mut self, v: &str) -> &mut Self;       // UTF-8
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self;       // raw, no length prefix
    pub fn ip(&mut self, v: std::net::Ipv4Addr) -> &mut Self; // reversed byte order as in protocol
    pub fn compress(&mut self) -> &mut Self;               // zlib-compress everything after the code
    pub fn build(&self) -> bytes::Bytes;                   // length-prefixed frame
}
pub struct MessageReader { /* owns Bytes of one frame body (no length prefix) */ }
impl MessageReader {
    pub fn new(body: bytes::Bytes) -> Self;
    pub fn code_u32(&mut self) -> Result<u32, ProtocolError>;
    pub fn code_u8(&mut self) -> Result<u8, ProtocolError>;
    pub fn u8/bool/u16/u32/i32/u64(&mut self) -> Result<_, ProtocolError>;
    pub fn string(&mut self) -> Result<String, ProtocolError>;
    pub fn bytes(&mut self, n: usize) -> Result<bytes::Bytes, ProtocolError>;
    pub fn ip(&mut self) -> Result<std::net::Ipv4Addr, ProtocolError>;
    pub fn remaining(&self) -> usize;
    pub fn has_remaining(&self) -> bool;
    pub fn decompress(&mut self) -> Result<(), ProtocolError>; // inflate the unread remainder in place
}
#[derive(Debug, thiserror::Error)] pub enum ProtocolError { Truncated{..}, UnknownCode{..}, Decompress(..), UnexpectedCode{..}, Invalid(String) }
```

### Message shape convention

* Outgoing message: a struct with `pub fn to_bytes(&self) -> bytes::Bytes` (full frame).
* Incoming message: a struct with `pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError>`
  where the reader is positioned **after** the code.
* Messages that travel both ways have both.
* Each module has an incoming dispatch enum, e.g. `pub enum ServerIncoming { Login(LoginResponse), ... , Unknown { code: u32, body: Bytes } }`
  with `pub fn decode(body: Bytes) -> Result<Self, ProtocolError>` reading the code itself.
* Unit tests per message: round trip or byte-exact fixtures derived from Soulseek.NET tests (`ref/Soulseek.NET/tests`).

### Model ownership (`soulseek::model`, re-exported at crate root)

| file | types |
|---|---|
| `model/file.rs` | `FileAttributeType`, `FileAttribute`, `File`, `Directory` |
| `model/peer.rs` | `SearchResponse`, `UserInfo`, `BrowseResponse` (directories + locked directories) |
| `model/server.rs` | `UserPresence`, `UserStatus`, `UserStatistics`, `UserData`, `RoomData`, `RoomInfo`, `RoomList`, `RoomTicker`, `Recommendation`, `UserInterests`, `ServerInfo`, `PrivateMessage`, `RoomMessage`, `PrivilegeNotification` |
| `model/search.rs` | `SearchQuery` (terms, exclusions, minimum size etc., parse + matches), `SearchScope`, `SearchState` (bitflags-like u32 newtype), `Search` |
| `model/transfer.rs` | `TransferDirection`, `TransferState` (bitflags-like u32 newtype matching Soulseek.NET `TransferStates`), `Transfer` |
| `model/client.rs` | `ClientState` (bitflags-like), `DistributedNetworkInfo`, `ConnectionType` |

Field naming: snake_case ports of Soulseek.NET property names; sizes `u64`, tokens `u32`, speeds `u32`, durations `std::time::Duration`, timestamps `std::time::SystemTime`.

### Client facade (`client::SoulseekClient`)

`SoulseekClient` is `Clone` (`Arc` inner). Methods are `async fn` returning
`soulseek::Result<T>`; long-running operations take a
`tokio_util::sync::CancellationToken`. Events are delivered with
`fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event>`. Incoming peer
requests are answered through `Arc<dyn Resolvers>` passed at construction. The
exact signatures live in `crates/soulseek/src/client/mod.rs`.

Architecture (re-design vs Soulseek.NET): actor-ish tasks instead of locks +
events. One server connection task (reader + writer via mpsc), a listener task,
a peer connection manager keyed by `(username, ConnectionType)`, a distributed
network manager, and a `Waiter` keyed by message kind + discriminator
(`WaitKey`) that resolves oneshot channels when matching responses arrive.

## slskr-api crate

Mirrors slskd JSON exactly (camelCase fields, enum string formats, slskd's flags
enums serialised as comma-separated strings, e.g. `"Completed, Succeeded"`).
Modules: `application` (State, version), `config` (the merged configuration, snake_case),
`settings` (`/settings` view: values, pinned keys, secrets set, API keys), `session`,
`server`, `logs`, `events`, `telemetry`, `search`, `transfers`, `users`,
`conversations`, `rooms`, `shares`, `files`. No I/O, compiles to wasm.

## Realtime

Daemon implements the ASP.NET SignalR JSON hub protocol (negotiate endpoint +
WebSocket transport, `\x1e` record separator) at `/hub/{application,logs,search,metrics}`
so both the original slskd React UI and the Rust UI work against it. The Rust UI
contains a minimal SignalR client over `web_sys::WebSocket`.

## HTTP API

`/api/v0/...` exactly as slskd (`ref/slskd/src/slskd/**/API/**Controller.cs`).
Auth: server-side sessions behind an `HttpOnly`, `SameSite=Strict` cookie (`slskr_session`,
set by `POST /api/v0/session`, cleared by `DELETE`), API keys (`X-API-Key`), or disabled
auth (passthrough). Browsers send the cookie on same-origin fetches and hub WebSocket
upgrades; see [daemon internals](DAEMON.md#authentication).

## Web UI

`crates/slskr-web` reproduces `ref/slskd/src/web/src` component-for-component in
Leptos. Styling: the compiled semantic-ui + slskd CSS is vendored as
`assets/static/css/slskd.css` (+ fonts/flags in `assets/static/media`), so UI
components must emit the same DOM/class names as semantic-ui-react.
`src/ui/` holds semantic-ui-react equivalents; `src/services/` ports `src/lib/*.js`;
`src/components/` ports `src/components/**`.
