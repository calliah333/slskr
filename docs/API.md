# HTTP API

The daemon's API, used by the web UI and by scripts. Every route below is served under
`{web.url_base}/api` (for example `http://127.0.0.1:5031/api/searches`, or
`/client/api/searches` with `web.url_base = "/client"`). The request and response types
are the Rust types in [`crates/slskr-types`](../crates/slskr-types/src); this page is the
authoritative list of routes, and a test (`bootstrap::tests::documented_routes_match_router`)
keeps it in step with the router.

## Conventions

- **JSON**: requests and responses are JSON (`Content-Type: application/json`). Field
  names and enum values are `snake_case`. Responses include every field, with `null` for
  absent values; request fields marked optional may be left out. Responses are compressed
  with gzip when the request's `Accept-Encoding` allows it.
- **Request compression** accepts `Content-Encoding: gzip` on REST requests. Body
  size limits apply to the decompressed JSON.
- **Timestamps** are RFC 3339 UTC strings (`"2026-01-01T12:00:00.123Z"`). **Sizes** are
  bytes, **speeds** bytes per second, **durations** integer milliseconds in fields named
  `*_ms`.
- **Ids**: searches, transfers and batches have UUIDs; users, rooms and conversations are
  addressed by name. Percent-encode path segments.
- **Status codes**: `200` with a body, `201` when something was created (the body is the
  new resource), `202` when work was started, `204` without a body. Enqueueing downloads
  answers `201` when every file was enqueued, `207` when some failed and `200` when none
  was (the body lists `enqueued` and `failed` files).
