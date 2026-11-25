# slskr daemon internals (`crates/slskr`)

Module ownership and the contracts between modules. The HTTP routes themselves are listed
in [API.md](API.md).

```
src/main.rs           CLI entry, calls slskr::run()
src/cli.rs            command line flags (hand-parsed; four flags and their env vars)
src/lib.rs            module declarations, pub async fn run(cli) (bootstrap)
src/app.rs            AppState / Core / Services / Paths (contract), Snapshots for the events socket
src/bootstrap.rs      CLI (--data-dir, --config, --no-connect, --web-dir), build + run(): load config, build core + services, api_routes() (every route), router, bind HTTP
src/config.rs         ConfigStore: layers (settings.json, slskr.toml, SLSKR_*, CLI) -> slskr_types::config::Config, pinned keys, settings API, validation, file watch
src/config/store.rs   settings.json: sparse settings tree, web password hash, API key digests (lenient load)
src/state.rs          StateStore: application State, coalesced "state" events
src/db.rs             Database: sqlite wrapper with migrations
src/http.rs           ApiError, Json/Path/Query extractors, json_errors middleware, Routes builder
src/web.rs            web UI: embedded files (build.rs) or --web-dir, caching, SPA fallback, urlBase
build.rs              embeds the trunk output named by SLSKR_EMBED_UI (or a placeholder page)
src/core_api.rs       /application, /auth, /session, /server, /config, /settings, /api-keys routes
src/setup.rs          first-run setup: setup code, the 503 gate, /setup, /soulseek/test-login, /fs/dirs routes
src/auth/             Auth (sessions, API keys, Origin checks, extractors); session.rs: SessionStore, sessions.db
src/resolvers.rs      impl soulseek::Resolvers by delegating to services
src/logging.rs        tracing setup, LogBuffer (ring buffer, "log" events), /logs routes, file logs
src/realtime/         EventBus (internal publish/subscribe) + socket.rs: the /api/events WebSocket
src/events.rs         EventService (raise/list) + /events/history route, events.db
src/users.rs          UserService (peer info/browse cache, group and blacklist policy) + /users routes
src/shares.rs         ShareService (atomic scan snapshots, persisted cache, search/browse/resolve) + /shares routes
src/files.rs          /files routes (browse and delete below the downloads/incomplete dirs)
src/telemetry/        TelemetryService ("metrics" events, prometheus) + /metrics and /reports routes (reports from transfers.db)
src/profiling.rs      always-on timings (database calls per call site, API routes, transfer ends, CPU/RSS/tasks): slskr_profile_* metrics and a log report every 15 min
src/transfers/        TransferService (downloads, uploads, upload queue, governor) + /transfers and /batches routes, transfers.db
src/search/           SearchService + /searches routes + "search_*" events, search.db
src/messaging/        MessagingService (conversations, rooms) + /conversations, /rooms routes, messaging.db
```

## Shared contracts

