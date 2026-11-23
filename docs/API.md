# HTTP API

The daemon's API, used by the web UI and by scripts. Every route below is served under
`{web.url_base}/api` (for example `http://127.0.0.1:5030/api/searches`, or
`/client/api/searches` with `web.url_base = "/client"`). The request and response types
are the Rust types in [`crates/slskr-types`](../crates/slskr-types/src); this page is the
authoritative list of routes, and a test (`bootstrap::tests::documented_routes_match_router`)
keeps it in step with the router.

## Conventions

- **JSON**: requests and responses are JSON (`Content-Type: application/json`). Field
  names and enum values are `snake_case`. Responses include every field, with `null` for
  absent values; request fields marked optional may be left out. Responses are compressed
  with brotli or gzip when the request's `Accept-Encoding` allows it.
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
  low or a cross-site request; `404` no such route or resource; `405` method not allowed;
  `409` conflicting state (already exists, not logged in to Soulseek, a pinned setting);
  `413` body too large; `415` missing JSON content type; `422` well-formed JSON of the
  wrong shape; `429` another enqueue request is running; `500` an unexpected failure;
  `503` the server connection dropped while a room request was pending; `504` a peer or
  the server did not answer in time.

## Authentication

A request is authenticated by one of:

- the `slskr_session` cookie, set by `POST /api/session` (browsers). Requests other than
  `GET`/`HEAD`/`OPTIONS` that carry the cookie must be same-origin: an `Origin` header
  naming another host is refused with `403`.
- an API key in the `X-API-Key` header (scripts). Keys are created in the web UI (System →
  Options) or with `POST /api/api-keys`, or configured in `slskr.toml`
  (`[[web.api_keys]]`); see [configuration](CONFIGURATION.md#web).
- nothing, when `web.auth = false`: every request acts as an administrator.

Roles, lowest first: `readonly` (read everything except logs and settings), `readwrite`
(also act: search, download, chat, connect), `admin` (also logs). Routes marked
**admin session** need a signed-in administrator and refuse API keys. **none** routes
need no authentication.

## Routes

| Method | Path | Role | Request | Response |
| --- | --- | --- | --- | --- |
| GET | `/api/auth` | none | | `AuthInfo` `{enabled}` |
| GET | `/api/session` | readonly | | `Session` `{username, role}` |
| POST | `/api/session` | none | `LoginRequest` `{username, password}` | 201 `Session`, sets the cookie |
| DELETE | `/api/session` | none | | 204, clears the cookie |
| GET | `/api/application` | readonly | | `State` |
| POST | `/api/application/shutdown` | admin session | | 204 |
| POST | `/api/application/restart` | admin session | | 204 |
| GET | `/api/application/version` | readonly | | `VersionState` |
| POST | `/api/application/version/check` | readwrite | | `VersionState`, after asking GitHub for the latest release |
| GET | `/api/server` | readonly | | `ServerState` |
| POST | `/api/server/connect` | readwrite | | 204; 409 when no Soulseek credentials are set |
| POST | `/api/server/disconnect` | readwrite | optional `{message}` | 204 |
| GET | `/api/config` | readonly | | `Config`, the effective configuration with secrets as `*****` |
| GET | `/api/settings` | admin session | | `Settings` |
| PATCH | `/api/settings` | admin session | sparse configuration object; `null` resets a key | `Settings` |
| DELETE | `/api/settings/{key}` | admin session | | `Settings`; `{key}` is dotted, e.g. `uploads.slots` |
| GET | `/api/api-keys` | admin session | | `[ApiKeyInfo]` |
| POST | `/api/api-keys` | admin session | `CreateApiKey` `{name, role?, allow?}` | 201 `CreatedApiKey`, the only time the key is shown |
| DELETE | `/api/api-keys/{name}` | admin session | | 204 |
| GET | `/api/events` | readonly | WebSocket upgrade | live events, see below |
| GET | `/api/events/history` | readonly | `?offset=0&limit=100` | `[Event]`, newest first; `X-Total-Count` header |
| GET | `/api/logs` | admin | | `[LogRecord]`, the recent records kept in memory |
| GET | `/api/logs/files` | admin | | `[LogFile]` |
| GET | `/api/logs/files/{name}` | admin | | the log file, `text/plain` |
| GET | `/api/metrics` | readonly | | `CurrentMetrics` |
| GET | `/api/reports/transfers/summary` | readonly | `?start&end&direction&username` | `TransferSummaryReport` |
| GET | `/api/reports/transfers/histogram` | readonly | `?start&end&direction&username&buckets=100` | `TransferHistogram` |
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
| GET | `/api/transfers/downloads` | readonly | `?username&include_removed=false` | `[Transfer]` |
| POST | `/api/transfers/downloads` | readwrite | `EnqueueDownloads` `{username, files: [{filename, size}]}` | 201/207/200 `EnqueuedDownloads` |
| DELETE | `/api/transfers/downloads/completed` | readwrite | | 204; removes every completed download from the list |
| GET | `/api/transfers/downloads/{id}` | readonly | | `Transfer` |
| DELETE | `/api/transfers/downloads/{id}` | readwrite | | 204; cancels if active and removes from the list |
| POST | `/api/transfers/downloads/{id}/cancel` | readwrite | | 204; cancels if active |
| GET | `/api/transfers/downloads/{id}/position` | readwrite | | `QueuePosition`, asked of the peer |
| GET | `/api/transfers/uploads` | readonly | `?username&include_removed=false` | `[Transfer]` |
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

Transfer reports key their maps by direction and then by final `TransferState`
(`{"upload": {"succeeded": {...}}}`).

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

An invalid command is answered with `{"type": "error", "data": {"message", "topic"}}`.
Close codes: `4401` sign in again, `4408` idle, `4429` too slow (reconnect and
resubscribe), `1001` shutdown. See [daemon internals](DAEMON.md#live-events-srcrealtime).

## Example

```sh
key=...   # created in System → Options → API keys
api=http://127.0.0.1:5030/api
curl -s -H "X-API-Key: $key" -H 'Content-Type: application/json' \
  -d '{"query": "artist album"}' "$api/searches"
curl -s -H "X-API-Key: $key" "$api/searches/<id>/responses"
curl -s -H "X-API-Key: $key" -H 'Content-Type: application/json' \
  -d '{"username": "peer", "files": [{"filename": "@@share\\dir\\a.flac", "size": 123}]}' \
  "$api/transfers/downloads"
```
