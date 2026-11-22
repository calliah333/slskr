//! Command line, application construction and HTTP server lifetime.

mod tls;

use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    http::StatusCode,
    routing::{any, get},
    serve::{Listener, ListenerExt},
};
use clap::{
    Arg, ArgAction, ArgMatches, Command, CommandFactory, FromArgMatches, Parser, value_parser,
};
use serde_yaml::Mapping;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{AppInner, AppState, Core, Hubs, Paths, Services},
    auth::{Auth, Role},
    config::{self, OptionsStore, SOURCES},
    core_api, events, files, http, integrations, logging, messaging, relay,
    resolvers::AppResolvers,
    search, shares,
    signalr::Hub,
    state::StateStore,
    telemetry, transfers, users,
};

pub struct Cli {
    app_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    overrides: Mapping,
    envars: bool,
    generate_secret: Option<usize>,
    generate_cert: bool,
}

impl Parser for Cli {}

impl CommandFactory for Cli {
    fn command() -> Command {
        let mut command = Command::new("slskr")
            .version(env!("CARGO_PKG_VERSION"))
            .about("Soulseek client daemon")
            .arg(
                Arg::new("app-dir")
                    .long("app-dir")
                    .short('a')
                    .value_parser(value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("config")
                    .long("config")
                    .short('c')
                    .value_parser(value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("set")
                    .long("set")
                    .value_name("KEY=VALUE")
                    .action(ArgAction::Append),
            )
            .arg(
                Arg::new("envars")
                    .long("envars")
                    .short('e')
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("generate-cert")
                    .long("generate-cert")
                    .short('g')
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("generate-secret")
                    .long("generate-secret")
                    .value_parser(value_parser!(usize)),
            );
        for &(env, name, short, _, kind) in SOURCES {
            let mut arg = Arg::new(name)
                .long(name)
                .help(format!("Override SLSKD_{env}"));
            if let Some(short) = short {
                arg = arg.short(short);
            }
            arg = match kind {
                'b' => arg.action(ArgAction::SetTrue),
                'i' => arg
                    .allow_negative_numbers(true)
                    .value_parser(value_parser!(i32)),
                'l' => arg.action(ArgAction::Append).value_delimiter(';'),
                _ => arg,
            };
            command = command.arg(arg);
        }
        command
    }

    fn command_for_update() -> Command {
        Self::command()
    }
}

impl FromArgMatches for Cli {
    fn from_arg_matches(matches: &ArgMatches) -> std::result::Result<Self, clap::Error> {
        let mut overrides = Mapping::new();
        for &(_, name, _, path, kind) in SOURCES {
            if matches.value_source(name) != Some(clap::parser::ValueSource::CommandLine) {
                continue;
            }
            match kind {
                'b' => config::cli_value(&mut overrides, path, matches.get_flag(name)),
                'i' => config::cli_value(&mut overrides, path, matches.get_one::<i32>(name)),
                'l' => config::cli_value(
                    &mut overrides,
                    path,
                    matches
                        .get_many::<String>(name)
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>(),
                ),
                _ => config::cli_value(&mut overrides, path, matches.get_one::<String>(name)),
            }
        }
        for assignment in matches.get_many::<String>("set").into_iter().flatten() {
            config::cli_assignment(&mut overrides, assignment).map_err(|e| {
                clap::Error::raw(clap::error::ErrorKind::InvalidValue, e.to_string())
            })?;
        }
        Ok(Self {
            app_dir: matches.get_one::<PathBuf>("app-dir").cloned(),
            config: matches.get_one::<PathBuf>("config").cloned(),
            overrides,
            envars: matches.get_flag("envars"),
            generate_secret: matches.get_one::<usize>("generate-secret").copied(),
            generate_cert: matches.get_flag("generate-cert"),
        })
    }

    fn update_from_arg_matches(
        &mut self,
        matches: &ArgMatches,
    ) -> std::result::Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    if cli.envars {
        for name in ["APP_DIR", "CONFIG"]
            .into_iter()
            .chain(SOURCES.iter().map(|source| source.0))
        {
            println!("SLSKD_{name}");
        }
        return Ok(());
    }
    if let Some(len) = cli.generate_secret {
        use rand::distr::{Alphanumeric, SampleString};
        ensure!(len > 0, "secret length must be positive");
        println!("{}", Alphanumeric.sample_string(&mut rand::rng(), len));
        return Ok(());
    }
    if cli.generate_cert {
        use rand::distr::{Alphanumeric, SampleString};
        let password = Alphanumeric.sample_string(&mut rand::rng(), 32);
        let filename = Path::new("slskd.pfx");
        tls::generate_pfx(filename, &password)?;
        println!("Certificate: {}\nPassword: {password}", filename.display());
        return Ok(());
    }

    let app_dir = cli
        .app_dir
        .or_else(|| config::env_path("APP_DIR").map(PathBuf::from))
        .map_or_else(default_app_dir, Ok)?;
    let app_dir = std::path::absolute(app_dir)?;
    let config_file = cli
        .config
        .or_else(|| config::env_path("CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| app_dir.join("slskd.yml"));
    let config_file = std::path::absolute(config_file)?;
    let options = OptionsStore::load(&app_dir, config_file.clone(), cli.overrides).await?;
    if options.startup().flags.no_start {
        return Ok(());
    }
    let web = &options.startup().web;
    let tcp = bind_tcp(web.ip_address.as_deref(), web.port).await?;
    let (https, acceptor) = if web.https.disabled {
        (Vec::new(), None)
    } else {
        (
            bind_tcp(web.https.ip_address.as_deref(), web.https.port).await?,
            Some(tls::acceptor(&web.https.certificate)?),
        )
    };
    let redirect = if web.https.force {
        let port = https
            .first()
            .context("forced HTTPS requires an HTTPS listener")?
            .local_addr()?
            .port();
        Some(tls::redirect(port))
    } else {
        None
    };
    let socket = web.socket.as_deref().filter(|path| !path.is_empty());
    #[cfg(unix)]
    let unix = socket.map(UnixListener::bind).transpose()?;
    #[cfg(not(unix))]
    ensure!(
        socket.is_none(),
        "Unix sockets are not supported on this platform"
    );

    let startup_key: [u8; 32] = rand::random();
    let app = build(app_dir, config_file, options, &startup_key).await?;
    let _cancel_on_drop = app.shutdown.clone().drop_guard();
    let router = router(&app);
    let plain_router = redirect.unwrap_or_else(|| router.clone());
    let client_events_done = CancellationToken::new();
    let _cancel_events_on_drop = client_events_done.clone().drop_guard();
    let client_events = app.start(startup_key, client_events_done.clone());

    let mut servers = JoinSet::new();
    for listener in tcp {
        tracing::info!(address = %listener.local_addr()?, "HTTP listener started");
        serve(
            &mut servers,
            listener,
            plain_router.clone(),
            app.shutdown.clone(),
        );
    }
    if let Some(acceptor) = acceptor {
        for listener in https {
            tracing::info!(address = %listener.local_addr()?, "HTTPS listener started");
            serve(
                &mut servers,
                tls::TlsListener::new(listener, acceptor.clone()),
                router.clone(),
                app.shutdown.clone(),
            );
        }
    }
    #[cfg(unix)]
    if let Some(listener) = unix {
        tracing::info!(path = %listener.path.display(), "HTTP Unix listener started");
        serve(&mut servers, listener, plain_router, app.shutdown.clone());
    }

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

async fn bind_tcp(ips: Option<&str>, port: i32) -> Result<Vec<TcpListener>> {
    let port = port.try_into()?;
    let mut listeners = Vec::new();
    for ip in config::http_ips(ips)? {
        let address = SocketAddr::new(ip, port);
        listeners.push(
            TcpListener::bind(address)
                .await
                .with_context(|| format!("binding {address}"))?,
        );
    }
    Ok(listeners)
}

async fn build(
    app_dir: PathBuf,
    config_file: PathBuf,
    options: OptionsStore,
    startup_key: &[u8],
) -> Result<AppState> {
    let startup = options.startup();
    let exe = std::env::current_exe()?;
    let paths = Paths {
        config_file,
        data_dir: app_dir.join("data"),
        logs_dir: app_dir.join("logs"),
        content_path: (!startup.headless)
            .then(|| discover_content(&exe, &startup.web.content_path))
            .flatten(),
        app_dir,
    };
    tokio::fs::create_dir_all(&paths.data_dir).await?;
    tokio::fs::create_dir_all(&startup.directories.downloads).await?;
    tokio::fs::create_dir_all(&startup.directories.incomplete).await?;
    let logs = logging::init(startup, &paths)?;
    let hubs = Hubs {
        application: Hub::new("application"),
        logs: Hub::new("logs"),
        search: Hub::new("search"),
        metrics: Hub::new("metrics"),
        relay: Hub::new("relay"),
    };
    let auth = Auth::new(config::auth_config(startup, startup_key)?);
    for hub in [&hubs.application, &hubs.search, &hubs.metrics] {
        hub.set_authorizer(auth.hub_authorizer(Role::ReadOnly));
    }
    hubs.logs
        .set_authorizer(auth.hub_authorizer(Role::Administrator));
    logs.attach_hub(hubs.logs.clone());
    let state = StateStore::new(hubs.application.clone());
    let concurrency = startup.throttling.search.incoming.concurrency.try_into()?;
    let resolvers = Arc::new(AppResolvers::new(concurrency));
    let client = soulseek::SoulseekClient::new(config::client_options(startup)?, resolvers.clone());
    let core = Core {
        paths,
        options,
        state,
        client,
        hubs,
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
    let relay = relay::RelayService::new(&core, &shares, &transfers)?;
    let integrations = integrations::IntegrationService::new(&core, &events)?;
    let services = Services {
        events,
        users,
        shares,
        files,
        transfers,
        telemetry,
        search,
        messaging,
        relay,
        integrations,
    };
    let app = AppState(Arc::new(AppInner::new(core, services)));
    resolvers.bind(&app);
    Ok(app)
}

fn router(app: &AppState) -> Router {
    let api = Router::new()
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
        .merge(relay::routes());
    let missing = || async { StatusCode::NOT_FOUND };
    let mut endpoints = Router::new()
        .nest("/api/v0", api)
        .route("/api", any(missing))
        .route("/api/{*unknown}", any(missing))
        .route("/health", get(|| async { StatusCode::OK }))
        .nest("/hub/application", app.hubs.application.router())
        .nest("/hub/logs", app.hubs.logs.router())
        .nest("/hub/search", app.hubs.search.router())
        .nest("/hub/metrics", app.hubs.metrics.router())
        .nest("/hub/relay", app.hubs.relay.router())
        .route("/hub", any(missing))
        .route("/hub/{*unknown}", any(missing));
    let startup = app.options.startup();
    if startup.metrics.enabled {
        let url = format!("/{}", startup.metrics.url.trim_start_matches('/'));
        endpoints = endpoints.merge(telemetry::prometheus_routes(&url));
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
    if web.logging {
        router = router.layer(tower_http::trace::TraceLayer::new_for_http());
    }
    router
}

fn default_app_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "XDG_DATA_HOME"
    })
    .filter(|path| !path.is_empty())
    {
        return Ok(PathBuf::from(path).join("slskd"));
    }
    let home = std::env::var_os("HOME").context("set SLSKD_APP_DIR or pass --app-dir")?;
    Ok(PathBuf::from(home)
        .join(if cfg!(target_os = "macos") {
            "Library/Application Support"
        } else {
            ".local/share"
        })
        .join("slskd"))
}

fn discover_content(exe: &Path, configured: &str) -> Option<PathBuf> {
    let path = Path::new(configured);
    let sibling = exe.parent()?.join(path);
    if sibling.join("index.html").is_file() {
        return Some(sibling);
    }
    if configured == "wwwroot" {
        let installed = exe.parent()?.parent()?.join("share/slskr/wwwroot");
        if installed.join("index.html").is_file() {
            return Some(installed);
        }
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

#[cfg(unix)]
struct UnixListener {
    socket: tokio::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl UnixListener {
    fn bind(path: &str) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let socket = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))?;
        Ok(Self { socket, path })
    }
}

#[cfg(unix)]
impl Drop for UnixListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
impl Listener for UnixListener {
    type Io = tokio::net::UnixStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.socket.accept().await {
                Ok((stream, _)) => return (stream, unix_peer_addr()),
                Err(error) => {
                    tracing::warn!(%error, "Unix socket accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(unix_peer_addr())
    }
}

#[cfg(unix)]
fn unix_peer_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reference_flags_and_overrides() {
        let cli = Cli::try_parse_from([
            "slskr",
            "-a",
            "/tmp/slskr",
            "-l",
            "5090",
            "--no-connect",
            "--slsk-username",
            "peer",
            "--shared",
            "/music;/audiobooks",
            "--set",
            "web.url_base=/client",
        ])
        .unwrap();
        let values = serde_yaml::Value::Mapping(cli.overrides);
        assert_eq!(cli.app_dir.as_deref(), Some(Path::new("/tmp/slskr")));
        assert_eq!(values["flags"]["noConnect"].as_bool(), Some(true));
        assert_eq!(values["web"]["port"].as_i64(), Some(5090));
        assert_eq!(values["web"]["urlBase"].as_str(), Some("/client"));
        assert_eq!(values["soulseek"]["username"].as_str(), Some("peer"));
        assert_eq!(
            values["shares"]["directories"].as_sequence().unwrap().len(),
            2
        );
        assert!(Cli::try_parse_from(["slskr", "--set", "missing-value"]).is_err());
    }

    #[test]
    fn discover_configured_and_packaged_content() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("bin/slskr");
        let root = dir.path().join("share/slskr/wwwroot");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("index.html"), "shell").unwrap();
        assert_eq!(discover_content(&executable, "wwwroot"), Some(root.clone()));
        assert_eq!(
            discover_content(&executable, root.to_str().unwrap()),
            Some(root)
        );
        assert_eq!(discover_content(&executable, "nonexistent"), None);
    }
}