### `config::ConfigStore` (Clone)
```rust
pub fn get(&self) -> Arc<Config>;                      // slskr_types::config::Config, current
pub fn startup(&self) -> &Config;                      // as loaded at process start
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<Config>>;
pub async fn settings(&self) -> slskr_types::settings::Settings;            // GET /settings
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
`SLSKR_CONFIG` and `SLSKR_WEB_DIR` are process settings, not keys; other `SLSKR_*`
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
`slskr_types::config::SECRET_KEYS` that are set (listed in `Settings.secrets_set`) and
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
pub fn get(&self) -> State;                            // slskr_types::application::State
pub fn update(&self, f: impl FnOnce(&mut State));      // mutate; watchers notified only on a real change
pub fn spawn_publisher(&self, bus: EventBus, shutdown: CancellationToken); // "state" events, at most 4/s, latest always sent
pub fn subscribe(&self) -> tokio::sync::watch::Receiver<State>;
```
### `db::Database` (Clone)
```rust
pub fn open(path: &Path, migrations: &[&str]) -> anyhow::Result<Self>; // path ":memory:" in tests
#[track_caller] pub fn call<T, F>(&self, f: F) -> impl Future<Output = anyhow::Result<T>> + Send + use<T, F>
    where F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static, T: Send + 'static; // spawn_blocking, timed per caller line
pub fn reader(&self) -> anyhow::Result<Self>; // separate read-only connection (itself in memory)
```
Each `Database` is one connection behind a mutex, so calls run one at a time. File
databases use WAL, so a `reader()` connection runs long reads (transfer reports) without
holding up the writer. `call` records its wait and run time in `profiling` under the
database name and the caller's line, and logs calls that run for a second or more.
Migrations: ordered SQL strings; applied ones tracked via `PRAGMA user_version`.
Timestamps are stored as TEXT RFC 3339 UTC, except transfer times (`transfers.*_at`),
which are INTEGER nanoseconds since the Unix epoch so reports can subtract and bucket
them without parsing; flags as INTEGER; GUIDs as TEXT.

### Errors (`http.rs`)
`enum ApiError { BadRequest(String), Unauthorized, Forbidden(String), NotFound(String), Conflict(String), Internal(anyhow::Error), Status(StatusCode, String) }`
(`pub type ApiResult<T> = Result<T, ApiError>`) answers `{"error": "<message>"}`
(`slskr_types::error::ErrorBody`); an empty message becomes the status reason, and
`Internal` is logged and answered with a generic message. `From<soulseek::Error>` maps
offline or unknown users and transfers to 404, timeouts to 504, a refused room to 403 and
duplicates to 409. Handlers take `http::{Json, Path, Query}` instead of the axum
extractors: their rejections become `ApiError`s with axum's status (400 malformed, 415
missing JSON content type, 422 wrong shape, 413 too large). `http::optional_json` reads an
optional body. The `http::json_errors` middleware wraps every `/api` route and rewrites any
other non-JSON 4xx/5xx (unknown route, 405, body limits, upgrade failures) into the same
shape, keeping a short plain-text body as the message. Auth rejections are JSON too.

### Authentication
`POST /api/session` with `{username, password}` checks the web credentials (a plaintext
`web.password` from the file or environment in constant time, otherwise the argon2id hash
of a password set from the UI, on a blocking thread; API keys are compared by SHA-256 in
constant time). A failure answers 401 after a 500 ms async delay; success answers 201 with the
`Session` (`{username, role}`) and `Set-Cookie: slskr_session=<token>; HttpOnly; SameSite=Strict;
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
`RequireAdminSession` refuses API keys (shutdown, restart, `/settings`, `/api-keys`). The events socket
authenticates its upgrade with `Auth::authenticate_request` and re-checks the same
credentials on every ping.

### First run (`setup.rs`)
Setup is required while `web.auth` is enabled and the merged configuration has neither a
plaintext `web.password` nor `web.password_hash` (`setup::required`). `AppInner.setup`
(`setup::Setup`) holds the one-time setup code and the wrong-code counters, in memory only.
`setup::sync(app)` aligns them with the configuration: it runs at startup
(`AppState::start`), after every configuration change (`watch_options`) and on the setup
routes. When setup becomes required it generates a code (12 characters from
`ABCDEFGHJKMNPQRSTUVWXYZ23456789`, `rand::rng()`, shown as `XXXX-XXXX-XXXX`) and announces
it with `http://<web.listen><url_base>/setup` at WARN and on stderr (process logs do not
reach stderr; containers show it); when setup is no longer required it forgets both.

`setup::gate` is layered over the whole API router (REST and events socket) and answers
`503 {"error": "setup required"}` for every path but `/api/auth`, `/api/setup`,
`/api/soulseek/test-login` and `/api/fs/dirs` (paths as seen below `url_base`). 503 rather
than 401 keeps "nobody can sign in yet" apart from "you are not signed in": the UI's 401
handling (reload, sign-in form) and `POST /api/session` (500 `Misconfigured` without a
password) would otherwise loop or mislead; `GET /api/auth` carries `setup_required` for the
UI to branch on.

