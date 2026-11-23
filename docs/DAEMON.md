# slskr daemon internals (`crates/slskr`)

Module ownership and the contracts between modules. Every module mirrors the
corresponding slskd area under `ref/slskd/src/slskd/<Area>`.

```
src/main.rs           CLI entry (clap), calls slskr::run()
src/lib.rs            module declarations, pub async fn run(cli) (bootstrap)
src/app.rs            AppState / Core / Services / Paths (contract), Snapshots for the events socket
src/bootstrap.rs      CLI (--data-dir, --config, --no-connect), App::build + run(): load config, build core + services, bind HTTP, mount /api/events
src/config.rs         ConfigStore: layers (settings.json, slskr.toml, SLSKR_*, CLI) -> slskr_api::config::Config, pinned keys, settings API, validation, file watch
src/config/store.rs   settings.json: sparse settings tree, web password hash, API key digests (lenient load)
src/state.rs          StateStore: application State, coalesced "state" events
src/db.rs             Database: sqlite wrapper with migrations
src/http.rs           ApiError, router assembly helpers, static file serving + SPA fallback, urlBase
src/core_api.rs       /application, /session, /server, /options, /debug controllers
src/auth/             Auth (sessions, API keys, Origin checks, extractors); session.rs: SessionStore, sessions.db
src/resolvers.rs      impl soulseek::Resolvers by delegating to services
src/logging.rs        tracing setup, LogBuffer (ring buffer, "log" events), /logs controller, file logs
src/realtime/         EventBus (internal publish/subscribe) + socket.rs: the /api/events WebSocket
src/events/           EventService (raise/list/subscribe) + /events controller, events.db
src/users.rs           UserService (peer info/browse cache, group and blacklist policy) + `/users` controller
src/shares.rs          ShareService (atomic scan snapshots, persisted cache, search/browse/resolve) + `/shares` controller
src/files/            FileService + /files controller (downloads/incomplete dirs)
src/telemetry/        TelemetryService ("metrics" events, prometheus) + /telemetry controllers (reports from transfers.db)
src/transfers/        TransferService (downloads, uploads, upload queue, governor) + /transfers controller, transfers.db
src/search/           SearchService + /searches controller + "search_*" events, search.db
src/messaging/        MessagingService (conversations, rooms) + /conversations, /rooms controllers, messaging.db
```

## Shared contracts

### `config::ConfigStore` (Clone)
```rust
pub fn get(&self) -> Arc<Config>;                      // slskr_api::config::Config, current
pub fn startup(&self) -> &Config;                      // as loaded at process start
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Config>>;
pub async fn settings(&self) -> slskr_api::settings::Settings;            // GET /settings
pub async fn patch(&self, patch: serde_json::Value) -> Result<Settings, SettingsError>;
pub async fn reset(&self, dotted_key: &str) -> Result<Settings, SettingsError>;
pub async fn create_api_key(&self, CreateApiKey) -> Result<CreatedApiKey, SettingsError>;
pub async fn delete_api_key(&self, name: &str) -> Result<Settings, SettingsError>;
```
Layers, lowest precedence first: `Config::default()`, the settings store
`<data_dir>/settings.json` (written from the settings API), `slskr.toml` (in `--data-dir`,
or `--config`), `SLSKR_<SECTION>_<KEY>` environment variables, then CLI flags
(`--no-connect` sets `soulseek.connect = false`). The layers are merged as TOML tables
into the one `Config`. Every leaf key set by the file, the environment or the CLI is
*pinned*: `Settings.pinned` lists it and `patch`/`reset` refuse it (409). Environment names
are matched against the serialized defaults, so keys containing `_` and nested tables
(`SLSKR_UPLOADS_LIMITS_QUEUED_FILES`) need no extra separator; values are typed like the
default (booleans, integers, comma-separated lists, strings). `[[web.api_keys]]` and
user-defined `[groups.<name>]` cannot come from the environment. `SLSKR_DATA_DIR`,
`SLSKR_CONFIG` and `SLSKR_CONTENT_PATH` are process settings, not keys; other `SLSKR_*`
names outside the configuration sections are ignored, while an unknown key inside a section
is an error. [CONFIGURATION.md](CONFIGURATION.md) lists every key; a test keeps it complete.

The file is strict: every section is `#[serde(default, deny_unknown_fields)]` and errors
name the key (`web.api_keys[0].key must be between 16 and 255 characters`). Loading
normalizes `web.url_base` (`/` or `/prefix`) and `metrics.path`, resolves relative paths
(`downloads.*directory`, `soulseek.picture`, `shares.directories`) against the data
directory, then validates. The daemon never writes the file. It is polled every second; a
valid change is published to subscribers, an invalid one is logged and ignored.

