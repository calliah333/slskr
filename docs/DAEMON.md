# slskr daemon internals (`crates/slskr`)

Module ownership and the contracts between modules. Every module mirrors the
corresponding slskd area under `ref/slskd/src/slskd/<Area>`.

```
src/main.rs           CLI entry (clap), calls slskr::run()
src/lib.rs            module declarations, pub async fn run(cli) (bootstrap)
src/app.rs            AppState / Core / Services / Hubs / Paths (contract, already written)
src/bootstrap.rs      App::build + run(): load options, build core + services, bind HTTP, wire hubs
src/config.rs         OptionsStore: YAML (snake_case) <-> Options (camelCase), env (SLSKD_*), CLI overrides, file watch
src/state.rs          StateStore: application State, broadcasts "state" on the application hub
src/db.rs             Database: sqlite wrapper with migrations
src/http.rs           ApiError, router assembly helpers, static file serving + SPA fallback, urlBase
src/core_api.rs       /application, /session, /server, /options, /debug controllers
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
src/relay/            RelayService (controller + agent modes) + /relay controller + relay hub
src/integrations/     IntegrationService (webhooks, scripts, ftp, pushbullet, ntfy) driven by events
```

## Shared contracts

### `config::OptionsStore` (Clone)
```rust
pub fn get(&self) -> Arc<Options>;                     // slskr_api::options::Options
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Options>>;
pub fn debug_view(&self) -> String;                    // for /options/debug
pub async fn apply_yaml(&self, yaml: &str) -> Result<(), String>; // validate + write config file + reload
pub fn read_yaml(&self) -> std::io::Result<String>;
pub fn validate_yaml(yaml: &str) -> Option<String>;    // None = valid, Some(error)
pub fn patch(&self, f: impl FnOnce(&mut Options));     // runtime-only overlay (remote_configuration PATCH)
```
### `state::StateStore` (Clone)
```rust
pub fn get(&self) -> State;                            // slskr_api::application::State
pub fn update(&self, f: impl FnOnce(&mut State));      // mutate + broadcast "state" (debounced) on application hub
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<State>;
```
### `db::Database` (Clone)
```rust
pub fn open(path: &Path, migrations: &[&str]) -> anyhow::Result<Self>; // path ":memory:" when flags.volatile
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

### Auth in handlers
`crate::auth::{Authenticated, RequireReadWrite, RequireAdmin, RequireAdminJwt}` extractors
(require `Auth: FromRef<AppState>`, implemented in http.rs). Use the role/policy shown in
slskd's `[Authorize]` attribute for each endpoint.

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
RelayService::new(&core, &shares, &transfers) -> anyhow::Result<Self>
IntegrationService::new(&core, &events) -> anyhow::Result<Self>
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
builds a replacement snapshot off-lock, persists `data/shares-cache.json` by rename,
then publishes the snapshot and updates `State.shares`. Failed/cancelled scans leave
the previous snapshot intact. Remote paths are normalized and reject traversal;
resolution canonicalizes both candidate and share root before returning a local path.
Alias names are a single remote path component. Lofty reads audio properties for
supported local files without loading tags or cover art. The relay imports validated
slskd SQLite snapshots and preserves their wire attributes. Disconnect unpublishes
the agent's shares without deleting its on-disk cache.

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