`setup::Caller::local` treats a request as coming from the daemon's machine when the
`ConnectInfo` peer is loopback, the `Host` (or URI authority) is `localhost` or a loopback
literal (defeats DNS rebinding), and none of `Forwarded`, `X-Forwarded-For`,
`X-Forwarded-Host`, `X-Real-IP` is present (a local reverse proxy makes every caller
loopback). Others must present the code (`SetupRequest.code` or `X-Setup-Code`), compared
in constant time after uppercasing and removing spaces and dashes. `Setup::check` keeps a
fixed one-minute window per address (IPv6 per /64): after 10 wrong codes the address gets
429 with `Retry-After`, also for the right code; at most 4096 addresses are tracked, and
when they are all in a window new addresses are refused too. A wrong code is answered
after a 500 ms async sleep. Non-GET setup requests are refused cross-site like sign-in.
`setup::SetupAccess` (login test, directory picker) applies this rule while setup is
required and `Auth::authorize(.., Administrator, Policy::Any)` afterwards.

`POST /api/setup` holds a mutex, re-checks that setup is required (409 otherwise),
checks the caller, validates the whole request (blocking checks of share paths on a
blocking thread) and answers 400 with every problem joined by `; `. It then applies one
`ConfigStore::patch` (pinned keys are left out when unchanged, otherwise the patch refuses
them with 409), updates `Auth`, calls `setup::sync` (clearing the code), creates a session
with `SessionStore::create` and sets the cookie like `POST /api/session`. Given shares are
appended to `shares.directories` and scanned (`ShareService::launch_scan`, retried every
500 ms while another scan runs); given Soulseek credentials trigger `AppState::connect`
when `soulseek.connect` is true.

`POST /api/soulseek/test-login` builds a second `SoulseekClient` from
`config::client_options` with the listener, distributed network and diagnostics off,
connects to `soulseek.server` under a 10 s timeout and disconnects; the running client is
not touched. It refuses (409) when `soulseek.connect` is false or the running client is
connected under the same username (the server would kick that session), and allows one
test at a time (429). `LoginRejected` messages map to `invalid_password` (`INVALIDPASS`,
`EMPTYPASSWORD`), `invalid_username`, `server_full`, `server_private`, `invalid_version` or
`rejected`; address, connection and I/O failures to `unreachable`; timeouts to `timeout`.
The password is never logged. `GET /api/fs/dirs` lists subdirectories on a blocking thread:
absolute paths without `..`, `stat` semantics (symlinks to directories are listed), entries
that can not be read or are not UTF-8 skipped, at most 20 000 entries read and 1000
returned.

### Routes
Each service module exposes `pub fn routes() -> http::Routes`, built with
`Routes::new().get("/api/searches/{id}", handler)` (full paths, axum 0.8 `{param}` syntax).
`Routes` records every `(method, path)` it registers; `bootstrap::api_routes()` merges the
modules and adds the events socket (`GET /api/events`), and a test compares that table
with [API.md](API.md), so a route can not be added without documenting it. `bootstrap`
adds catch-all JSON 404s for `/api` and `/api/{*rest}`, layers `json_errors` and response
compression (gzip, fastest level) over them, merges the events socket without
compression, adds `/health` and the Prometheus path, and nests the whole set under
`web.url_base` (keeping the root reachable for proxies that strip the prefix). The web UI
router (`web.rs`) is merged beside it and never answers `{url_base}/api/...` with
`index.html`.