The store is lenient (`config/store.rs`): `{"version": 1, "settings": {<sparse Config
tree>}, "web_password_hash": "$argon2id$...", "api_keys": [{name, sha256, role, allow}]}`.
`settings` holds only keys changed through the API. On load, unknown keys, values that do
not deserialize, and values the merged configuration rejects are logged and dropped, and an
unreadable file is ignored, so the store never prevents startup. `patch` deep-merges a
sparse object (objects merge, other values replace, `null` resets), checks each leaf on its
own (errors name the key), validates the fully merged configuration, writes the store
atomically (temp file + rename, mode 0600) and publishes the result. `web.password` is
taken in plain text and stored as an argon2id hash (`Config.web.password_hash`, a
`serde(skip)` field); a plaintext `web.password` from the file or environment wins and pins
the key. `soulseek.password` and `metrics.password` are stored as given. API keys created
through the API keep only their SHA-256 (`Config.web.managed_api_keys`); the key is
returned once. Secrets are never returned: `Settings.values` leaves out
`slskr_api::config::SECRET_KEYS` that are set (listed in `Settings.secrets_set`) and
`web.api_keys` (listed without keys in `Settings.api_keys`).

`config::restart_required(startup, current)` drives `State.pending_restart` and
`Settings.pending_restart`: `web.listen`,
`web.url_base`, `web.auth`, `web.log_requests`, `soulseek.connect`, `uploads.slots`,
`downloads.slots`, `search.incoming_concurrency`, `[logging]` and `[metrics]` apply only on
restart. Everything else is read live or applied on change: web credentials and API keys
and `web.session_ttl` (`Auth::update`; a username or password change signs out every
session), Soulseek listener, proxy, distributed network and speed limits
(`ClientOptionsPatch`, reconnecting when required), Soulseek credentials and server
(`pending_reconnect`), shares (rescan), groups, blacklist, limits, retry policy, ignored
searches and retention.

### Data directory
```
slskr.toml          configuration file (optional, hand-edited)
settings.json       settings changed from the web UI (written by the daemon, 0600)
db/                 events.db, transfers.db, search.db, messaging.db, sessions.db, shares-cache.json
logs/               slskr-YYYYMMDD.log (pruned by retention.logs)
downloads/          default downloads.directory
incomplete/         default downloads.incomplete_directory
```

### `state::StateStore` (Clone)
```rust
pub fn get(&self) -> State;                            // slskr_api::application::State
pub fn update(&self, f: impl FnOnce(&mut State));      // mutate; watchers notified only on a real change
pub fn spawn_publisher(&self, bus: EventBus, shutdown: CancellationToken); // "state" events, at most 4/s, latest always sent
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<State>;
```
### `db::Database` (Clone)
```rust
pub fn open(path: &Path, migrations: &[&str]) -> anyhow::Result<Self>; // path ":memory:" in tests
pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static, T: Send + 'static; // spawn_blocking
```
Migrations: ordered SQL strings; applied ones tracked via `PRAGMA user_version`.
Timestamps stored as TEXT RFC 3339 UTC; flags as INTEGER; GUIDs as TEXT.

