//! Command line, application construction and HTTP server lifetime.

use std::{ffi::OsString, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::Response,
    routing::{any, get},
};
use slskr_types::config::Config;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::{
    app::{AppInner, AppState, Core, Paths, Services},
    auth::{self, Auth, SessionStore},
    config::{self, ConfigStore, Overrides, Sources},
    core_api, events, files,
    http::{self, Routes},
    logging, messaging,
    realtime::{self, EventBus},
    resolvers::AppResolvers,
    search, setup, shares,
    state::StateStore,
    telemetry, transfers, users, web,
};

pub async fn run(cli: Cli) -> Result<()> {
    let data_dir = std::path::absolute(cli.data_dir.map_or_else(default_data_dir, Ok)?)?;
    let config_file = std::path::absolute(
        cli.config
            .unwrap_or_else(|| data_dir.join(config::FILE_NAME)),
    )?;
    let web_dir = cli
        .web_dir
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(std::path::absolute)
        .transpose()?;
    if let Some(dir) = &web_dir {
        anyhow::ensure!(
            dir.join("index.html").is_file(),
            "--web-dir {}: index.html not found (build the UI with trunk first)",
            dir.display()
        );
    }
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

    let app = build(data_dir, config_file, web_dir, options).await?;

    let _cancel_on_drop = app.shutdown.clone().drop_guard();
    let router = router(&app);
    let client_events_done = CancellationToken::new();
    let _cancel_events_on_drop = client_events_done.clone().drop_guard();
    let client_events = app.start(client_events_done.clone());

    tracing::info!(address = %tcp.local_addr()?, "HTTP listener started");
    let shutdown = app.shutdown.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(
            tcp,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
    });

    let mut server_ended = false;
    let exit: Result<()> = tokio::select! {
        _ = app.shutdown.cancelled() => Ok(()),
        signal = termination_signal() => signal,
        result = &mut server => {
            server_ended = true;
            served(result)
        }
    };
    app.shutdown.cancel();
    app.disconnect("Application shut down").await;
    client_events_done.cancel();
    client_events
        .await
        .context("application event listener task")?;
    if !server_ended {
        // Open connections (the events WebSocket) get a moment to close cleanly.
        match tokio::time::timeout(Duration::from_secs(10), &mut server).await {
            Ok(result) => served(result)?,
            Err(_) => server.abort(),
        }
    }
    exit?;
    if app.restart_requested() {
        restart()?;
    }
    Ok(())
}

async fn build(
    data_dir: PathBuf,
    config_file: PathBuf,
    web_dir: Option<PathBuf>,
    options: ConfigStore,
) -> Result<AppState> {
    let startup = options.startup();
    let paths = Paths {
        config_file,
        db_dir: data_dir.join("db"),
        logs_dir: data_dir.join("logs"),
        web_dir,
        data_dir,
    };
    create_dirs(&paths, startup).await?;
    let bus = EventBus::new();
    let logs = logging::init(startup, &paths, bus.clone())?;
    warn_about_web_access(startup);
    match &paths.web_dir {
        Some(dir) => tracing::info!(dir = %dir.display(), "serving the web UI from a directory"),
        None if !web::EMBEDDED => tracing::warn!(
            "this build does not include the web UI; build it with trunk and pass --web-dir, or rebuild with SLSKR_EMBED_UI"
        ),
        None => {}
    }
    assemble(paths, options, bus, logs).await
}

async fn create_dirs(paths: &Paths, startup: &Config) -> Result<()> {
    for dir in [&paths.db_dir, &paths.logs_dir] {
        crate::create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    tokio::fs::create_dir_all(&startup.downloads.directory).await?;
    tokio::fs::create_dir_all(&startup.downloads.incomplete_directory).await?;
    Ok(())
}

/// Builds the core handles and every service (after logging is set up).
async fn assemble(
    paths: Paths,
    options: ConfigStore,
    bus: EventBus,
    logs: logging::LogBuffer,
) -> Result<AppState> {
    let startup = options.startup();
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
    let transfers = transfers::TransferService::new(&core, &events, &users, &shares)?;
    let telemetry = telemetry::TelemetryService::new(&core, &transfers)?;
    let search = search::SearchService::new(&core).await?;
    let messaging = messaging::MessagingService::new(&core, &events)?;
    let services = Services {
        events,
        users,
        shares,
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
    rest_routes().merge(socket_routes())
}

/// The JSON API, everything but the events WebSocket.
fn rest_routes() -> Routes {
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
        .merge(setup::routes())
}

fn socket_routes() -> Routes {
    Routes::new().get(slskr_types::realtime::PATH, events_socket)
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

pub(crate) fn router(app: &AppState) -> Router {
    use tower_http::compression::{CompressionLayer, CompressionLevel};
    use tower_http::decompression::RequestDecompressionLayer;
    let not_found = || async { http::error_response(StatusCode::NOT_FOUND, "") };
    // Compress JSON responses cheaply; the WebSocket is merged in after the layer.
    let compression = CompressionLayer::new()
        .gzip(true)
        .no_deflate()
        .no_zstd()
        .quality(CompressionLevel::Fastest);
    let api = rest_routes()
        .into_router()
        .route_layer(axum::middleware::from_fn(crate::profiling::http_timing))
        .route("/api", any(not_found))
        .route("/api/{*unknown}", any(not_found))
        .layer(RequestDecompressionLayer::new())
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            telemetry::reports::cache_response,
        ))
        .layer(axum::middleware::from_fn(http::json_errors))
        .layer(compression)
        .merge(
            socket_routes()
                .into_router()
                .layer(axum::middleware::from_fn(http::json_errors)),
        )
        // Until first-run setup is done, only the setup routes answer.
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            setup::gate,
        ));
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
    router = router.merge(web::router(app.paths.web_dir.clone(), base));
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

/// Warns when authentication is disabled. A missing web password is announced by
/// first-run setup (`setup::sync`), with the setup code.
fn warn_about_web_access(config: &Config) {
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
    }
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
    // `exec` only returns on failure; elsewhere the new process outlives this one.
    #[cfg(unix)]
    let started = {
        use std::os::unix::process::CommandExt;
        Err(process.exec())
    };
    #[cfg(not(unix))]
    let started = process.spawn().map(|_| ());
    started.context("restarting daemon")
}

/// The outcome of the HTTP server task.
fn served(result: Result<std::io::Result<()>, tokio::task::JoinError>) -> Result<()> {
    result
        .context("HTTP server task")?
        .context("serving HTTP requests")
}

/// An application on `data_dir` with `file` as `slskr.toml` and `env` as `SLSKR_*`
/// variables, without global logging or background tasks, for router tests.
#[cfg(test)]
pub(crate) async fn test_app(
    data_dir: &std::path::Path,
    file: &str,
    env: &[(&str, &str)],
) -> Result<AppState> {
    let config_file = data_dir.join(config::FILE_NAME);
    std::fs::write(&config_file, file)?;
    let vars = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned()));
    let sources = Sources::new(data_dir.into(), vars, Overrides::default())?;
    let options = ConfigStore::load(config_file.clone(), sources).await?;
    let paths = Paths {
        config_file,
        db_dir: data_dir.join("db"),
        logs_dir: data_dir.join("logs"),
        web_dir: None,
        data_dir: data_dir.into(),
    };
    create_dirs(&paths, options.startup()).await?;
    let bus = EventBus::new();
    let logs = logging::LogBuffer::new(bus.clone());
    assemble(paths, options, bus, logs).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
