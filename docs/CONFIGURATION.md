# Configuration reference

Every setting slskr understands. Most people change these in the web UI (System →
Settings), which saves them to `settings.json` in the data directory. To keep a setting
in a file instead, put it in `slskr.toml` (see [`slskr.example.toml`](../slskr.example.toml));
for containers and service managers, use the environment variable shown.

## Layers

Settings are merged from these layers, lowest precedence first:

1. built-in defaults (the tables below);
2. `<data_dir>/settings.json`, written by the daemon when settings are changed from the
   UI or `PATCH /api/settings`;
3. `slskr.toml` in the data directory (or the file given with `--config`);
4. `SLSKR_<SECTION>_<KEY>` environment variables;
5. command-line flags (`--no-connect` sets `soulseek.connect = false`).

A key set by `slskr.toml`, the environment or the command line is **pinned**: the UI
shows it as locked and the settings API refuses to change it (409). Remove it from the
file or the environment to manage it from the UI again.

`slskr.toml` is strict: an unknown key or invalid value stops the daemon at startup and
is ignored (with a warning) when the file changes while it runs. `settings.json` is
lenient: unknown keys and invalid values in it are logged and skipped, so it can never
prevent startup. Do not edit `settings.json` by hand; it is replaced on every change.

## Values

- Durations are strings such as `"30s"`, `"5m"`, `"1h30m"`, `"7d"` or `"2w"` (units `ms`,
  `s`, `m`, `h`, `d`, `w`). Where a duration is optional, `""` means never.
- Relative paths resolve against the data directory.
- Environment values are typed like the default: booleans accept `true/false/1/0/yes/no`,
  lists are comma-separated (`SLSKR_SHARES_DIRECTORIES="/music,[Books]/srv/books"`).
- **Restart** marks keys that only take effect after a restart; the UI shows when one is
  pending. Everything else applies immediately.
- Secrets (`soulseek.password`, `soulseek.proxy` when it has credentials, `web.password`,
  `metrics.password`, API keys) are never returned by the API. A web password set from the
  UI is stored as an argon2id hash; `soulseek.password` must be stored as given to log in,
  so `settings.json` is created with mode 0600.

## `[soulseek]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `soulseek.username` | `""` | `SLSKR_SOULSEEK_USERNAME` | | Soulseek account; empty runs without connecting. |
| `soulseek.password` | `""` | `SLSKR_SOULSEEK_PASSWORD` | | Soulseek password (secret). |
| `soulseek.server` | `"server.slsknet.org:2242"` | `SLSKR_SOULSEEK_SERVER` | | Server as `host:port`. |
| `soulseek.listen_address` | `"0.0.0.0"` | `SLSKR_SOULSEEK_LISTEN_ADDRESS` | | Address for incoming peer connections. |
| `soulseek.listen_port` | `50300` | `SLSKR_SOULSEEK_LISTEN_PORT` | | Port for incoming peer connections (1024–65535). |
| `soulseek.description` | `""` | `SLSKR_SOULSEEK_DESCRIPTION` | | Shown to peers viewing your user info. |
| `soulseek.picture` | `""` | `SLSKR_SOULSEEK_PICTURE` | | Image file shown in your user info; empty for none. |
| `soulseek.connect` | `true` | `SLSKR_SOULSEEK_CONNECT` | yes | `false` starts without connecting (same as `--no-connect`). |
| `soulseek.proxy` | `""` | `SLSKR_SOULSEEK_PROXY` | | SOCKS5 proxy, `socks5://[user:pass@]host:port`; empty for none. |
| `soulseek.distributed_network` | `true` | `SLSKR_SOULSEEK_DISTRIBUTED_NETWORK` | | Take part in the distributed search network. |
| `soulseek.distributed_children` | `25` | `SLSKR_SOULSEEK_DISTRIBUTED_CHILDREN` | | Maximum distributed children; 0 accepts none. |
| `soulseek.rooms` | `[]` | `SLSKR_SOULSEEK_ROOMS` | | Chat rooms joined after every login. |

## `[web]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `web.listen` | `"127.0.0.1:5031"` | `SLSKR_WEB_LISTEN` | yes | Address of the web UI and API. |
| `web.url_base` | `"/"` | `SLSKR_WEB_URL_BASE` | yes | Path prefix behind a reverse proxy, e.g. `"/slskr"`. |
| `web.username` | `"admin"` | `SLSKR_WEB_USERNAME` | | Web sign-in name. Changing it signs out every browser. |
| `web.password` | `""` | `SLSKR_WEB_PASSWORD` | | Web sign-in password; while no password is set anywhere, the daemon waits for first-run setup (see below). Changing it signs out every browser. |
| `web.auth` | `true` | `SLSKR_WEB_AUTH` | yes | `false` disables authentication: loopback or development only. The UI then answers only on `localhost` or an IP address, not on a host name. |
| `web.session_ttl` | `"30d"` | `SLSKR_WEB_SESSION_TTL` | | How long a browser stays signed in; each use extends it (5m–365d). |
| `web.log_requests` | `false` | `SLSKR_WEB_LOG_REQUESTS` | yes | Log every HTTP request at info level. |
| `web.api_keys` | `[]` | file only | | API keys for the `X-API-Key` header, see below. |