- **Errors**: every error is `{"error": "<message>"}` with a 4xx or 5xx status: `400`
  malformed JSON, query or path, or an invalid value; `401` not signed in; `403` role too
  low, a cross-site request, or a missing or wrong setup code; `404` no such route or
  resource; `405` method not allowed; `409` conflicting state (already exists, not logged
  in to Soulseek, a pinned setting); `413` body too large; `415` missing JSON content type
  or unsupported request encoding;
  `422` well-formed JSON of the wrong shape; `429` another enqueue request or login test is
  running, or too many failed sign-ins or wrong setup codes (with `Retry-After`); `500` an unexpected failure;
  `503` first-run setup is required (`{"error": "setup required"}`, see
  [below](#first-run-setup)), or the server connection dropped while a room request was
  pending; `504` a peer or the server did not answer in time.

## Authentication

A request is authenticated by one of:

- the `slskr_session` cookie, set by `POST /api/session` (browsers). Requests other than
  `GET`/`HEAD`/`OPTIONS` that carry the cookie must be same-origin: an `Origin` header
  naming another host is refused with `403`. After 10 failed sign-ins within 15 minutes
  from one address (IPv6: one /64), `POST /api/session` answers `429` for that address,
  even with the right password, until 15 minutes after its first failure.
- an API key in the `X-API-Key` header (scripts). Keys are created in the web UI (System →
  Settings) or with `POST /api/api-keys`, or configured in `slskr.toml`
  (`[[web.api_keys]]`); see [configuration](CONFIGURATION.md#web).
- nothing, when `web.auth = false`: every request acts as an administrator, as long as
  its `Host` is `localhost` (or a `*.localhost` name) or an IP address. Other names get
  `403`, because a web page on a name its owner points at this address (DNS rebinding)
  would count as same-origin.

Roles, lowest first: `readonly` (read everything except logs and settings), `readwrite`
(also act: search, download, chat, connect), `admin` (also logs). Routes marked
**admin session** need a signed-in administrator and refuse API keys. **none** routes
need no authentication. **admin or setup** routes need an administrator (session or API
key) once setup is done, and before that a [setup caller](#first-run-setup).

## First-run setup

While `web.auth` is enabled and no web password is set anywhere (no password set from the
UI, none in `slskr.toml` or `SLSKR_WEB_PASSWORD`), nobody can sign in. Until setup is
completed, every API route except `GET /api/auth`, `/api/setup`,
`/api/soulseek/test-login` and `/api/fs/dirs` answers `503 {"error": "setup required"}`
(`/health` and the web UI keep working). `GET /api/auth` and `GET /api/setup` say whether
setup is required.

The daemon then logs a one-time **setup code** (`XXXX-XXXX-XXXX`, at WARN and on stderr)
with the URL of the setup page. It lives in memory only; a new one is made whenever setup
becomes required again. A *setup caller* is either on the daemon's machine (loopback peer
address, `Host` naming `localhost` or a loopback address, and no `Forwarded`,
`X-Forwarded-For`, `X-Forwarded-Host` or `X-Real-IP` header) or sends the code: as `code`
in `POST /api/setup`, or in an `X-Setup-Code` header on any setup route. Codes are
case-insensitive and may contain spaces. A wrong code is answered after 500 ms with `403`;
after 10 wrong codes in a minute from one address (IPv6: one /64) the address gets `429`
with `Retry-After`, even for the right code. Requests other than `GET` must be same-origin.
A reverse proxy on the same machine must pass the original `Host` or `X-Forwarded-For`,
otherwise remote callers look local.

`POST /api/setup` validates everything before saving anything and answers `400` naming
every problem (messages joined with `; `, each starting with the dotted key or `shares`):
`web.username` not blank (it may be blank when it is pinned: the pinned name is kept), `web.password` at least 8 characters, Soulseek
username not blank, without leading or trailing spaces and at most 30 characters, Soulseek
password not blank, and every share an existing, readable, absolute directory. It saves the
values like `PATCH /api/settings` (the web password as an argon2id hash; a pinned key may
only be sent with its current value, otherwise `409`), forgets the code, signs the caller
in like `POST /api/session`, starts a share scan when shares were given, and connects to
Soulseek when Soulseek credentials were given and `soulseek.connect` is true.

`POST /api/soulseek/test-login` logs in to `soulseek.server` with the given credentials on
a separate short-lived connection (no listener, no distributed network) and disconnects;
the running session is untouched. It answers `200 {"ok": true, "reason": null, "message":
null}` or `200 {"ok": false, "reason": ..., "message": "<server or connection message>"}`
with `reason` one of `invalid_password`, `invalid_username`, `server_full`,
`server_private`, `invalid_version`, `rejected`, `unreachable`, `timeout` (10 s) or
`error`. It answers `409` when `soulseek.connect` is false or the client is already
connected under that username (testing would disconnect it), and `429` while another test
runs. **Logging in with an unused username registers that account** on the Soulseek
server, with the given password.

`GET /api/fs/dirs` lists the subdirectories of an absolute local path (default: the
daemon's home directory) for choosing shares: no files, sorted by name, symlinks to
directories included, unreadable entries skipped, at most 1000 (`truncated` says when more
exist). It reveals local directory names to administrators and setup callers; that is its
purpose, so keep administrator access to trusted people.

## Routes

| Method | Path | Role | Request | Response |
| --- | --- | --- | --- | --- |
| GET | `/api/auth` | none | | `AuthInfo` `{enabled, setup_required}` |
| GET | `/api/setup` | none | | `SetupStatus` `{required, code_required, web_username_pinned, web_password_pinned, soulseek_configured, soulseek_pinned}` |
| POST | `/api/setup` | setup | `SetupRequest` `{code?, web: {username, password}, soulseek?: {username, password}, shares?: [path]}` | 201 `Session`, sets the cookie; 409 once setup is done |
| POST | `/api/soulseek/test-login` | admin or setup | `Credentials` `{username, password}` | `LoginTestResult` `{ok, reason, message}`; 409 with `soulseek.connect = false` |
| GET | `/api/fs/dirs` | admin or setup | `?path=<absolute directory>` (default: home) | `DirectoryListing` `{path, parent, dirs: [{name, path}], truncated}` |
| GET | `/api/session` | readonly | | `Session` `{username, role}` |
| POST | `/api/session` | none | `LoginRequest` `{username, password}` | 201 `Session`, sets the cookie |
| DELETE | `/api/session` | none | | 204, clears the cookie |
| GET | `/api/application` | readonly | | `State` |
| POST | `/api/application/shutdown` | admin session | | 204 |
| POST | `/api/application/restart` | admin session | | 204 |
| GET | `/api/application/version` | readonly | | `VersionState` |
| GET | `/api/server` | readonly | | `ServerState` |
| POST | `/api/server/connect` | readwrite | | 204; 409 when no Soulseek credentials are set |
| POST | `/api/server/disconnect` | readwrite | optional `{message}` | 204 |
| GET | `/api/config` | readonly | | `Config`, the effective configuration with secrets as `*****` |
| GET | `/api/settings` | admin session | | `Settings` `{values, pinned, stored, secrets_set, pending_restart, api_keys}` |
| PATCH | `/api/settings` | admin session | sparse configuration object; `null` resets a key | `Settings` |
| DELETE | `/api/settings/{key}` | admin session | | `Settings`; `{key}` is dotted, e.g. `uploads.slots` |
| GET | `/api/api-keys` | admin session | | `[ApiKeyInfo]` |
| POST | `/api/api-keys` | admin session | `CreateApiKey` `{name, role?, allow?}` | 201 `CreatedApiKey`, the only time the key is shown |
| DELETE | `/api/api-keys/{name}` | admin session | | 204 |
| GET | `/api/events` | readonly | WebSocket upgrade | live events, see below |
| GET | `/api/events/history` | readonly | `?offset=0&limit=100` | `[Event]`, newest first; `X-Total-Count` header. The history keeps the newest 10000 events of the last 30 days |
| GET | `/api/logs` | admin | | `[LogRecord]`, the recent records kept in memory |
| GET | `/api/logs/files` | admin | | `[LogFile]` |
| GET | `/api/logs/files/{name}` | admin | | the log file, `text/plain` |
| GET | `/api/metrics` | readonly | | `CurrentMetrics` |
| GET | `/api/reports/transfers/summary` | readonly | `?start&end&direction&username` | `TransferSummaryReport` |
| GET | `/api/reports/transfers/histogram` | readonly | `?start&end&direction&username&buckets=100` | `TransferHistogram` |
| GET | `/api/reports/transfers/chart` | readonly | `?start&end&buckets=100` | `TransferHistogram` with bytes and counts only, with hour-aligned buckets |
| GET | `/api/reports/transfers/leaderboard` | readonly | `?direction&start&end&sort_by&sort_order&limit&offset` | `[TransferSummary]` |
| GET | `/api/reports/transfers/errors` | readonly | `?direction&start&end&username&sort_order&limit&offset` | `[TransferErrorDetail]` |
| GET | `/api/reports/transfers/errors/pareto` | readonly | `?direction&start&end&username&limit&offset` | `[TransferErrorSummary]` |
| GET | `/api/reports/transfers/directories` | readonly | `?start&end&username&limit&offset` | `[TransferDirectorySummary]` |
| GET | `/api/reports/transfers/users/{username}` | readonly | `?start&end` | `UserTransferReport` |
| GET | `/api/searches` | readonly | | `[Search]`, newest first |
| POST | `/api/searches` | readwrite | `SearchRequest` `{query, id?, timeout_ms?, ...}` | 201 `Search`; 409 unless logged in |
| GET | `/api/searches/{id}` | readonly | | `Search` |
| DELETE | `/api/searches/{id}` | readwrite | | 204; stops the search if it is running |
| POST | `/api/searches/{id}/cancel` | readwrite | | 204; the final state arrives as an event |
| GET | `/api/searches/{id}/responses` | readonly | | `[SearchResponse]`, in arrival order |
| GET | `/api/transfers/downloads` | readonly | `?username&include_removed=false&recent_completed&completed_offset=0&completed=all&query` | `[Transfer]` or 304 when a matching `If-None-Match` validator is sent |
| GET | `/api/transfers/downloads/summary` | readonly | `?username&include_removed=false&query` | `TransferListSummary` `{states: [{state, files, bytes}]}` |
| POST | `/api/transfers/downloads` | readwrite | `EnqueueDownloadsRequest` `{username, files: [{filename, size}], root?}`; with `root`, each file is saved as `<downloads>/<last component of root>/<path below root>`, and a file not below `root` is a 400 | 201/207/200 `EnqueuedDownloads` `{enqueued, failed, files}` |
| POST | `/api/transfers/downloads/folder` | readwrite | `EnqueueFolder` `{username, path, recursive=false}` | 201/207/200 `EnqueuedDownloads` for files saved as `<downloads>/<last component of path>/<path below it>`. Includes downloadable files in the peer's folder `path` and, with `recursive`, its subfolders. Returns 404 if the folder is missing or has no files, 409 if not logged in, and 504 if the peer does not answer. |
| DELETE | `/api/transfers/downloads/completed` | readwrite | | 204; removes every completed download from the list |
| GET | `/api/transfers/downloads/{id}` | readonly | | `Transfer` |
| DELETE | `/api/transfers/downloads/{id}` | readwrite | | 204; cancels if active and removes from the list |
| POST | `/api/transfers/downloads/{id}/cancel` | readwrite | | 204; cancels if active |
| GET | `/api/transfers/downloads/{id}/position` | readwrite | | `QueuePosition`, asked of the peer |
| GET | `/api/transfers/uploads` | readonly | `?username&include_removed=false&recent_completed&completed_offset=0&completed=all&query` | `[Transfer]` or 304 when a matching `If-None-Match` validator is sent |
| GET | `/api/transfers/uploads/summary` | readonly | `?username&include_removed=false&query` | `TransferListSummary` |
| DELETE | `/api/transfers/uploads/completed` | readwrite | | 204 |
| GET | `/api/transfers/uploads/{id}` | readonly | | `Transfer` |
| DELETE | `/api/transfers/uploads/{id}` | readwrite | | 204; cancels if active and removes from the list |
| POST | `/api/transfers/uploads/{id}/cancel` | readwrite | | 204 |
| POST | `/api/batches` | readwrite | `CreateBatch` `{username, files, id?, search_id?, destination?, external_id?}` | 201/207/200 `CreatedBatch` `{batch, failed}`; 409 if the id exists |
| GET | `/api/batches/{id}` | readonly | | `Batch`, with its transfers |
| GET | `/api/users/{username}/browse` | readwrite | | `BrowseResponse`, the user's shares (can take minutes) |
| GET | `/api/users/{username}/browse/status` | readonly | | `BrowseStatus` of a running browse; 404 when none |
| GET | `/api/users/{username}/directory` | readwrite | `?path=<remote directory>` | `[Directory]` |
| GET | `/api/users/{username}/endpoint` | readwrite | | `Endpoint` `{address, port}` |
| GET | `/api/users/{username}/info` | readwrite | | `Info` |
| GET | `/api/users/{username}/statistics` | readonly | | `Statistics` |
| GET | `/api/users/{username}/status` | readwrite | | `Status` |
| GET | `/api/conversations` | readonly | `?include_inactive=false&unacknowledged_only=false` | `[Conversation]` |
| GET | `/api/conversations/{username}` | readonly | `?include_messages=true` | `Conversation` |
| DELETE | `/api/conversations/{username}` | readwrite | | 204; closes it until a new message arrives |
| POST | `/api/conversations/{username}/ack` | readwrite | | 204; acknowledges every message |
| GET | `/api/conversations/{username}/messages` | readonly | `?unacknowledged_only=false` | `[PrivateMessage]` |
| POST | `/api/conversations/{username}/messages` | readwrite | `SendMessage` `{message}` | 201 `PrivateMessage` |
| POST | `/api/conversations/{username}/messages/{id}/ack` | readwrite | | 204 |
| GET | `/api/rooms` | readonly | | `[RoomInfo]`, the rooms on the server |
| GET | `/api/rooms/joined` | readonly | | `[string]`, joined room names |
| POST | `/api/rooms/joined` | readwrite | `JoinRoom` `{name}` | 201 `Room`; 200 `Room` when already joined |
| GET | `/api/rooms/joined/{room}` | readonly | | `Room` |
| DELETE | `/api/rooms/joined/{room}` | readwrite | | 204; leaves the room |
| GET | `/api/rooms/joined/{room}/users` | readonly | | `[RoomUser]` |
| POST | `/api/rooms/joined/{room}/members` | readwrite | `AddMember` `{username}` | 204; private rooms you own or operate |
| GET | `/api/rooms/joined/{room}/messages` | readonly | | `[RoomMessage]`, the recent messages |
| POST | `/api/rooms/joined/{room}/messages` | readwrite | `SendMessage` `{message}` | 204 |
| GET | `/api/rooms/joined/{room}/ticker` | readonly | | `Ticker` `{ticker}`, your ticker |
| PUT | `/api/rooms/joined/{room}/ticker` | readwrite | `Ticker` `{ticker}` | 204 |
| GET | `/api/shares` | readonly | | `[Share]` |
| POST | `/api/shares/scan` | readwrite | | 202; 409 while a scan runs |
| DELETE | `/api/shares/scan` | readwrite | | 204; 404 when no scan runs |
| GET | `/api/shares/{id}` | readonly | | `Share` |
| GET | `/api/shares/{id}/contents` | readonly | | `[Directory]`, as peers see them |
| GET | `/api/files/{root}` | readonly | `?path=&recursive=false` | `FilesystemDirectory`; `{root}` is `downloads` or `incomplete` |
| DELETE | `/api/files/{root}` | readwrite | `?path=` | 204; deletes a file or a directory tree below the root |

Outside `/api`: `GET {url_base}/health` answers `200` without authentication, and with
`metrics.enabled` the Prometheus exposition is served at `metrics.path` (Basic auth with
`metrics.username`/`metrics.password` when set).

With `recent_completed`, transfer lists return every active transfer and at most that
many completed transfers, newest first by end time, skipping the first `completed_offset`
(pages). `completed` picks which completed transfers: `all`, `succeeded`, `failed` (every
other final state) or `none`. `query` keeps transfers whose user or file name contains
the text, ignoring case. The summary counts every transfer in the list by state, as
stored; without `username` and `query` it reads counts kept as rows change, so it stays
cheap on lists with hundreds of thousands of rows.

Transfer lists include a weak `ETag` when the snapshot is stable. Clients that still
hold that snapshot can send the tag as `If-None-Match` on the same list URL and keep
the snapshot on a bodyless `304`. Lists with live transfers or changing elapsed time
return a fresh snapshot without a tag. The daemon tracks its own writes to each list
(downloads and uploads separately), so a change to one list leaves the other's tag valid.
Stop the daemon before editing its transfer database directly.

### Query parameters

`start` and `end` are RFC 3339 timestamps. `direction` is `download` or `upload`;
`sort_by` is `count`, `total_bytes` or `average_speed`; `sort_order` is `asc` or `desc`;
`limit` (default 25) and `offset` page lists. Booleans are `true` or `false`.

## Enums

| Type | Values |
| --- | --- |
| `TransferState` | `requested`, `queued_locally`, `queued_remotely`, `initializing`, `in_progress` (active); `succeeded`, `cancelled`, `timed_out`, `errored`, `rejected`, `aborted` (final) |
| `SearchState` | `requested`, `queued`, `in_progress` (active); `completed`, `timed_out`, `cancelled`, `response_limit_reached`, `file_limit_reached`, `errored` (final, saying why the search ended) |
| `ConnectionState` | `disconnected`, `connecting`, `connected`, `logging_in`, `logged_in`, `disconnecting` |
| `TransferDirection` | `download`, `upload` |
| `UserPresence` | `offline`, `away`, `online` |
| `MessageDirection` | `in`, `out` |
| `EventType` | `download_file_complete`, `download_directory_complete`, `upload_file_complete`, `private_message_received`, `room_message_received`, `server_connected`, `server_disconnected` |
| `Role` | `readonly`, `readwrite`, `admin` |
| `LoginTestFailure` | `invalid_password`, `invalid_username`, `server_full`, `server_private`, `invalid_version`, `rejected`, `unreachable`, `timeout`, `error` |

Transfer reports key their maps by direction and then by final `TransferState`
(`{"upload": {"succeeded": {...}}}`). Reports with explicit start and end times may
reuse a server snapshot for up to 60 seconds.

## Live events

`GET /api/events` upgrades to a WebSocket carrying JSON text frames. The client sends
`{"type": "subscribe", "topics": [...]}` or `{"type": "unsubscribe", "topics": [...]}`;
each subscription first delivers the topic's snapshot, then its changes, as
`{"type": "<event>", "data": ...}`.

| Topic | Snapshot | Then |
| --- | --- | --- |
| `state` | `state` (`State`) | `state`, at most four per second |
| `settings` | `settings` (`Config`, as `GET /api/config`) | `settings` on every change |
| `searches` | `search_list` (`[Search]`) | `search_created`, `search_updated`, `search_deleted` `{id}`, `search_responses` `{search_id, responses}` |
| `logs` (admin) | `log_buffer` (`[LogRecord]`) | `log` |
| `metrics` | `metrics` (`CurrentMetrics`) | `metrics`, every second |
| `transfers` | `transfer_list` (`[Transfer]`, the transfers the daemon is working on, both directions) | `transfer_updated` (`[Transfer]`, added or changed rows, at most four batches per second), `transfer_removed` `{ids}` |
| `conversations` | `conversation_list` (`[Conversation]`, as `GET /api/conversations`) | `conversation_message` (`PrivateMessage`, received or sent), `conversation_updated` (`Conversation` without messages) |
| `rooms` | `room_list` (`[Room]`, every joined room; empty while logged out) | `room_joined` (`Room`), `room_left` `{room}`, `room_message` (`RoomMessage`), `room_user_joined` `{room, user}`, `room_user_left` `{room, username}` |

Changes are applied in event order; a change can also arrive again right after the
snapshot that already includes it, so apply events idempotently (replace rows by id).

`transfers` carries rows exactly as the list routes return them. Its snapshot holds only
live transfers; fetch the list route for history, applying events received while that
request runs on top of its result (the newest event about a transfer is its current
state). A row with `removed` set, or an id in `transfer_removed`, has left the list
(removed, cleared, superseded by a new transfer of the same file, or expired by
retention). `conversation_updated` follows every message, acknowledgement and close.
Room tickers and operator changes have no events; reload the room for them.

An invalid command is answered with `{"type": "error", "data": {"message", "topic"}}`.
Close codes: `4401` sign in again, `4408` idle, `4429` too slow (reconnect and
resubscribe), `1001` shutdown. See [daemon internals](DAEMON.md#live-events-srcrealtime).

## Example

```sh
key=...   # created in System → Settings → API keys
api=http://127.0.0.1:5031/api
curl -s -H "X-API-Key: $key" -H 'Content-Type: application/json' \
  -d '{"query": "artist album"}' "$api/searches"
curl -s -H "X-API-Key: $key" "$api/searches/<id>/responses"
curl -s -H "X-API-Key: $key" -H 'Content-Type: application/json' \
  -d '{"username": "peer", "files": [{"filename": "@@share\\dir\\a.flac", "size": 123}]}' \
  "$api/transfers/downloads"
```
