# slskr architecture

slskr is a Soulseek client daemon with a web UI. It began as a port of
[slskd](https://github.com/slskd/slskd) and keeps its features, but its HTTP API, live
events and configuration are its own. The Soulseek network library is a Rust port of
[Soulseek.NET](https://github.com/jpdillingham/Soulseek.NET) behaviour (wire-compatible
with real Soulseek clients and servers, not file-by-file). Reference sources are cloned
(read-only, git-ignored) under `ref/Soulseek.NET` and `ref/slskd`.

Toolchain: `nix develop` (flake.nix) provides stable Rust (edition 2024) with the
`wasm32-unknown-unknown` target, `trunk` and `wasm-bindgen-cli` 0.2.130 (pinned in
`crates/slskr-web`; the Dockerfile downloads the same versions).

## Crates

| crate | kind | purpose |
|---|---|---|
| `crates/soulseek` | lib | async (tokio) Soulseek network client |
| `crates/slskr-types` | lib | serde types of the HTTP API and live events; shared by daemon and UI |
| `crates/slskr` | bin `slskr` (+ lib) | the daemon: axum HTTP API, live events WebSocket, services, sqlite, the embedded UI |
| `crates/slskr-web` | bin (wasm) | Leptos 0.8 CSR web UI (derived from the slskd React UI), built with trunk |

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

## slskr-types crate

The request and response types of the HTTP API and the live events. JSON is `snake_case`
(fields and enum values), timestamps are RFC 3339 UTC, durations integer milliseconds in
`*_ms` fields, and absent values `null`; see [API.md](API.md#conventions). States are
plain enums (`TransferState`, `SearchState`, `ConnectionState`); the daemon converts the
`soulseek` crate's bit flags at its boundary. Modules: `application` (State, version),
`config` (the merged configuration, as in `slskr.toml`), `settings` (`/api/settings` view:
values, pinned and stored keys, secrets set, API keys), `setup` (first-run setup, login test,
directory picker), `error` (`{"error"}` body), `session`,
`server`, `logs`, `events`, `telemetry`, `search`, `transfers`, `users`, `conversations`,
`rooms`, `shares`, `files`, `realtime` (live event protocol). No I/O, compiles to wasm.

## Realtime

One plain WebSocket at `{url_base}/api/events` carries every live update as JSON text
frames of `slskr_types::realtime::Event` (`{"type": "...", "data": ...}`). The client
subscribes to topics (`state`, `settings`, `searches`, `logs` for administrators,
`metrics`, `transfers`, `conversations`, `rooms`); each subscription first delivers that topic's snapshot, then changes. Services
publish to an internal `EventBus`; the socket fans it out per connection. The upgrade is
authenticated like any route (session cookie or `X-API-Key`, same-origin check for
cookies). See [daemon internals](DAEMON.md#live-events-srcrealtime) for the message list,
coalescing, limits and close codes.

The UI (`services/events.rs`) opens the connection after the session check, lets components
register per-topic handlers that unsubscribe on cleanup, exposes the connection status,
reconnects after 0 s, 2 s, 10 s, then every 30 s (resubscribing, which brings fresh
snapshots), and returns to the sign-in form on close code 4401.

## HTTP API

`{url_base}/api/...`, listed route by route in [API.md](API.md) (a test keeps that list and
the router in step). Errors are `{"error": "<message>"}` with a 4xx/5xx status.
Auth: server-side sessions behind an `HttpOnly`, `SameSite=Strict` cookie (`slskr_session`,
set by `POST /api/session`, cleared by `DELETE`), API keys (`X-API-Key`), or disabled
auth (passthrough). Browsers send the cookie on same-origin fetches and the events WebSocket
upgrades; see [daemon internals](DAEMON.md#authentication).

## Web UI

`crates/slskr-web` is a Leptos UI derived from slskd's React UI (`ref/slskd/src/web/src`),
calling the API through `services/api.rs` with the `slskr-types` types. Styling is slskr's
own design system (shadcn/ui-style slate tokens, light and dark): one hand-written
stylesheet, `assets/static/css/app.css`, and the components in `src/ui/` (buttons, cards,
badges, tables, modal, lucide icons, theme toggle, …), documented with examples in
[UI.md](UI.md). `src/services/` ports `src/lib/*.js`; `src/components/` holds the pages.
The app shell (`components/app.rs`) is a header with the brand, connection status, theme,
settings and logout, and a tab bar of pages; Overview (`/overview`) shows live transfer
speeds and history.

The setup wizard (`components/setup.rs`, route `/setup`) and Settings (`/settings`, also
under System; `components/system/settings*`) are slskr's own. They build their forms only from the
components in `src/ui/form.rs` (`Section`, `Field`, `TextInput`, `Toggle`, `ListEditor`,
`SaveBar`, …), which carry no logic and are styled by the "forms" section of
`assets/static/css/app.css`. `Field` provides the control's `id`,
`aria-describedby` and lock state as context. The settings page keeps a sparse draft of
changed dotted keys (`settings/model.rs`) and saves it with one `PATCH /api/settings`.
While first-run setup is required, every route shows the wizard at `/setup` and returns
to the requested page afterwards.

## Build and packaging

`trunk build --release` in `crates/slskr-web` writes the UI to the workspace `dist/`:
`index.html`, the hashed wasm/JS, the hashed stylesheet under `static/css/`, a favicon, and
`LICENSE`/`NOTICE`/`licenses/`. Building the
daemon with `SLSKR_EMBED_UI=<dist>` compiles those files into the binary (see
[daemon internals](DAEMON.md#web-ui-webrs-buildrs)), so a release is one executable.
`nix build .#slskr` builds the UI as its own derivation (`.#slskr-web`) and passes it in;
the Dockerfile does the same in two stages and copies the binary onto distroless. During
UI work, `slskr --web-dir dist` serves the trunk output from disk instead.
