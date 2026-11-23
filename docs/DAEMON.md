# slskr daemon internals (`crates/slskr`)

Module ownership and the contracts between modules. Every module mirrors the
corresponding slskd area under `ref/slskd/src/slskd/<Area>`.

```
src/main.rs           CLI entry (clap), calls slskr::run()
src/lib.rs            module declarations, pub async fn run(cli) (bootstrap)
src/app.rs            AppState / Core / Services / Hubs / Paths (contract, already written)
src/bootstrap.rs      CLI (--data-dir, --config, --no-connect), App::build + run(): load config, build core + services, bind HTTP, wire hubs
src/config.rs         ConfigStore: slskr.toml <-> slskr_api::config::Config, env (SLSKR_*), CLI overrides, validation, save, file watch
src/state.rs          StateStore: application State, broadcasts "state" on the application hub
src/db.rs             Database: sqlite wrapper with migrations
src/http.rs           ApiError, router assembly helpers, static file serving + SPA fallback, urlBase
src/core_api.rs       /application, /session, /server, /options, /debug controllers
src/auth/             Auth (sessions, API keys, Origin checks, extractors); session.rs: SessionStore, sessions.db
src/resolvers.rs      impl soulseek::Resolvers by delegating to services
src/logging.rs        tracing setup, LogBuffer (ring buffer), logs hub, /logs controller, file logs
src/events/           EventService (raise/list/subscribe) + /events controller, events.db
src/users.rs           UserService (peer info/browse cache, group and blacklist policy) + `/users` controller
src/shares.rs          ShareService (atomic scan snapshots, persisted cache, search/browse/resolve) + `/shares` controller
src/files/            FileService + /files controller (downloads/incomplete dirs)
src/telemetry/        TelemetryService (metrics hub "Update", prometheus) + /telemetry controllers (reports from transfers.db)
src/transfers/        TransferService (downloads, uploads, upload queue, governor) + /transfers controller, transfers.db
src/search/           SearchService + /searches controller + search hub, search.db
src/messaging/        MessagingService (conversations, rooms) + /conversations, /rooms controllers, messaging.db
```

## Shared contracts

### `config::ConfigStore` (Clone)
```rust
pub fn get(&self) -> Arc<Config>;                      // slskr_api::config::Config, current
pub fn startup(&self) -> &Config;                      // as loaded at process start
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Config>>;
pub fn read_toml(&self) -> std::io::Result<String>;    // file text, "" when absent
pub fn validate_toml(&self, text: &str) -> Option<String>;          // None = valid
pub async fn apply_toml(&self, text: &str) -> Result<(), String>;   // validate + write + reload
```
Sources, lowest precedence first: `Config::default()`, `slskr.toml` (in `--data-dir`, or
`--config`), `SLSKR_<SECTION>_<KEY>` environment variables, then CLI flags (`--no-connect`
sets `soulseek.connect = false`). Environment names are matched against the serialized
defaults, so keys containing `_` and nested tables (`SLSKR_UPLOADS_LIMITS_QUEUED_FILES`)
need no extra separator; values are typed like the default (booleans, integers,
comma-separated lists, strings). `[[web.api_keys]]` and user-defined `[groups.<name>]` are
file-only. `SLSKR_DATA_DIR`, `SLSKR_CONFIG` and `SLSKR_CONTENT_PATH` are process settings,
not keys; other `SLSKR_*` names outside the configuration sections are ignored, while an
unknown key inside a section is an error.

Every section is `#[serde(default, deny_unknown_fields)]`. Loading normalizes `web.url_base`
(`/` or `/prefix`) and `metrics.path`, resolves relative paths (`downloads.*directory`,
`soulseek.picture`, `shares.directories`) against the data directory, then validates;
errors name the key (`web.api_keys[0].key must be between 16 and 255 characters`). A missing
file means defaults; nothing is written until the UI saves. `apply_toml` refuses blank
documents, resolves the full layer stack before writing, keeps the previous file as
`slskr.toml.bak`, and replaces the file atomically (temp file + rename, mode 0600). The text
is written as given, so comments survive. The file is polled every second; a valid change is
published to subscribers, an invalid one is logged and ignored.

`config::restart_required(startup, current)` drives `State.pending_restart`: `web.listen`,
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
slskr.toml          configuration (optional)
db/                 events.db, transfers.db, search.db, messaging.db, sessions.db, shares-cache.json
logs/               slskr-YYYYMMDD.log (pruned by retention.logs)
downloads/          default downloads.directory
incomplete/         default downloads.incomplete_directory
```

### `state::StateStore` (Clone)
```rust
pub fn get(&self) -> State;                            // slskr_api::application::State
pub fn update(&self, f: impl FnOnce(&mut State));      // mutate + broadcast "state" (debounced) on application hub
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
`POST /api/v0/session` with `{username, password}` checks the web credentials in constant
time (a failure answers 401 after a 500 ms async delay) and returns the `Session`
(`{username, role}`) with `Set-Cookie: slskr_session=<token>; HttpOnly; SameSite=Strict;
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
WebSocket upgrade (SignalR hubs included), is refused with 403 when it carries an `Origin`
whose host and port differ from `Host`. Forwarded host headers are not trusted, so a reverse
proxy must pass the original `Host`. Sign-in and sign-out apply the same check. API-key
requests skip it; with `web.auth = false` cross-site requests are refused too.

### Auth in handlers
`crate::auth::{Authenticated, RequireReadWrite, RequireAdmin, RequireAdminSession}` extractors
(require `Auth: FromRef<AppState>`, implemented in http.rs) reject with 401 or 403.
`RequireAdminSession` refuses API keys (shutdown, restart, raw configuration file). Hubs use
`Auth::hub_authorizer(role)` on negotiate and upgrade.

### Routes
Each service module exposes `pub fn routes() -> axum::Router<AppState>` with paths relative to
`/api/v0` (e.g. `"/searches/{id}"`, axum 0.8 `{param}` syntax). `bootstrap.rs` merges them.
Hubs are mounted at `/hub/{name}` by bootstrap.

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
