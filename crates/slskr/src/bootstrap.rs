//! Command line, application construction and HTTP server lifetime.

use std::{
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::Response,
    routing::{any, get},
    serve::{Listener, ListenerExt},
};
use clap::Parser;
use slskr_types::config::Config;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{AppInner, AppState, Core, Paths, Services},
    auth::{self, Auth, SessionStore},
    config::{self, ConfigStore, Overrides, Sources},
    core_api, events, files,
    http::{self, Routes},
    logging, messaging,
    realtime::{self, EventBus},
    resolvers::AppResolvers,
    search, shares,
    state::StateStore,
    telemetry, transfers, users,
};

/// Command line. Everything else is configured in the web UI, `slskr.toml` or `SLSKR_*`
/// variables.
#[derive(Debug, Parser)]
#[command(
    name = "slskr",
    version,
    about = "Soulseek client daemon with a web UI"
)]
pub struct Cli {
    /// Data directory holding the configuration, databases, logs and default download
    /// directories [default: $XDG_DATA_HOME/slskr or ~/.local/share/slskr]
    #[arg(long, value_name = "DIR", env = "SLSKR_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Configuration file [default: <DATA_DIR>/slskr.toml]
    #[arg(long, value_name = "FILE", env = "SLSKR_CONFIG")]
    pub config: Option<PathBuf>,
    /// Do not connect to the Soulseek server (overrides soulseek.connect)
    #[arg(long)]
    pub no_connect: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    let data_dir = std::path::absolute(cli.data_dir.map_or_else(default_data_dir, Ok)?)?;
    let config_file = std::path::absolute(
        cli.config
            .unwrap_or_else(|| data_dir.join(config::FILE_NAME)),
    )?;
    let sources = Sources::from_env(
        data_dir.clone(),
        Overrides {
            no_connect: cli.no_connect,
        },
    )?;
    let options = ConfigStore::load(config_file.clone(), sources).await?;
    let listen = options.startup().web.listen;
    let tcp = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding web.listen {listen}"))?;

    let app = build(data_dir, config_file, options).await?;

    let _cancel_on_drop = app.shutdown.clone().drop_guard();
    let router = router(&app);
    let client_events_done = CancellationToken::new();
    let _cancel_events_on_drop = client_events_done.clone().drop_guard();
    let client_events = app.start(client_events_done.clone());

    let mut servers = JoinSet::new();
    tracing::info!(address = %tcp.local_addr()?, "HTTP listener started");
    serve(&mut servers, tcp, router, app.shutdown.clone());