slskr serves plain HTTP. Terminate TLS at a reverse proxy, have it send
`X-Forwarded-Proto: https` (the session cookie is then marked `Secure`) and pass the
original `Host` header (browser requests that change state or open a WebSocket are refused
when their `Origin` differs from `Host`).

**First-run setup.** With `web.auth` enabled and no `web.password` in any layer (none set
from the UI, in the file or in the environment), nobody can sign in: the API answers
`503 setup required` and the daemon logs a one-time setup code and the URL
`http://<web.listen><web.url_base>/setup`. Setup saves the web username and password (as
a hash in `settings.json`), and optionally `soulseek.username`, `soulseek.password` and
`shares.directories`, like the settings API: a key pinned by the file or the environment
can only be sent with its current value. To skip setup, set `web.password` in
`slskr.toml` or `SLSKR_WEB_PASSWORD` (do not set it to an empty string: that pins an empty
password, and setup can not replace it). Behind a reverse proxy on the same machine, pass
the original `Host` or `X-Forwarded-For`, or every visitor counts as local and needs no
setup code. See [the API reference](API.md#first-run-setup).

API keys are created in the UI (`POST /api/api-keys`), which shows the key
once and stores only its SHA-256, or written in `slskr.toml` (pinned; they can not be
deleted from the UI):

```toml
[[web.api_keys]]
name = "soularr"
key = "16 to 255 characters"
role = "readwrite"            # readonly | readwrite | admin
allow = ["0.0.0.0/0", "::/0"] # networks allowed to use the key
```

## `[downloads]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `downloads.directory` | `"downloads"` | `SLSKR_DOWNLOADS_DIRECTORY` | | Completed downloads. |
| `downloads.incomplete_directory` | `"incomplete"` | `SLSKR_DOWNLOADS_INCOMPLETE_DIRECTORY` | | Partial downloads. |
| `downloads.slots` | `0` | `SLSKR_DOWNLOADS_SLOTS` | yes | Concurrent downloads; 0 is unlimited. |
| `downloads.speed_limit` | `0` | `SLSKR_DOWNLOADS_SPEED_LIMIT` | | KiB/s; 0 is unlimited. |
| `downloads.retry_attempts` | `3` | `SLSKR_DOWNLOADS_RETRY_ATTEMPTS` | | Attempts per file. |
| `downloads.retry_delay` | `"5s"` | `SLSKR_DOWNLOADS_RETRY_DELAY` | | First retry backoff, doubling up to `retry_max_delay`. |
| `downloads.retry_max_delay` | `"5m"` | `SLSKR_DOWNLOADS_RETRY_MAX_DELAY` | | Longest retry backoff. |
| `downloads.resume` | `true` | `SLSKR_DOWNLOADS_RESUME` | | Resume partial files on retry instead of starting over. |
| `downloads.on_conflict` | `"rename"` | `SLSKR_DOWNLOADS_ON_CONFLICT` | | When the destination exists: `rename` or `overwrite`. |
| `downloads.subdirectory` | `"${SOURCE_DIRECTORY}"` | `SLSKR_DOWNLOADS_SUBDIRECTORY` | | Subdirectory per download; empty for none. Tokens: `${SOURCE_DIRECTORY}`, `${SOURCE_PATH}`, `${SOURCE_USERNAME}`, `${BATCH_ID}`, `${BATCH_EXTERNAL_ID}`, `${SEARCH_ID}`, `${SEARCH_TEXT}`. |
| `downloads.file_mode` | `""` | `SLSKR_DOWNLOADS_FILE_MODE` | | Mode of completed files, octal (`"0644"`) or symbolic (`"rw-r--r--"`); empty keeps the process default. |

## `[uploads]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `uploads.slots` | `20` | `SLSKR_UPLOADS_SLOTS` | yes | Concurrent uploads, at least 1. |
| `uploads.speed_limit` | `0` | `SLSKR_UPLOADS_SPEED_LIMIT` | | KiB/s; 0 is unlimited. |

Per-user limits; 0 is unlimited. A group's own non-zero limits take precedence.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `uploads.limits.queued_files` | `0` | `SLSKR_UPLOADS_LIMITS_QUEUED_FILES` | | Files queued per user. |
| `uploads.limits.queued_megabytes` | `0` | `SLSKR_UPLOADS_LIMITS_QUEUED_MEGABYTES` | | Megabytes queued per user. |
| `uploads.limits.daily_files` | `0` | `SLSKR_UPLOADS_LIMITS_DAILY_FILES` | | Files per user per day. |
| `uploads.limits.daily_megabytes` | `0` | `SLSKR_UPLOADS_LIMITS_DAILY_MEGABYTES` | | Megabytes per user per day. |
| `uploads.limits.weekly_files` | `0` | `SLSKR_UPLOADS_LIMITS_WEEKLY_FILES` | | Files per user per week. |
| `uploads.limits.weekly_megabytes` | `0` | `SLSKR_UPLOADS_LIMITS_WEEKLY_MEGABYTES` | | Megabytes per user per week. |
| `uploads.limits.daily_failures` | `0` | `SLSKR_UPLOADS_LIMITS_DAILY_FAILURES` | | Failed uploads per user per day. |
| `uploads.limits.weekly_failures` | `0` | `SLSKR_UPLOADS_LIMITS_WEEKLY_FAILURES` | | Failed uploads per user per week. |

## `[shares]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `shares.directories` | `[]` | `SLSKR_SHARES_DIRECTORIES` | | Shared directories. `[Alias]/path` sets the name peers see (default: the last path component); `!/path` excludes a subtree of another share. |
| `shares.exclude` | `[]` | `SLSKR_SHARES_EXCLUDE` | | Case-insensitive regexes on full local paths; matches are skipped. |
| `shares.rescan_interval` | `""` | `SLSKR_SHARES_RESCAN_INTERVAL` | | Rescan when the last scan is older than this; `""` scans only on startup (when no cached scan matches) or on request. |

## `[groups]`

Upload queue groups. Privileged users always go first, then lower `priority` numbers.
Users in no group are in `default`; users sharing too little are in `leechers`. Any other
`[groups.<name>]` is user-defined. `groups.default.*` and `groups.leechers.*` can be set
from the environment (`SLSKR_GROUPS_DEFAULT_PRIORITY`, `SLSKR_GROUPS_LEECHERS_SLOTS`);
user-defined groups only in the UI or the file.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `groups.<name>.priority` | `500` (`leechers`: `999`) | `SLSKR_GROUPS_DEFAULT_PRIORITY` | | Lower is served first. |
| `groups.<name>.strategy` | `"roundrobin"` | `SLSKR_GROUPS_DEFAULT_STRATEGY` | | `roundrobin` or `firstinfirstout`. |
| `groups.<name>.slots` | `20` | `SLSKR_GROUPS_DEFAULT_SLOTS` | | Upload slots for the group; 0 leaves only the global limit. |
| `groups.<name>.speed_limit` | `0` | `SLSKR_GROUPS_DEFAULT_SPEED_LIMIT` | | KiB/s; 0 is unlimited. |
| `groups.<name>.limits` | all `0` | `SLSKR_GROUPS_DEFAULT_LIMITS_QUEUED_FILES`, … | | The keys of `uploads.limits`; non-zero values override them for the group. |
| `groups.<name>.members` | `[]` | — | | Usernames; user-defined groups only, a user may be in one group. |
| `groups.<name>.min_shared_files` | `1` | `SLSKR_GROUPS_LEECHERS_MIN_SHARED_FILES` | | `leechers` only: users sharing fewer files are leechers. |
| `groups.<name>.min_shared_directories` | `1` | `SLSKR_GROUPS_LEECHERS_MIN_SHARED_DIRECTORIES` | | `leechers` only: users sharing fewer directories are leechers. |

## `[blacklist]`

Users refused searches, browsing and transfers. Failed and cancelled uploads to blacklisted
users are deleted from the transfer history when the blacklist changes and at startup;
their successful uploads stay for the reports.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `blacklist.users` | `[]` | `SLSKR_BLACKLIST_USERS` | | Usernames, case-insensitive. |
| `blacklist.patterns` | `[]` | `SLSKR_BLACKLIST_PATTERNS` | | Username wildcards with `*` and `?`, case-insensitive. |
| `blacklist.cidrs` | `[]` | `SLSKR_BLACKLIST_CIDRS` | | IP ranges; when set, search responses and downloads also check the peer's address. |
| `blacklist.ban_upload_loops` | `true` | `SLSKR_BLACKLIST_BAN_UPLOAD_LOOPS` | | Adds a user to `blacklist.users` once five of their files have each been accepted and then dropped before any data 20 times over at least 24 hours, with no upload to them succeeding meanwhile. Unreachable or slow peers, refusals and connections lost mid-file never count; the own account and privileged users are never banned. Each ban is logged and can be undone by removing the name. |

## `[search]`

Incoming search requests from the network.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `search.incoming_concurrency` | `10` | `SLSKR_SEARCH_INCOMING_CONCURRENCY` | yes | Searches answered concurrently. |
| `search.incoming_circuit_breaker` | `500` | `SLSKR_SEARCH_INCOMING_CIRCUIT_BREAKER` | | Queued searches beyond this are dropped. |
| `search.response_file_limit` | `500` | `SLSKR_SEARCH_RESPONSE_FILE_LIMIT` | | Most files returned per response. |
| `search.ignore_queries` | `[]` | `SLSKR_SEARCH_IGNORE_QUERIES` | | Case-insensitive regexes; matching searches are ignored. |

## `[retention]`

How long finished records are kept; `""` keeps them forever.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `retention.searches` | `"7d"` | `SLSKR_RETENTION_SEARCHES` | | Completed searches (at least 5m). |
| `retention.uploads_succeeded` | `""` | `SLSKR_RETENTION_UPLOADS_SUCCEEDED` | | |
| `retention.uploads_errored` | `""` | `SLSKR_RETENTION_UPLOADS_ERRORED` | | |
| `retention.uploads_cancelled` | `""` | `SLSKR_RETENTION_UPLOADS_CANCELLED` | | |
| `retention.uploads_failed` | `""` | `SLSKR_RETENTION_UPLOADS_FAILED` | | |
| `retention.downloads_succeeded` | `""` | `SLSKR_RETENTION_DOWNLOADS_SUCCEEDED` | | |
| `retention.downloads_errored` | `""` | `SLSKR_RETENTION_DOWNLOADS_ERRORED` | | |
| `retention.downloads_cancelled` | `""` | `SLSKR_RETENTION_DOWNLOADS_CANCELLED` | | |
| `retention.downloads_failed` | `""` | `SLSKR_RETENTION_DOWNLOADS_FAILED` | | |
| `retention.logs` | `"30d"` | `SLSKR_RETENTION_LOGS` | | Log files under `<data_dir>/logs`. |

## `[logging]`

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `logging.level` | `"info"` | `SLSKR_LOGGING_LEVEL` | yes | `trace`, `debug`, `info`, `warn` or `error`; `RUST_LOG` takes precedence. |
| `logging.file` | `true` | `SLSKR_LOGGING_FILE` | yes | Write daily log files under `<data_dir>/logs`. |

## `[metrics]`

Prometheus exposition.

| Key | Default | Environment | Restart | Description |
| --- | --- | --- | --- | --- |
| `metrics.enabled` | `false` | `SLSKR_METRICS_ENABLED` | yes | Serve metrics at `metrics.path`. |
| `metrics.path` | `"/metrics"` | `SLSKR_METRICS_PATH` | yes | Outside the `api`, `health` and `web.url_base` roots. |
| `metrics.username` | `""` | `SLSKR_METRICS_USERNAME` | yes | Basic auth; set both or neither. |
| `metrics.password` | `""` | `SLSKR_METRICS_PASSWORD` | yes | Basic auth password (secret). |

## Process settings

Not configuration keys, so never pinned (`slskr --help` lists them):

| Flag | Environment | Description |
| --- | --- | --- |
| `--data-dir DIR` | `SLSKR_DATA_DIR` | Data directory; default `$XDG_DATA_HOME/slskr` or `~/.local/share/slskr`. |
| `--config FILE` | `SLSKR_CONFIG` | Configuration file; default `<data_dir>/slskr.toml`. |
| `--no-connect` | | Start without connecting to Soulseek (`soulseek.connect = false`). |
| `--web-dir DIR` | `SLSKR_WEB_DIR` | Serve the web UI from a trunk build instead of the copy in the binary (UI development). |
| | `RUST_LOG` | Log filter; overrides `logging.level`. |

## Settings API

All routes require an administrator signed in to the web UI (API keys are refused).

- `GET /api/settings` returns `{values, pinned, stored, secrets_set, pending_restart,
  api_keys}`: the merged configuration without secrets or API keys, the pinned dotted
  keys, the dotted keys saved in `settings.json` (those `DELETE` can return to their
  default; a list is one key), the secrets that have a value, whether a restart is
  pending, and the API keys as `{name, role, allow, pinned}`.
- `PATCH /api/settings` takes any subset of `values`, e.g.
  `{"uploads": {"slots": 5}}`, and returns the same body as `GET`. Objects are merged,
  other values replace; `null` resets a key. An absent secret is unchanged;
  `web.password` takes the new password in plain text. A pinned key answers 409, an
  invalid value 400 with a message naming the key.
- `DELETE /api/settings/{key}` resets one dotted key or section to its default.
- `GET /api/api-keys` lists the keys as `{name, role, allow, pinned}`.
- `POST /api/api-keys` with `{name, role, allow}` creates a key and answers 201 with
  `{name, role, allow, key}`; the key is not shown again.
  `DELETE /api/api-keys/{name}` deletes it.

Errors are `{"error": "<message>"}`; see [the API reference](API.md) for every route.
