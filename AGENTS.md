# Working on slskr

`slskr` is a Soulseek **client daemon** with a browser UI, not a replacement for the central Soulseek server. It started as a port of slskd but is its own project: its HTTP API, live events and configuration are designed here, not copied. On the network it must stay compatible with real Soulseek clients and servers, so preserve Soulseek.NET's observable wire behavior without copying its file layout. Use [README.md](README.md) for setup and run instructions.

## Where to work

| Area | Owner | Reference |
| --- | --- | --- |
| Soulseek wire format, connection lifecycle, peers, and transfers | `crates/soulseek`, a standalone Tokio library; see [protocol notes](docs/SOULSEEK_INTERNALS.md) | `ref/Soulseek.NET/src` and `ref/Soulseek.NET/tests` |
| HTTP and live event payloads | `crates/slskr-types`, shared by daemon and WASM; keep it free of I/O | [API reference](docs/API.md) |
| HTTP routes, auth, the events WebSocket, config, SQLite, and services | `crates/slskr`; see [daemon internals](docs/DAEMON.md) | `ref/slskd/src/slskd` for feature behavior |
| Leptos UI, browser API client, and styling | `crates/slskr-web`; see [architecture](docs/ARCHITECTURE.md#web-ui) | `ref/slskd/src/web/src` |

`ref/` contains gitignored upstream checkouts, not build inputs. If missing, clone the repos linked in the README and check out the revisions in [NOTICE](NOTICE). Consult upstream tests for wire changes; slskd is a reference for features, not for the API. The Rust code is authoritative for current implementation details; the design notes are not evidence of full parity.

- Keep wire/network types in `soulseek` and API types in `slskr-types`. Convert at the daemon boundary. Preserve Soulseek message bytes. The API follows the conventions in [docs/API.md](docs/API.md): routes under `/api`, `snake_case` JSON with enum values (no flag strings), RFC 3339 timestamps, `*_ms` durations, and `{"error": ...}` for every error. Treat routes, JSON fields, enum values and live event names as a contract for scripts: change them on purpose, and register routes with `http::Routes` and list them in `docs/API.md` (a test compares the two).
- Preserve non-root `web.url_base` behavior: SPA routes, asset URLs, API calls, and the events WebSocket must resolve under the prefix. Match upstream DOM classes where the vendored Semantic UI/slskd CSS depends on them.
- Expose only masked share paths to peers, never local paths. Do not resolve a peer request outside its configured share root. Never commit credentials, runtime databases, downloads, `ref/`, or build outputs.
- When delegating in omp, use native subagents rather than spawning `omp` processes. Give independent slices explicit file ownership; the orchestrator owns shared contracts, integration, and the final checks.

## Verify the surface you change

Run commands from the repository root in `nix develop`. Cargo's default members omit the web crate; use `--workspace` for the full suite.

| Change | Check |
| --- | --- |
| Protocol or client runtime | `cargo test --locked -p soulseek`; `cargo test --locked -p soulseek --test interop` specifically exercises two real Rust peers with an independent loopback server fixture (login, browse, search, resume). |
| Daemon HTTP or routing | `cargo test --locked -p slskr`: `--test http_smoke` starts the daemon with temporary config and exercises auth, JSON errors, compression, shares, and static/SPA routing; `web::tests` covers the embedded and `--web-dir` UI (caching, ETags, encodings, traversal); `bootstrap::tests::documented_routes_match_router` checks the route table in `docs/API.md`. |
| Web UI | `cargo check --locked -p slskr-web --target wasm32-unknown-unknown`; build with `(cd crates/slskr-web && NO_COLOR=true trunk build --frozen)` (writes workspace-root `dist/`), run the daemon with `--web-dir dist`, then exercise the changed workflow in a browser. |
| Packaging | `nix build .#slskr` builds the UI (`.#slskr-web`) and embeds it in the one binary (`SLSKR_EMBED_UI`). Start `result/bin/slskr` without `--web-dir` to test the embedded assets. Git flakes do not see untracked files: `git add -N` new files, or build a clean `path:` snapshot rather than importing the working tree's `ref/`, `target/`, or `dist/`. For the Dockerfile, `docker build .`. |

For browser smoke tests, use a temporary data directory whose `slskr.toml` sets `[soulseek] connect = false`, a loopback `[web] listen`, `url_base = "/client"`, and temporary `username`/`password`. Open a deep link such as `/client/searches`, sign in (cookie session), then reload an authenticated route. If packaging or routing changed, confirm the hashed CSS and WASM load with `Cache-Control: immutable` and that `index.html` carries `<base href="/client/">`.

The initial MVP was verified locally; public Soulseek login and transfer were not. Keep that check separate from the loopback suite. Run it only with an authorized test account configured outside the repository, using a controlled share and peer. Do not print credentials or ask for them in chat.