    let exit: Result<()> = tokio::select! {
        _ = app.shutdown.cancelled() => Ok(()),
        signal = termination_signal() => signal,
        result = servers.join_next() => match result {
            Some(Ok(result)) => result.context("serving HTTP requests"),
            Some(Err(error)) => Err(error).context("HTTP server task"),
            None => Ok(()),
        }
    };
    app.shutdown.cancel();
    app.disconnect("Application shut down").await;
    client_events_done.cancel();
    client_events
        .await
        .context("application event listener task")?;
    match tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = servers.join_next().await {
            result??;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    {
        Ok(drained) => drained?,
        Err(_) => servers.shutdown().await,
    }
    exit?;
    if app.restart_requested() {
        restart()?;
    }
    Ok(())
}

async fn build(data_dir: PathBuf, config_file: PathBuf, options: ConfigStore) -> Result<AppState> {
    let startup = options.startup();
    let exe = std::env::current_exe()?;
    let paths = Paths {
        config_file,
        db_dir: data_dir.join("db"),
        logs_dir: data_dir.join("logs"),
        content_path: discover_content(&exe, std::env::var_os("SLSKR_CONTENT_PATH")),
        data_dir,
    };
    tokio::fs::create_dir_all(&paths.db_dir).await?;
    tokio::fs::create_dir_all(&startup.downloads.directory).await?;
    tokio::fs::create_dir_all(&startup.downloads.incomplete_directory).await?;
    let bus = EventBus::new();
    let logs = logging::init(startup, &paths, bus.clone())?;
    warn_about_web_access(startup, &paths.config_file);
    if paths.content_path.is_none() {
        tracing::warn!(
            "web UI files not found (set SLSKR_CONTENT_PATH to a directory containing index.html)"
        );
    }
    let current = options.get();
    let sessions = SessionStore::open(
        &paths.db_dir.join("sessions.db"),
        current.web.session_ttl,
        &current.web.username,
        &auth::credential_secret(&current.web.password, &current.web.password_hash),
    )
    .await
    .context("opening sessions database")?;
    let auth =
        Auth::new(config::auth_config(&current), sessions).with_url_base(&startup.web.url_base);
    let state = StateStore::new();
    let concurrency = startup.search.incoming_concurrency.try_into()?;
    let resolvers = Arc::new(AppResolvers::new(concurrency));
    let client = soulseek::SoulseekClient::new(config::client_options(startup)?, resolvers.clone());
    let core = Core {
        paths,
        options,
        state,
        client,
        bus,
        auth,
        logs,
        shutdown: CancellationToken::new(),
    };

    let events = events::EventService::new(&core)?;
    let users = users::UserService::new(&core)?;
    let shares = shares::ShareService::new(&core)?;
    let files = files::FileService::new(&core)?;
    let transfers = transfers::TransferService::new(&core, &events, &users, &shares)?;
    let telemetry = telemetry::TelemetryService::new(&core, &transfers)?;
    let search = search::SearchService::new(&core).await?;
    let messaging = messaging::MessagingService::new(&core, &events)?;
    let services = Services {
        events,
        users,
        shares,
        files,
        transfers,
        telemetry,
        search,
        messaging,
    };
    let app = AppState(Arc::new(AppInner::new(core, services)));
    resolvers.bind(&app);
    Ok(app)
}

/// Every API route, with paths starting at `/api` (below `web.url_base`). `docs/API.md`
/// documents each of them; a test keeps the two in step.
pub fn api_routes() -> Routes {
    Routes::new()
        .merge(core_api::routes())
        .merge(logging::routes())
        .merge(events::routes())
        .merge(users::routes())
        .merge(shares::routes())
        .merge(files::routes())
        .merge(transfers::routes())
        .merge(telemetry::routes())
        .merge(search::routes())
        .merge(messaging::routes())
        .get(slskr_types::realtime::PATH, events_socket)
}

/// `GET /api/events`: the live events WebSocket.
async fn events_socket(State(app): State<AppState>, request: Request) -> Response {
    realtime::socket::EventsEndpoint::new(
        app.auth.clone(),
        app.bus.clone(),
        Arc::new(app.clone()),
        app.shutdown.clone(),
        realtime::Limits::default(),
    )
    .upgrade(request)
    .await
}

fn router(app: &AppState) -> Router {
    let not_found = || async { http::error_response(StatusCode::NOT_FOUND, "") };
    let api = api_routes()
        .into_router()
        .route("/api", any(not_found))
        .route("/api/{*unknown}", any(not_found))
        .layer(axum::middleware::from_fn(http::json_errors));
    let mut endpoints = Router::new()
        .merge(api)
        .route("/health", get(|| async { StatusCode::OK }));
    let startup = app.options.startup();
    if startup.metrics.enabled {
        endpoints = endpoints.merge(telemetry::prometheus_routes(&startup.metrics.path));
    }
    let endpoints = endpoints.with_state(app.clone());
    let web = &startup.web;
    let base = web.url_base.trim_end_matches('/');
    // Like UsePathBase, keep root endpoints reachable for proxies that strip the prefix.
    let mut router = if base.is_empty() {
        endpoints
    } else {
        endpoints.clone().nest(base, endpoints)
    };
    if let Some(path) = &app.paths.content_path {
        router = router.merge(http::static_router(path.clone(), base));
    }
    router = router.layer(axum::middleware::from_fn_with_state(
        app.auth.clone(),
        auth::refresh_session_cookie,
    ));
    if web.log_requests {
        use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
        // Log at INFO like slskd request logging; the span records the path only so query
        // strings never reach the logs.
        router = router.layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        path = %request.uri().path(),
                    )
                })
                .on_request(DefaultOnRequest::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        );
    }
    router
}

fn default_data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "XDG_DATA_HOME"
    })
    .filter(|path| !path.is_empty())
    {
        return Ok(PathBuf::from(path).join("slskr"));
    }
    let home = std::env::var_os("HOME").context("set HOME or pass --data-dir")?;
    Ok(PathBuf::from(home)
        .join(if cfg!(target_os = "macos") {
            "Library/Application Support"
        } else {
            ".local/share"
        })
        .join("slskr"))
}

