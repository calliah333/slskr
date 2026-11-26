# slskr

slskr is a self-hosted [Soulseek](https://www.slsknet.org) client: a daemon that stays
connected to the Soulseek network, shares your files, and lets you search, download and
chat from a web UI. It is a single binary with the UI built in, written in Rust, and
started as a port of [slskd](https://github.com/slskd/slskd).

![Overview page: live speeds and queue, 30-day upload history](docs/screenshots/overview.png)

<table>
  <tr>
    <td><img src="docs/screenshots/uploads.png" alt="Uploads grouped by user and folder, dark theme"></td>
    <td><img src="docs/screenshots/settings.png" alt="Settings page with the Soulseek account section"></td>
  </tr>
</table>

## Performance

Numbers from one real instance (i7-8700K, NixOS) sharing 29,978 files (0.57 TiB), taken from
the profile report slskr writes to its log every 15 minutes. Over 16.5 hours on
2026-11-26 it finished 4,280 uploads (100.6 GiB):

| | |
| --- | --- |
| CPU | 3.4% of one core (median), 6.6% at peak |
| Memory (RSS) | 72–138 MiB |
| All-time Overview reports over 1.2 M transfer rows | about 70–300 ms each |
| Binary with the UI embedded | 30 MB |
| Web UI (WASM) | 3.5 MB, 1.4 MB gzipped |

## Quick start

### Docker Compose

```sh
docker compose up -d
docker compose logs slskr   # shows the setup code
```

Open <http://localhost:5031/setup> and complete [first-run setup](#first-run) with the
setup code from the log. Settings, databases and downloads live in the `slskr-data` volume
(`/data`). To share music, uncomment the bind mount in
[`docker-compose.yml`](docker-compose.yml) and add `/music` to the shares. Forward port
50300 on your router so other users can connect to you.

### Nix

```sh
nix run . -- --data-dir ~/.local/share/slskr
```

`nix build .#slskr` puts the same single binary in `result/bin/slskr`. On the first start
it prints the setup URL at `127.0.0.1:5031`. See [First run](#first-run).

### From source

Needs Rust, the `wasm32-unknown-unknown` target, [trunk](https://trunkrs.dev) and
`wasm-bindgen-cli` 0.2.130 (`nix develop` provides all of them).

```sh
(cd crates/slskr-web && trunk build --release)
SLSKR_EMBED_UI="$PWD/dist" cargo build --release -p slskr --features embed-ui
./target/release/slskr --help
```

## First run

A fresh install has no web password, so nobody can sign in yet and the API answers
`503 setup required`. The daemon prints a one-time setup code and the setup URL
(`http://<web.listen><web.url_base>/setup`) to its log and stderr. Open the URL (any page
of the UI leads there while setup is required), choose the web username and password, and
optionally enter your Soulseek account and the directories to share. From the daemon's own machine no code is needed; from anywhere else
(including a browser outside a Docker container) enter the code. Wrong codes are
rate-limited. Testing a Soulseek login with an unused username registers that account.

Setting `web.password` in `slskr.toml` or `SLSKR_WEB_PASSWORD` skips setup; so does
`web.auth = false`. See [docs/API.md](docs/API.md#first-run-setup) for the details.

## Configuration

Most settings are changed in the web UI (Settings: the gear button at the top right).
Things you want to keep in a file or set before the first start (web credentials, the
listen address, the URL base behind a reverse proxy) go in `slskr.toml` in the data directory
or in `SLSKR_<SECTION>_<KEY>` environment variables; anything set there is locked in the UI.
Start from [`slskr.example.toml`](slskr.example.toml); [docs/CONFIGURATION.md](docs/CONFIGURATION.md)
lists every key.

```toml
[web]
username = "admin"
password = "a password for the web UI"
```

## Development

```sh
nix develop
cargo test --locked --workspace

# UI work: rebuild on change in one terminal, serve the result in another
(cd crates/slskr-web && trunk watch)
cargo run -p slskr -- --data-dir ./data --web-dir dist --no-connect
```

A plain `cargo build` embeds a placeholder page instead of the UI; `--web-dir`
(`SLSKR_WEB_DIR`) serves a trunk build from disk, so UI changes need only a
rebuild and a browser reload. See [architecture](docs/ARCHITECTURE.md) for the crate
layout and [AGENTS.md](AGENTS.md) for conventions and checks.

## API

Everything the UI does goes through a JSON API under `/api`, with live updates on one
WebSocket. Create an API key in the UI to script it; [docs/API.md](docs/API.md) lists
every route.

## Attribution

This is a modified version of slskd. It is not maintained by, endorsed by, or
affiliated with the slskd project or its author(s).

This is a modified version of Soulseek.NET. It is not maintained by, endorsed by,
or affiliated with the Soulseek.NET project or its author(s).

The gitignored reference checkouts are [`ref/slskd`](https://github.com/slskd/slskd)
and [`ref/Soulseek.NET`](https://github.com/jpdillingham/Soulseek.NET). See
[NOTICE](NOTICE), [LICENSE](LICENSE), and the [upstream notices](licenses/) for
source revisions, attribution, and license terms. slskr is licensed under the
AGPL-3.0-or-later.
