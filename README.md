# slskr

A Rust Soulseek client daemon with a browser UI. `soulseek` is a standalone Tokio
library, `slskr` serves the daemon and the `/api/v0` API, and `slskr-web` builds the
Leptos UI to WebAssembly. It connects to an existing Soulseek server; it does not
replace the central Soulseek server.

## Run on NixOS

Put a `slskd.yml` in your app directory. Set `soulseek.username` and
`soulseek.password` for your network account, and set separate
`web.authentication.username` and `web.authentication.password` values for the
browser. To inspect the UI without connecting to Soulseek, set
`flags.no_connect: true`. The default HTTP listener is `127.0.0.1:5030`.

```sh
nix run . -- --app-dir "$HOME/.local/share/slskr"
```

Open `http://127.0.0.1:5030`. The flake packages the daemon and UI together.
Configure `shares.directories` to share files and `soulseek.listen_port` for
incoming peers. Use only directories you intend to share. Keep credentials out
of Git; the daemon's app directory also holds downloads and local SQLite data.

## Develop

```sh
nix develop
cargo test --locked --workspace
(cd crates/slskr-web && NO_COLOR=true trunk build --frozen)
SLSKR_CONTENT_PATH="$PWD/dist" cargo run --locked -p slskr -- --app-dir ./data
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