### Web UI (`web.rs`, `build.rs`)
`build.rs` embeds the trunk output named by `SLSKR_EMBED_UI` at compile time (with the
`embed-ui` feature, which brings in the compressors): every file with its content type, a
SHA-256 ETag and brotli/gzip variants when they save at least 10%.
Without the variable the binary embeds a placeholder page explaining how to build the UI,
so `cargo build` and `cargo test` need no wasm toolchain. `--web-dir DIR` serves a
directory instead (UI development; paths are confined to the directory, symlinks
included). Names with a content hash (`slskr-web-<hash>_bg.wasm`, `static/css/app-<hash>.css`)
are sent with `Cache-Control: public, max-age=31536000, immutable`, everything else with
`no-cache`; `If-None-Match` answers 304. `index.html` gets `<base href="{url_base}/">` and
`window.urlBase` once at startup. Paths without an extension fall back to `index.html` so
client-side routes deep-link.

### Service construction (in `bootstrap.rs`, this order)
```rust
EventService::new(&core) -> anyhow::Result<Self>
UserService::new(&core) -> anyhow::Result<Self>
ShareService::new(&core) -> anyhow::Result<Self>
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
EventService::raise(&self, data: slskr_types::events::EventData);   // record (events.db)
// users
UserService::group(&self, username: &str) -> Arc<str>;               // "privileged" | "default" | "leechers" | "blacklisted" | user-defined
UserService::is_blacklisted(&self, username: &str, ip: Option<IpAddr>) -> bool;
// shares
ShareService::browse_frame(&self) -> bytes::Bytes;                    // encoded SharesReply, cached per snapshot
ShareService::list_directory(&self, directory: &str) -> anyhow::Result<Vec<soulseek::Directory>>;
ShareService::search(&self, query: &soulseek::SearchQuery, limit: usize) -> anyhow::Result<Vec<soulseek::File>>;
ShareService::resolve_file(&self, remote_filename: &str) -> anyhow::Result<(std::path::PathBuf, u64)>; // local path + size
ShareService::counts(&self) -> (u32 /*dirs*/, u32 /*files*/);
// transfers
TransferService::enqueue_upload(&self, username: &str, endpoint: SocketAddr, filename: &str) -> Result<(), String>; // Err = reject reason
TransferService::place_in_queue(&self, username: &str, filename: &str) -> Option<u32>;
TransferService::upload_info_for(&self, username: &str) -> (u32, u32, bool); // user group forecast
TransferService::database(&self) -> &Database;                       // transfers.db, read by telemetry
```
Users routes (`/api/users/{username}/...`): `browse`, `browse/status`, `directory?path=`,
`endpoint`, `info`, `statistics`, `status`. Requests that contact the peer require
`RequireReadWrite`; cached reads (`browse/status`, `statistics`) require `Authenticated`. The service caches observed peer info/status/statistics and completed
browses; status/metadata requests populate the caches used by the fast group lookup.
Group precedence: blacklist, privileged, configured user-defined membership, leecher
thresholds, then default. Blacklist matching supports case-insensitive names, `*`/`?`
patterns, and configured IP ranges.