### `http::ApiError`
`enum ApiError { BadRequest(String), Unauthorized, Forbidden(String), NotFound(String), Conflict(String), Internal(anyhow::Error), Status(StatusCode, String) }`
implementing `IntoResponse` like ASP.NET results (`BadRequest("msg")` -> 400 with JSON string body;
`NotFound()` -> 404 empty body when the message is empty) and `From<anyhow::Error>`, `From<soulseek::Error>`
(UserOffline -> 404, Timeout -> 504? see slskd controllers for each endpoint's mapping).
`pub type ApiResult<T> = Result<T, ApiError>;`

### Authentication
`POST /api/v0/session` with `{username, password}` checks the web credentials (a plaintext
`web.password` from the file or environment in constant time, otherwise the argon2id hash
of a password set from the UI, on a blocking thread; API keys are compared by SHA-256 in
constant time). A failure answers 401 after a 500 ms async delay; success returns the
`Session` (`{username, role}`) with `Set-Cookie: slskr_session=<token>; HttpOnly; SameSite=Strict;
Path=<url_base>; Max-Age=<web.session_ttl>`, plus `Secure` when `X-Forwarded-Proto: https`
(TLS is a reverse proxy's job). `GET` returns the caller's `Session` (cookie, API key, or the
`Anonymous` administrator when `web.auth = false`) or 401; `DELETE` deletes the session and
expires the cookie (204).

The token is 32 random bytes, base64url; `db/sessions.db` stores only its SHA-256 with
username, role, `created_at`, `expires_at` and `last_seen_at`. `SessionStore` keeps the rows
in memory for synchronous lookups and writes through one ordered writer task. Expiry slides:
a request at least an hour after the last refresh extends `expires_at` by the TTL and the
`refresh_session_cookie` middleware re-sends the cookie. Expired rows are pruned hourly and
on startup. Sessions survive restarts unless the web username or password changed, also
while the daemon was stopped (a salted fingerprint of the credentials is kept in the `meta`
table).

Cross-site protection: a cookie-authenticated request other than GET/HEAD/OPTIONS, and any
WebSocket upgrade (`/api/events`), is refused with 403 when it carries an `Origin`
whose host and port differ from `Host`. Forwarded host headers are not trusted, so a reverse
proxy must pass the original `Host`. Sign-in and sign-out apply the same check. API-key
requests skip it; with `web.auth = false` cross-site requests are refused too.

### Auth in handlers
`crate::auth::{Authenticated, RequireReadWrite, RequireAdmin, RequireAdminSession}` extractors
(require `Auth: FromRef<AppState>`, implemented in http.rs) reject with 401 or 403.
`RequireAdminSession` refuses API keys (shutdown, restart, `/settings`). The events socket
authenticates its upgrade with `Auth::authenticate_request` and re-checks the same
credentials on every ping.

### Routes
Each service module exposes `pub fn routes() -> axum::Router<AppState>` with paths relative to
`/api/v0` (e.g. `"/searches/{id}"`, axum 0.8 `{param}` syntax). `bootstrap.rs` merges them.
The events socket is mounted at `/api/events` by bootstrap (beside `/api/v0`, also under
`web.url_base`).

### Service construction (in `bootstrap.rs`, this order)
```rust
EventService::new(&core) -> anyhow::Result<Self>
UserService::new(&core) -> anyhow::Result<Self>
ShareService::new(&core) -> anyhow::Result<Self>
FileService::new(&core) -> anyhow::Result<Self>
TransferService::new(&core, &events, &users, &shares) -> anyhow::Result<Self>
TelemetryService::new(&core, &transfers) -> anyhow::Result<Self>
SearchService::new(&core).await -> anyhow::Result<Self>
MessagingService::new(&core, &events) -> anyhow::Result<Self>
```
All services are `Clone` (Arc inner). After `AppState` exists, bootstrap calls
`service.start(app.clone())` (sync fn that spawns background tasks: soulseek event
subscriptions, timers, options watchers) on every service in the same order.

### Cross-service methods
```rust
// events
EventService::raise(&self, event: slskr_api::events::Event);       // persist + notify subscribers
EventService::subscribe(&self) -> tokio::sync::broadcast::Receiver<slskr_api::events::Event>;
// users
UserService::group(&self, username: &str) -> String;                 // "privileged" | "default" | "leechers" | "blacklisted" | user-defined
UserService::is_blacklisted(&self, username: &str, ip: Option<IpAddr>) -> bool;
UserService::group_upload_options(&self, group: &str) -> GroupUploadInfo { slots: u32, strategy: &'static str, speed_limit: u32, priority: u32 };
// shares
ShareService::browse(&self) -> anyhow::Result<soulseek::BrowseResponse>;
ShareService::list_directory(&self, directory: &str) -> anyhow::Result<Vec<soulseek::Directory>>;
ShareService::search(&self, query: &soulseek::SearchQuery) -> anyhow::Result<Vec<soulseek::File>>;
ShareService::resolve_file(&self, remote_filename: &str) -> anyhow::Result<(std::path::PathBuf, u64)>; // local path + size
ShareService::counts(&self) -> (u32 /*dirs*/, u32 /*files*/);
// transfers
TransferService::enqueue_upload(&self, username: &str, endpoint: SocketAddr, filename: &str) -> Result<(), String>; // Err = reject reason
TransferService::place_in_queue(&self, username: &str, filename: &str) -> Option<u32>;
TransferService::upload_info(&self) -> (u32 /*slots*/, u32 /*queue length*/, bool /*has free slot*/);
TransferService::upload_info_for(&self, username: &str) -> (u32, u32, bool); // user group forecast
TransferService::database(&self) -> &Database;                       // transfers.db, read by telemetry
```
Users routes: `GET /users/{username}/browse`, `/browse/status`, `/info`, `/status`,
`/statistics`, `/endpoint`; `POST /users/{username}/directory` accepts
`DirectoryContentsRequest`. Reads require `Authenticated`; directory requests require
`RequireReadWrite`. The service caches observed peer info/status/statistics and completed
browses; status/metadata requests populate the caches used by the fast group lookup.
Group precedence: blacklist, privileged, configured user-defined membership, leecher
thresholds, then default. Blacklist matching supports case-insensitive names, `*`/`?`
patterns, and configured IP ranges.

Shares routes: `GET /shares`, `/shares/{id}`, `/shares/contents`, and
`/shares/{id}/contents`. `PUT /shares` starts a scan and `DELETE /shares` cancels it;
both require read/write access. `ShareService::scan(&self, cancel: CancellationToken)`
walks configured non-excluded roots without following symlinks,
builds a replacement snapshot off-lock, persists `db/shares-cache.json` by rename,
then publishes the snapshot and updates `State.shares`. Failed/cancelled scans leave
the previous snapshot intact. Remote paths are normalized and reject traversal;
resolution canonicalizes both candidate and share root before returning a local path.
Alias names are a single remote path component. Lofty reads audio properties for
supported local files without loading tags or cover art.

### transfers.db schema (owned by transfers, read by telemetry)
```sql
CREATE TABLE transfers (
  id TEXT PRIMARY KEY, batch_id TEXT, username TEXT NOT NULL, direction TEXT NOT NULL, -- 'Download' | 'Upload'
  filename TEXT NOT NULL, size INTEGER NOT NULL, start_offset INTEGER NOT NULL DEFAULT 0,
  state INTEGER NOT NULL, state_description TEXT, requested_at TEXT NOT NULL, enqueued_at TEXT,
  started_at TEXT, ended_at TEXT, bytes_transferred INTEGER NOT NULL DEFAULT 0,
  average_speed REAL NOT NULL DEFAULT 0, exception TEXT, attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT, removed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ix_transfers_username ON transfers(username);
CREATE INDEX ix_transfers_state ON transfers(state);
```
`state` uses slskd `TransferStates` bit values (same as `soulseek::TransferState`).

## Live events (`src/realtime/`)

Services publish to one `EventBus` (`Core.bus`, a tokio broadcast channel). `publish`
serializes an `slskr_api::realtime::Event` once and stamps it with a sequence number;
`wants(topic)` / `publish_with(topic, || event)` skip building events nobody is subscribed
to. Publishers change their data first and publish second, so a snapshot taken after
reading `bus.seq()` includes every event up to that number (the log buffer publishes and
snapshots under its own lock to make this exact).

| Topic | Snapshot on subscribe | Then | Publisher |
|---|---|---|---|
| `state` | `state` (`State`) | `state`, coalesced to at most one per 250 ms, the latest always sent | `StateStore::spawn_publisher` |
| `settings` | `settings` (redacted `Config`, as `GET /options`) | `settings` on every configuration change | `AppState::watch_options` |
| `searches` | `search_list` (`Vec<Search>`, no responses) | `search_created`, `search_updated`, `search_deleted {id}`, `search_responses {search_id, responses}` | `SearchService`; progress (response batch, then counts) at most every 500 ms per search; state changes and the final state at once |
| `logs` (administrators) | `log_buffer` (`Vec<LogRecord>`) | `log` per record | `LogBuffer` |
| `metrics` | `metrics` (`CurrentMetrics`) | `metrics` every second | `TelemetryService` |

Frames are JSON text, `{"type": "<event>", "data": ...}`. The client sends
`{"type": "subscribe", "topics": [...]}` and `{"type": "unsubscribe", "topics": [...]}`
(at most 4 KiB per message). Subscribing sends the topic's snapshot, also when it was
already subscribed. An invalid command, a binary frame, or `logs` without the
Administrator role is answered with `{"type": "error", "data": {"message", "topic"?}}`;
the connection stays open.

Each connection (`socket.rs`) is one task: a dispatcher (client commands, bus filtering,
pings) and a writer draining a bounded queue (256 frames) into the socket. The server pings
every 20 s and re-checks the upgrade's credentials then. Close codes: `4401` the session or
key is no longer valid (sign in again), `4408` nothing received for 60 s, `4429` the queue
or the bus overflowed (reconnect and resubscribe), `1001` daemon shutdown. A closing
connection drops its queue and gets 5 s to deliver the close frame.