fn warn_about_web_access(config: &Config, file: &Path) {
    let web = &config.web;
    if !web.auth {
        // Process logs only go to the log buffer and files, so say this on stderr as well.
        eprintln!(
            "slskr: WARNING: web.auth is disabled; {} is unauthenticated",
            web.listen
        );
        if web.listen.ip().is_loopback() {
            tracing::warn!(
                listen = %web.listen,
                "web.auth is disabled: anyone who can reach the web UI has full control"
            );
        } else {
            tracing::error!(
                listen = %web.listen,
                "web.auth is disabled on a non-loopback address: anyone on the network has full control of this daemon"
            );
        }
    } else if web.password.is_empty() && web.password_hash.is_empty() {
        tracing::warn!(
            file = %file.display(),
            "web.password is not set; web sign-in is disabled until it is configured"
        );
    }
}

/// `SLSKR_CONTENT_PATH`, else `wwwroot` next to the executable, else the packaged
/// `../share/slskr/wwwroot` (and `dist/` in debug builds).
fn discover_content(exe: &Path, configured: Option<OsString>) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        return path.join("index.html").is_file().then_some(path);
    }
    let bin = exe.parent()?;
    let installed = [
        bin.join("wwwroot"),
        bin.parent()?.join("share/slskr/wwwroot"),
    ];
    if let Some(found) = installed
        .into_iter()
        .find(|path| path.join("index.html").is_file())
    {
        return Some(found);
    }
    // Development convenience for `cargo run` after `trunk build`.
    #[cfg(debug_assertions)]
    {
        let dist = Path::new("dist");
        if dist.join("index.html").is_file() {
            return Some(dist.to_owned());
        }
    }
    None
}

async fn termination_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn restart() -> Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut process = std::process::Command::new(std::env::current_exe()?);
    process.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        return Err(process.exec()).context("restarting daemon");
    }
    #[cfg(not(unix))]
    process.spawn().context("restarting daemon").map(|_| ())
}

fn serve<L: Listener<Addr = SocketAddr>>(
    tasks: &mut JoinSet<std::io::Result<()>>,
    listener: L,
    router: Router,
    shutdown: CancellationToken,
) {
    tasks.spawn(async move {
        // Axum supplies Connected for TapIo listeners with their declared peer address.
        axum::serve(
            listener.tap_io(|_| {}),
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_line() {
        let cli = Cli::try_parse_from([
            "slskr",
            "--data-dir",
            "/tmp/slskr",
            "--config",
            "/etc/slskr.toml",
            "--no-connect",
        ])
        .unwrap();
        assert_eq!(cli.data_dir.as_deref(), Some(Path::new("/tmp/slskr")));
        assert_eq!(cli.config.as_deref(), Some(Path::new("/etc/slskr.toml")));
        assert!(cli.no_connect);
        for removed in [
            &["slskr", "--app-dir", "/x"][..],
            &["slskr", "--set", "a=b"],
            &["slskr", "-g"],
        ] {
            assert!(Cli::try_parse_from(removed).is_err(), "{removed:?}");
        }
    }

    /// `docs/API.md` is the route reference for script authors: it must list exactly the
    /// routes the router serves.
    #[test]
    fn documented_routes_match_router() {
        use std::collections::BTreeSet;
        let documented: BTreeSet<(String, String)> = include_str!("../../../docs/API.md")
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                let method = *cells.get(1)?;
                let path = cells.get(2)?.strip_prefix('`')?.strip_suffix('`')?;
                (!method.is_empty()
                    && method.bytes().all(|b| b.is_ascii_uppercase())
                    && path.starts_with("/api"))
                .then(|| (method.to_owned(), path.to_owned()))
            })
            .collect();
        let routes = api_routes();
        let registered: BTreeSet<(String, String)> = routes
            .table()
            .iter()
            .map(|(method, path)| (method.to_string(), path.clone()))
            .collect();
        assert_eq!(
            registered.len(),
            routes.table().len(),
            "a route is registered twice"
        );
        let undocumented: Vec<_> = registered.difference(&documented).collect();
        let unrouted: Vec<_> = documented.difference(&registered).collect();
        assert!(
            undocumented.is_empty() && unrouted.is_empty(),
            "missing from docs/API.md: {undocumented:?}; documented but not routed: {unrouted:?}"
        );
    }

    #[test]
    fn discover_configured_and_packaged_content() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("bin/slskr");
        let root = dir.path().join("share/slskr/wwwroot");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "shell").unwrap();
        assert_eq!(discover_content(&executable, None), Some(root.clone()));
        assert_eq!(
            discover_content(&executable, Some(root.clone().into())),
            Some(root)
        );
        assert_eq!(
            discover_content(&executable, Some(dir.path().join("nonexistent").into())),
            None
        );
    }
}
