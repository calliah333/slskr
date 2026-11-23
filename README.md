# slskr

A Rust Soulseek client daemon with a browser UI. `soulseek` is a standalone Tokio
library, `slskr` serves the daemon and the `/api/v0` API, and `slskr-web` builds the
Leptos UI to WebAssembly. It connects to an existing Soulseek server; it does not
replace the central Soulseek server.

## Run on NixOS

slskr keeps its configuration, databases, logs and default download directories in a
data directory: `$XDG_DATA_HOME/slskr` (usually `~/.local/share/slskr`), or the one
given with `--data-dir`. Configuration is read from `slskr.toml` there (or the file
given with `--config`); without one, slskr starts with defaults. Copy
[`slskr.example.toml`](slskr.example.toml), which documents every key, and set at least:

```toml
[soulseek]
username = "your-soulseek-account"
password = "..."

[web]
username = "admin"
password = "a separate password for the browser"
```

Then start it:

```sh
nix run . -- --data-dir "$HOME/.local/share/slskr"
```

Open `http://127.0.0.1:5030` (`web.listen`). The flake packages the daemon and UI
together. Configure `shares.directories` to share files and `soulseek.listen_port` for
incoming peers. Use only directories you intend to share. To inspect the UI without
connecting to Soulseek, pass `--no-connect` (or set `soulseek.connect = false`).

Every scalar or list key can also come from the environment as
`SLSKR_<SECTION>_<KEY>`, for example `SLSKR_SOULSEEK_PASSWORD` or `SLSKR_WEB_LISTEN`
(lists are comma-separated). Environment variables override the file, and
command-line flags override both. Relative paths in the file resolve against the data
directory. Changes saved to the file, by hand or from System → Options in the UI, apply
without a restart except for the web listener, URL base, authentication switch,
logging, metrics, slot counts and incoming search concurrency; the UI shows when a
restart is pending. Keep credentials out of Git.

## Develop

```sh
nix develop
cargo test --locked --workspace
(cd crates/slskr-web && NO_COLOR=true trunk build --frozen)
SLSKR_CONTENT_PATH="$PWD/dist" cargo run --locked -p slskr -- --data-dir ./data
```

Protocol and transfer tests use loopback fixtures and do not require a public
Soulseek account. See [architecture](docs/ARCHITECTURE.md) for the crate layout.
For repository conventions, upstream references, and targeted checks, see
[AGENTS.md](AGENTS.md).

## Attribution

This is a modified version of slskd. It is not maintained by, endorsed by, or
affiliated with the slskd project or its author(s).

This is a modified version of Soulseek.NET. It is not maintained by, endorsed by,
or affiliated with the Soulseek.NET project or its author(s).

The gitignored reference checkouts are [`ref/slskd`](https://github.com/slskd/slskd)
and [`ref/Soulseek.NET`](https://github.com/jpdillingham/Soulseek.NET). See
[NOTICE](NOTICE), [LICENSE](LICENSE), and the [upstream notices](licenses/) for
source revisions, attribution, and license terms.