Shares routes: `GET /api/shares`, `/api/shares/{id}` and `/api/shares/{id}/contents`.
`POST /api/shares/scan` starts a scan (202) and `DELETE /api/shares/scan` cancels it;
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
  id BLOB NOT NULL, -- 16 UUID bytes (`store::key`/`store::id`); text if not a canonical UUID
  batch_id TEXT, username TEXT NOT NULL, direction TEXT NOT NULL, -- 'Download' | 'Upload'
  file_id INTEGER NOT NULL, size INTEGER NOT NULL, state INTEGER NOT NULL,
  requested_at INTEGER NOT NULL, enqueued_at INTEGER, started_at INTEGER, ended_at INTEGER, -- ns
  bytes_transferred INTEGER NOT NULL DEFAULT 0, average_speed REAL NOT NULL DEFAULT 0,
  error_id INTEGER, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER,
  removed INTEGER NOT NULL DEFAULT 0, place_in_queue INTEGER,
  root TEXT -- the remote folder of a folder download
);
CREATE TABLE transfer_directories (directory_id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
CREATE TABLE transfer_files (file_id INTEGER PRIMARY KEY, directory_id INTEGER NOT NULL,
  name TEXT NOT NULL, UNIQUE (directory_id, name)); -- filename = path || name
CREATE TABLE transfer_errors (error_id INTEGER PRIMARY KEY, exception TEXT NOT NULL UNIQUE);
CREATE VIEW transfer_details; -- transfers with seq (rowid), filename, directory and exception
CREATE INDEX ix_transfers_listed_file ON transfers(direction, username, file_id)
  WHERE removed = 0 AND (state & 16) != 0; -- superseding a listed transfer of the same file
```
Paths and errors are interned (`store::file_id`, `store::error_id`): a share has tens of
thousands of files, and failures repeat a few hundred texts once `mask_addresses` replaces
the peer addresses in them with `<address>`. Reads go through `transfer_details`; writes
and filters use the ids. `state` stores the `soulseek::TransferState` bit values
(`state & 16` is "completed"); `transfers::store::{state_from_bits, state_to_bits}` convert
them to and from the API's `TransferState` enum. `exception` holds the failure as
`Kind: message`; the API calls it `error`.

Rows removed from the list (cleared, superseded or pruned by `retention.*`) are deleted
[`store::PURGE_AFTER`] (30 days) after they ended, by `store::purge` on the retention tick.
It marks them `removed = 2` first, which the rollup delete triggers skip, so the reports
keep their totals. Other deletes (the failures of banned users) leave the rollups too.

Reports read hourly rollups that triggers on `transfers` keep current:
`transfer_chart_hours` (chart), `transfer_user_hours` (summary, leaderboards, user report,
histograms with whole-hour buckets),
`transfer_error_hours` (error types) and `transfer_directory_hours` (popular folders).
A window reads its whole hours from them and only the transfers of the partial hours at
its edges (`telemetry::reports::Split`), so a year of history costs tens of thousands of
rows rather than every transfer. A change to what a report counts must change its rollup,
its triggers and its backfill (`migrations/compact.sql`) together; `report_hours_follow_transfer_changes` checks that
the triggers agree with the backfill.

Upload backoff (`transfers/backoff.rs`): after two failures in a row of one file to one
user, the next upload of it waits queued (`next_attempt_at` set, no slot held) for 1 min,
doubling to 1 h. Peers that re-queue on every `UploadFailed` otherwise loop every few
seconds. A success, or a failure that resumed past the furthest byte reached, resets it;
records are kept in memory and forgotten after 6 h without a failure. A user whose
uploads fail five times in a row across files, with no success, is held as a whole:
`UploadQueue::dispatch` skips them for 1 min, doubling to 1 h, and the progress tick
runs it again when the hold ends. Offline peers, hung clients and peers that abort every
file otherwise fail each queued file in turn. Failures on this side (`local_failure`)
count for neither.

Queued uploads are stored in groups (`transfers/inserts.rs`): a peer queues a folder as
one request per file, so each new upload joins a queue and one writer task stores all
queued rows per transaction (at most 500). Before, a burst of hundreds of single-row
transactions queued on the writer connection for seconds.

Restarts (`transfers::store::initialize`, `TransferService::recover`): unfinished
downloads and uploads go back to `queued_locally`, in their stored order; removed ones are
marked aborted. A requeued upload waits for the server login and the share index, then
resolves its file again and fails if the file is no longer shared or changed size. Sending
resumes at the offset the peer asks for. At shutdown, uploads end by the disconnect rather
than by cancellation, so the peer is not sent `UploadDenied` for a file it will be offered
again; a user's cancellation still sends it.

### Folder downloads and the download layout
A download's local folder is computed from its stored record each time it is attempted
(`transfers::destination::derive`), so retries and restarts install to the same place. The
first of these rules that applies picks the folder below `downloads.directory`: a
batch's `destination`; the transfer's `root`; or the `downloads.subdirectory` pattern.
A `root` is the remote folder a folder download was enqueued under
(`POST /api/transfers/downloads/folder`, or `POST /api/transfers/downloads` with `root`).
The file goes to `<last component of root>/<folders of the filename below root>/<name>`.
For example, root `@@music\Artist\Album` and `@@music\Artist\Album\CD1\01.flac` give
`Album/CD1/01.flac`. Each component comes from the peer, so it is split on both `\` and
`/`, empty components are dropped, and each is sanitized. `.` and `..` become `_`, and
`:`, `<`, `>`, `|` and the other reserved characters become `_`, which turns a drive
prefix such as `C:` into `C_`. Requests already refuse names with `.`/`..` components.
Installation still confines the result to `downloads.directory`, refusing symlinked
folders, and keeps the no-clobber rename unless `on_conflict = "overwrite"`.

The folder endpoint asks the peer for the folder (FolderContentsRequest, with the client's
`message_timeout`). A response may list several folders, but slskd, Nicotine+ and slskr
answer with the requested folder only. So when `recursive` is set and the response lists
nothing below `path`, the daemon browses the user (`UserService::browse_peer`, which
reuses a browse finished in the last few seconds) and takes the folders below `path`
from it, locked ones included. The files are planned by `transfers::folder::plan`, which
drops duplicates and reports unsafe peer names as `failed`. They are then enqueued
through the normal path, which handles duplicate detection, retries, slots and events.

`search.db` keeps each `Search` and `SearchResponse` as its API JSON, and `events.db`
each event's type and data; their migrations converted records written in the older
slskd-shaped JSON.

## Live events (`src/realtime/`)

Services publish to one `EventBus` (`Core.bus`, a tokio broadcast channel). `publish`
serializes an `slskr_types::realtime::Event` once and stamps it with a sequence number;
`wants(topic)` / `publish_with(topic, || event)` skip building events nobody is subscribed
to. Publishers change their data first and publish second, so a snapshot taken after
reading `bus.seq()` includes every event up to that number (the log buffer publishes and
snapshots under its own lock to make this exact).

| Topic | Snapshot on subscribe | Then | Publisher |
|---|---|---|---|
| `state` | `state` (`State`) | `state`, coalesced to at most one per 250 ms, the latest always sent | `StateStore::spawn_publisher` |
| `settings` | `settings` (redacted `Config`, as `GET /api/config`) | `settings` on every configuration change | `AppState::watch_options` |
| `searches` | `search_list` (`Vec<Search>`, no responses) | `search_created`, `search_updated`, `search_deleted {id}`, `search_responses {search_id, responses}` | `SearchService`; progress (response batch, then counts) at most every 500 ms per search; state changes and the final state at once |
| `logs` (administrators) | `log_buffer` (`Vec<LogRecord>`) | `log` per record | `LogBuffer` |
| `metrics` | `metrics` (`CurrentMetrics`) | `metrics` every second | `TelemetryService` |
| `transfers` | `transfer_list` (in-memory transfers, stored ones only) | `transfer_updated` (changed rows, one batch per 250 ms flush), `transfer_removed {ids}` | `TransferService`; batches are published and snapshots taken under the runtime lock |
| `conversations` | `conversation_list` (open conversations) | `conversation_message`, `conversation_updated` | `ConversationService`, on the database connection right after each commit |
| `rooms` | `room_list` (joined rooms) | `room_joined`, `room_left {room}`, `room_message`, `room_user_joined {room, user}`, `room_user_left {room, username}` | `RoomService`, under the rooms write lock |

`Snapshots::snapshot` is async, so a snapshot may read a database (`conversations`); the
connection's queued events wait meanwhile. The transfer service persists a state change
on the next 250 ms flush but byte progress only every 5 s per transfer (and everything at
shutdown); the list routes overlay the live values.

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
