//! `slskr.toml`: loading, validation, saving from the UI, and file watching.
//!
//! Precedence, lowest first: built-in defaults, the configuration file, `SLSKR_<SECTION>_<KEY>`
//! environment variables, then command-line flags. Relative paths resolve against the data
//! directory. The file is polled for changes; [`restart_required`] decides which changes only
//! take effect after a restart.

use std::{
    collections::{BTreeSet, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use slskr_api::config::{ApiKeyRole, Config, LogLevel, RESERVED_GROUPS};
use soulseek::{
    ClientOptions, ClientOptionsPatch, ConnectionOptions, DiagnosticLevel, ProxyOptions,
};
use tokio::{io::AsyncWriteExt, sync::watch};
use tokio_util::sync::CancellationToken;
use toml::{Table, Value};

use crate::auth::{ApiKey, AuthConfig, Role, parse_ranges};

/// Configuration file name inside the data directory.
pub const FILE_NAME: &str = "slskr.toml";
/// Prefix of configuration environment variables.
pub const ENV_PREFIX: &str = "SLSKR_";
/// `SLSKR_*` process variables that are not configuration keys.
pub const RESERVED_ENV: [&str; 3] = ["SLSKR_DATA_DIR", "SLSKR_CONFIG", "SLSKR_CONTENT_PATH"];
/// Lifetime of issued web tokens.
pub(crate) const TOKEN_TTL: Duration = Duration::from_secs(7 * 86_400);

/// Command-line settings that override the file and environment.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub no_connect: bool,
}

/// Every layer except the file, captured once at startup so reloads are reproducible.
#[derive(Clone, Debug)]
pub struct Sources {
    data_dir: PathBuf,
    env: Table,
    overrides: Overrides,
}

impl Sources {
    /// Reads `SLSKR_*` variables from the process environment.
    pub fn from_env(data_dir: PathBuf, overrides: Overrides) -> Result<Self> {
        let mut vars = Vec::new();
        for (name, value) in std::env::vars_os() {
            let Some(name) = name.to_str().filter(|n| n.starts_with(ENV_PREFIX)) else {
                continue;
            };
            let value = value
                .into_string()
                .map_err(|_| anyhow!("{name} is not valid UTF-8"))?;
            vars.push((name.to_owned(), value));
        }
        Self::new(data_dir, vars, overrides)
    }

    pub fn new(
        data_dir: PathBuf,
        vars: impl IntoIterator<Item = (String, String)>,
        overrides: Overrides,
    ) -> Result<Self> {
        Ok(Self {
            data_dir,
            env: env_layer(vars)?,
            overrides,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Builds the effective configuration from `file` (the file's text, if it exists).
    pub fn resolve(&self, file: Option<&str>) -> Result<Config> {
        let mut table = Table::new();
        if let Some(text) = file {
            // Deserializing the text directly reports errors with line and column.
            if let Err(error) = toml::from_str::<Config>(text) {
                return Err(
                    match error.span().and_then(|span| key_at(text, span.start)) {
                        Some(key) => anyhow!("{key}: {error}"),
                        None => error.into(),
                    },
                );
            }
            table = toml::from_str(text)?;
        }
        merge(&mut table, self.env.clone());
        let mut config: Config = Value::Table(table)
            .try_into()
            .context("invalid configuration")?;
        if self.overrides.no_connect {
            config.soulseek.connect = false;
        }
        normalize(&mut config, &self.data_dir);
        validate(&config)?;
        Ok(config)
    }
}

#[derive(Clone)]
pub struct ConfigStore(Arc<Inner>);

struct Inner {
    path: PathBuf,
    sources: Sources,
    startup: Arc<Config>,
    sender: watch::Sender<Arc<Config>>,
    disk: tokio::sync::Mutex<()>,
}

impl ConfigStore {
    /// Loads `path`, starting from defaults when it does not exist.
    pub async fn load(path: PathBuf, sources: Sources) -> Result<Self> {
        let text = read_optional(&path).await?;
        let config = sources
            .resolve(text.as_deref())
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(Self::from_config(path, sources, config))
    }

    /// A store whose current and startup configuration is `config`.
    pub fn from_config(path: PathBuf, sources: Sources, config: Config) -> Self {
        let startup = Arc::new(config);
        Self(Arc::new(Inner {
            path,
            sources,
            sender: watch::channel(startup.clone()).0,
            startup,
            disk: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn get(&self) -> Arc<Config> {
        self.0.sender.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.0.sender.subscribe()
    }

    /// The configuration the process started with.
    pub fn startup(&self) -> &Config {
        &self.0.startup
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// The file's text; empty when it does not exist yet.
    pub fn read_toml(&self) -> io::Result<String> {
        match std::fs::read_to_string(&self.0.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            result => result,
        }
    }

    /// `None` when `text` would be accepted by [`Self::apply_toml`].
    pub fn validate_toml(&self, text: &str) -> Option<String> {
        ensure_document(text)
            .and_then(|()| self.0.sources.resolve(Some(text)))
            .err()
            .map(|e| format!("{e:#}"))
    }

    /// Validates and saves `text`, keeping the previous file as `<file>.bak`, then applies
    /// it. Comments are kept, since the text is written as given.
    pub async fn apply_toml(&self, text: &str) -> Result<(), String> {
        self.write_toml(text).await.map_err(|e| format!("{e:#}"))
    }

    async fn write_toml(&self, text: &str) -> Result<()> {
        let inner = &self.0;
        ensure_document(text)?;
        let _guard = inner.disk.lock().await;
        let config = inner.sources.resolve(Some(text))?;
        if let Some(previous) = read_optional(&inner.path).await? {
            let mut backup = inner.path.as_os_str().to_os_string();
            backup.push(".bak");
            replace_file(Path::new(&backup), &previous).await?;
        }
        replace_file(&inner.path, text).await?;
        inner.publish(config);
        Ok(())
    }

    pub(crate) fn watch_file(&self, shutdown: CancellationToken) {
        tokio::spawn(watch_file(Arc::downgrade(&self.0), shutdown));
    }
}

impl Inner {
    fn publish(&self, config: Config) {
        let config = Arc::new(config);
        self.sender.send_if_modified(|old| {
            if **old == *config {
                false
            } else {
                *old = config;
                true
            }
        });
    }
}

async fn watch_file(inner: Weak<Inner>, shutdown: CancellationToken) {
    let mut observed = None;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = timer.tick() => {}
        }
        let Some(inner) = inner.upgrade() else { return };
        let _guard = inner.disk.lock().await;
        let next = read_optional(&inner.path)
            .await
            .map_err(|e| format!("{e:#}"));
        if observed.as_ref() == Some(&next) {
            continue;
        }
        let result = match &next {
            Ok(text) => inner
                .sources
                .resolve(text.as_deref())
                .map_err(|e| format!("{e:#}")),
            Err(error) => Err(error.clone()),
        };
        // The first poll only records the file the store was loaded from.
        let first = observed.is_none();
        observed = Some(next);
        match result {
            Ok(config) => inner.publish(config),
            Err(error) if first => {
                tracing::debug!(path = %inner.path.display(), %error, "configuration changed before watch")
            }
            Err(error) => {
                tracing::warn!(path = %inner.path.display(), %error, "configuration reload rejected")
            }
        }
    }
}

async fn read_optional(path: &Path) -> Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

async fn replace_file(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    let temp = parent.join(format!(".slskr-config-{}.tmp", uuid::Uuid::new_v4()));
    let mut open = tokio::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    open.mode(0o600);
    let mut file = open.open(&temp).await?;
    let result = async {
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result.with_context(|| format!("replace {}", path.display()))
}

/// A document sent to replace the file must not be blank (e.g. an emptied editor).
fn ensure_document(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty(),
        "configuration document may not be empty"
    );
    Ok(())
}

/// Best-effort dotted key (`section.key`) of the line containing `offset`, so errors name the
/// setting as well as the position.
fn key_at(text: &str, offset: usize) -> Option<String> {
    let header = |line: &str| {
        let line = line.trim();
        line.strip_prefix("[[")
            .and_then(|rest| rest.split_once("]]"))
            .or_else(|| line.strip_prefix('[').and_then(|rest| rest.split_once(']')))
            .map(|(name, _)| name.trim().to_owned())
    };
    let start = text.get(..offset)?.rfind('\n').map_or(0, |i| i + 1);
    let line = text[start..].lines().next().unwrap_or_default();
    if let Some(name) = header(line) {
        return Some(name);
    }
    let key = line.split_once('=')?.0.trim();
    if key.is_empty() {
        return None;
    }
    Some(match text[..start].lines().rev().find_map(header) {
        Some(section) => format!("{section}.{key}"),
        None => key.to_owned(),
    })
}

fn merge(into: &mut Table, from: Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(next)) => merge(base, next),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

/// Maps `SLSKR_<SECTION>_<KEY>` variables onto configuration keys. Names are matched
/// against the default configuration, so keys containing `_` (and nested tables such as
/// `uploads.limits`) need no extra separator. Values are typed like the default: booleans
/// accept `true/false/1/0/yes/no`, lists are comma-separated.
fn env_layer(vars: impl IntoIterator<Item = (String, String)>) -> Result<Table> {
    let defaults = Table::try_from(Config::default()).expect("Config serializes to TOML");
    let mut layer = Table::new();
    for (name, raw) in vars {
        let Some(key) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if RESERVED_ENV.contains(&name.as_str()) {
            continue;
        }
        let key = key.to_ascii_lowercase();
        let Some((path, default)) = env_key(&defaults, &key) else {
            if defaults
                .keys()
                .any(|section| key == *section || key.starts_with(&format!("{section}_")))
            {
                bail!(
                    "{name} does not name a configuration key (API keys and user-defined groups can only be set in the configuration file)"
                );
            }
            continue;
        };
        ensure!(
            path != ["web", "api_keys"],
            "{name}: API keys can only be set in the configuration file"
        );
        let value = env_value(&name, default, &raw)?;
        let mut single = Table::new();
        insert(&mut single, &path, value.clone());
        Value::Table(single)
            .try_into::<Config>()
            .map_err(|e| anyhow!("{name}: {e}"))?;
        insert(&mut layer, &path, value);
    }
    Ok(layer)
}

fn env_key<'a>(table: &'a Table, name: &str) -> Option<(Vec<String>, &'a Value)> {
    for (key, value) in table {
        match value {
            Value::Table(child) => {
                if let Some(rest) = name
                    .strip_prefix(key.as_str())
                    .and_then(|rest| rest.strip_prefix('_'))
                    && let Some((mut path, value)) = env_key(child, rest)
                {
                    path.insert(0, key.clone());
                    return Some((path, value));
                }
            }
            _ if name == key => return Some((vec![key.clone()], value)),
            _ => {}
        }
    }
    None
}

fn env_value(name: &str, default: &Value, raw: &str) -> Result<Value> {
    Ok(match default {
        Value::Boolean(_) => Value::Boolean(match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => bail!("{name} must be true or false"),
        }),
        Value::Integer(_) => Value::Integer(
            raw.trim()
                .parse()
                .with_context(|| format!("{name} must be an integer"))?,
        ),
        Value::Array(_) => Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_owned()))
                .collect(),
        ),
        _ => Value::String(raw.to_owned()),
    })
}

fn insert(into: &mut Table, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut table = into;
    for key in parents {
        let child = table
            .entry(key.clone())
            .or_insert_with(|| Value::Table(Table::new()));
        if !child.is_table() {
            *child = Value::Table(Table::new());
        }
        table = child.as_table_mut().expect("created table");
    }
    table.insert(last.clone(), value);
}

/// Normalizes `web.url_base` and `metrics.path` and resolves relative paths.
fn normalize(config: &mut Config, data_dir: &Path) {
    let base = config.web.url_base.trim().trim_end_matches('/');
    config.web.url_base = if base.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", base.trim_start_matches('/'))
    };
    config.metrics.path = format!("/{}", config.metrics.path.trim().trim_start_matches('/'));

    let absolute = |path: &mut String| {
        if !path.is_empty() && Path::new(path.as_str()).is_relative() {
            *path = data_dir.join(path.as_str()).to_string_lossy().into_owned();
        }
    };
    absolute(&mut config.downloads.directory);
    absolute(&mut config.downloads.incomplete_directory);
    absolute(&mut config.soulseek.picture);
    for spec in &mut config.shares.directories {
        *spec = resolve_share(spec, data_dir);
    }
}

/// Resolves the local part of a `[!][[Alias]]path` share specification.
fn resolve_share(spec: &str, data_dir: &Path) -> String {
    let (prefix, body) = match spec.strip_prefix(['!', '-']) {
        Some(body) => (&spec[..1], body),
        None => ("", spec),
    };
    let (alias, local) = match body.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((alias, local)) => (Some(alias), local),
        None => (None, body),
    };
    if local.is_empty() || Path::new(local).is_absolute() {
        return spec.to_owned();
    }
    let local = data_dir.join(local);
    match alias {
        Some(alias) => format!("{prefix}[{alias}]{}", local.display()),
        None => format!("{prefix}{}", local.display()),
    }
}

fn validate(c: &Config) -> Result<()> {
    let s = &c.soulseek;
    server_endpoint(&s.server).context("soulseek.server")?;
    ensure!(
        s.listen_port >= 1024,
        "soulseek.listen_port must be between 1024 and 65535"
    );
    proxy(&s.proxy).context("soulseek.proxy")?;

    let web = &c.web;
    let base = web.url_base.as_str();
    if base != "/" {
        ensure!(
            valid_web_path(base),
            "web.url_base must be an absolute URL path"
        );
        ensure!(
            !reserved_web_path(base),
            "web.url_base may not use the reserved api, hub or health namespace"
        );
    }
    ensure!(
        !web.auth || !web.username.trim().is_empty(),
        "web.username may not be blank while web.auth is enabled"
    );
    let mut names = HashSet::new();
    let mut keys = HashSet::new();
    for (i, key) in web.api_keys.iter().enumerate() {
        let at = format!("web.api_keys[{i}]");
        ensure!(!key.name.trim().is_empty(), "{at}.name may not be blank");
        ensure!(
            names.insert(key.name.as_str()),
            "{at}.name: duplicate API key name {:?}",
            key.name
        );
        ensure!(
            (16..=255).contains(&key.key.chars().count()),
            "{at}.key must be between 16 and 255 characters"
        );
        ensure!(keys.insert(key.key.as_str()), "{at}.key: duplicate API key");
        ensure!(
            !key.allow.is_empty(),
            "{at}.allow must list at least one CIDR"
        );
        parse_ranges(&key.allow.join(",")).map_err(|e| anyhow!("{at}.allow: {e}"))?;
    }

    let d = &c.downloads;
    ensure!(
        !d.directory.is_empty(),
        "downloads.directory may not be blank"
    );
    ensure!(
        !d.incomplete_directory.is_empty(),
        "downloads.incomplete_directory may not be blank"
    );
    ensure!(
        d.retry_delay <= d.retry_max_delay,
        "downloads.retry_delay may not exceed downloads.retry_max_delay"
    );
    crate::transfers::parse_file_mode(&d.file_mode).context("downloads.file_mode")?;
    ensure!(c.uploads.slots >= 1, "uploads.slots must be at least 1");

    crate::shares::validate_directories(&c.shares.directories).context("shares.directories")?;
    for (i, pattern) in c.shares.exclude.iter().enumerate() {
        regex(pattern).with_context(|| format!("shares.exclude[{i}]"))?;
    }

    let groups = &c.groups;
    for (name, group) in [("default", &groups.default), ("leechers", &groups.leechers)] {
        ensure!(
            group.members.is_empty(),
            "groups.{name}.members: membership is only configurable for user-defined groups"
        );
    }
    let mut members = BTreeSet::new();
    for (name, group) in std::iter::once(("default", &groups.default)).chain(
        groups
            .custom
            .iter()
            .map(|(name, group)| (name.as_str(), group)),
    ) {
        ensure!(
            group.min_shared_files.is_none() && group.min_shared_directories.is_none(),
            "groups.{name}: min_shared_files and min_shared_directories only apply to groups.leechers"
        );
    }
    for (name, group) in &groups.custom {
        ensure!(
            !RESERVED_GROUPS.contains(&name.as_str()),
            "groups.{name}: {name:?} is a built-in group name"
        );
        ensure!(
            !name.trim().is_empty(),
            "groups: group names may not be blank"
        );
        for member in &group.members {
            ensure!(
                members.insert(member.to_lowercase()),
                "groups.{name}.members: {member:?} is listed in more than one group"
            );
        }
    }

    for (i, cidr) in c.blacklist.cidrs.iter().enumerate() {
        parse_ranges(cidr).map_err(|e| anyhow!("blacklist.cidrs[{i}]: {e}"))?;
    }

    let search = &c.search;
    ensure!(
        search.incoming_concurrency > 0
            && search.incoming_concurrency as usize <= tokio::sync::Semaphore::MAX_PERMITS,
        "search.incoming_concurrency must be at least 1"
    );
    for (i, pattern) in search.ignore_queries.iter().enumerate() {
        regex(pattern).with_context(|| format!("search.ignore_queries[{i}]"))?;
    }
    ensure!(
        c.retention
            .searches
            .is_none_or(|age| age >= Duration::from_secs(300)),
        "retention.searches must be empty or at least 5m"
    );

    let m = &c.metrics;
    if m.enabled {
        let path = m.path.trim_end_matches('/');
        ensure!(
            !path.is_empty()
                && valid_web_path(path)
                && !reserved_web_path(path)
                && path != base.trim_end_matches('/'),
            "metrics.path must be an absolute path outside the reserved api, hub, health and web.url_base roots"
        );
    }
    ensure!(
        m.username.is_empty() == m.password.is_empty(),
        "metrics.username and metrics.password must be set together"
    );

    client_options(c)?;
    Ok(())
}

/// Case-insensitive regex, as used for share exclusions and ignored searches.
pub(crate) fn regex(pattern: &str) -> Result<regex::Regex> {
    Ok(regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()?)
}

fn valid_web_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains("//")
        && !path
            .split('/')
            .any(|segment| segment.starts_with(':') || matches!(segment, "." | ".."))
        && !path.chars().any(|c| {
            c.is_whitespace() || matches!(c, '?' | '#' | '{' | '}' | '*' | '\\' | '"' | '<' | '>')
        })
        && path.parse::<axum::http::Uri>().is_ok()
}

fn reserved_web_path(path: &str) -> bool {
    matches!(
        path.trim_start_matches('/').split('/').next(),
        Some("api" | "hub" | "health")
    )
}

/// Splits `host:port` (IPv6 hosts in brackets).
pub(crate) fn server_endpoint(server: &str) -> Result<(String, u16)> {
    let (host, port) = server
        .trim()
        .rsplit_once(':')
        .context("expected host:port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    ensure!(!host.is_empty(), "missing host in {server:?}");
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .with_context(|| format!("invalid port in {server:?}"))?;
    Ok((host.to_owned(), port))
}

/// Parses `socks5://[user:pass@]host:port`; empty means no proxy.
fn proxy(url: &str) -> Result<Option<ProxyOptions>> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(None);
    }
    let rest = url
        .strip_prefix("socks5://")
        .context("expected socks5://[user:pass@]host:port")?;
    let (credentials, endpoint) = match rest.rsplit_once('@') {
        Some((credentials, endpoint)) => {
            let (user, pass) = credentials
                .split_once(':')
                .context("proxy credentials must be user:pass")?;
            (Some((user.to_owned(), pass.to_owned())), endpoint)
        }
        None => (None, rest),
    };
    let (host, port) = server_endpoint(endpoint)?;
    Ok(Some(ProxyOptions::new(host, port, credentials)?))
}

fn diagnostic_level(level: LogLevel) -> DiagnosticLevel {
    match level {
        LogLevel::Trace => DiagnosticLevel::Trace,
        LogLevel::Debug => DiagnosticLevel::Debug,
        LogLevel::Info => DiagnosticLevel::Info,
        LogLevel::Warn | LogLevel::Error => DiagnosticLevel::Warning,
    }
}

pub(crate) fn client_options(c: &Config) -> Result<ClientOptions> {
    let s = &c.soulseek;
    let connection = ConnectionOptions {
        read_buffer_size: 16_384,
        write_buffer_size: 16_384,
        write_queue_size: 50,
        connect_timeout: Duration::from_secs(10),
        inactivity_timeout: Some(Duration::from_secs(15)),
        proxy: proxy(&s.proxy).context("soulseek.proxy")?,
    };
    let transfer = ConnectionOptions {
        read_buffer_size: 262_144,
        write_buffer_size: 262_144,
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..connection.clone()
    };
    let result = ClientOptions {
        listen_ip_address: s.listen_address,
        listen_port: Some(s.listen_port),
        enable_distributed_network: s.distributed_network,
        accept_distributed_children: s.distributed_children > 0,
        distributed_child_limit: s.distributed_children,
        maximum_concurrent_uploads: c.uploads.slots as usize,
        maximum_concurrent_downloads: match c.downloads.slots {
            0 => usize::MAX,
            slots => slots as usize,
        },
        maximum_upload_speed: c.uploads.speed_limit,
        maximum_download_speed: c.downloads.speed_limit,
        auto_acknowledge_private_messages: false,
        minimum_diagnostic_level: diagnostic_level(c.logging.level),
        server_connection_options: connection.clone().without_inactivity_timeout(),
        peer_connection_options: connection.clone(),
        transfer_connection_options: transfer,
        incoming_connection_options: ConnectionOptions {
            proxy: None,
            ..connection.clone()
        },
        distributed_connection_options: connection,
        ..ClientOptions::default()
    };
    result.validate()?;
    Ok(result)
}

pub(crate) fn client_patch(old: &ClientOptions, c: &Config) -> Result<ClientOptionsPatch> {
    let new = client_options(c)?;
    macro_rules! changed {
        ($field:ident) => {
            (old.$field != new.$field).then_some(new.$field)
        };
    }
    Ok(ClientOptionsPatch {
        listen_ip_address: changed!(listen_ip_address),
        listen_port: (old.listen_port != new.listen_port)
            .then_some(new.listen_port)
            .flatten(),
        enable_distributed_network: changed!(enable_distributed_network),
        accept_distributed_children: changed!(accept_distributed_children),
        distributed_child_limit: changed!(distributed_child_limit),
        maximum_upload_speed: changed!(maximum_upload_speed),
        maximum_download_speed: changed!(maximum_download_speed),
        server_connection_options: changed!(server_connection_options),
        peer_connection_options: changed!(peer_connection_options),
        transfer_connection_options: changed!(transfer_connection_options),
        incoming_connection_options: changed!(incoming_connection_options),
        distributed_connection_options: changed!(distributed_connection_options),
        ..ClientOptionsPatch::default()
    })
}

pub(crate) fn auth_config(c: &Config, token_key: &[u8]) -> AuthConfig {
    let web = &c.web;
    AuthConfig {
        disabled: !web.auth,
        username: web.username.clone(),
        password: web.password.clone(),
        jwt_key: token_key.to_vec(),
        jwt_ttl_ms: TOKEN_TTL.as_millis() as u64,
        api_keys: web
            .api_keys
            .iter()
            .map(|key| ApiKey {
                name: key.name.clone(),
                key: key.key.clone(),
                role: match key.role {
                    ApiKeyRole::ReadOnly => Role::ReadOnly,
                    ApiKeyRole::ReadWrite => Role::ReadWrite,
                    ApiKeyRole::Admin => Role::Administrator,
                },
                cidr: key.allow.join(","),
            })
            .collect(),
    }
}

/// True when `current` differs from `startup` in a setting that is only read at startup.
/// Everything else (credentials, API keys, shares, groups, blacklist, limits, retention,
/// Soulseek listener and network options, ...) applies on reload.
pub(crate) fn restart_required(startup: &Config, current: &Config) -> bool {
    let (old, new) = (&startup.web, &current.web);
    startup.soulseek.connect != current.soulseek.connect
        || old.listen != new.listen
        || old.url_base != new.url_base
        || old.auth != new.auth
        || old.log_requests != new.log_requests
        // The client's transfer semaphores are sized once.
        || startup.uploads.slots != current.uploads.slots
        || startup.downloads.slots != current.downloads.slots
        || startup.search.incoming_concurrency != current.search.incoming_concurrency
        || startup.logging != current.logging
        || startup.metrics != current.metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(vars: &[(&str, &str)]) -> Result<Sources> {
        Sources::new(
            PathBuf::from("/data"),
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
            Overrides::default(),
        )
    }

    fn resolve(text: &str) -> Result<Config> {
        sources(&[])?.resolve(Some(text))
    }

    fn error(text: &str) -> String {
        format!("{:#}", resolve(text).unwrap_err())
    }

    #[test]
    fn defaults_without_a_file_are_valid() {
        let config = sources(&[]).unwrap().resolve(None).unwrap();
        assert_eq!(config.web.listen.to_string(), "127.0.0.1:5030");
        assert_eq!(config.web.url_base, "/");
        assert_eq!(config.downloads.directory, "/data/downloads");
        assert_eq!(config.downloads.incomplete_directory, "/data/incomplete");
        assert_eq!(config.downloads.retry_delay, Duration::from_secs(5));
        assert_eq!(config.soulseek.server, "server.slsknet.org:2242");
        assert!(config.soulseek.connect);
        assert_eq!(
            config.retention.searches,
            Some(Duration::from_secs(7 * 86_400))
        );
        assert_eq!(config.groups.leechers.min_shared_files, Some(1));
        let client = client_options(&config).unwrap();
        assert_eq!(client.listen_port, Some(50300));
        assert_eq!(client.maximum_concurrent_downloads, usize::MAX);
        assert_eq!(client.maximum_concurrent_uploads, 20);
    }

    #[test]
    fn file_values_and_partial_sections() {
        let config = resolve(
            r#"
            [soulseek]
            username = "peer"
            distributed_children = 0

            [web]
            listen = "[::1]:5031"
            url_base = "client/"

            [downloads]
            retry_delay = "1m30s"
            on_conflict = "overwrite"

            [uploads.limits]
            daily_files = 10

            [groups.friends]
            members = ["Alice"]
            priority = 1
            strategy = "firstinfirstout"
            "#,
        )
        .unwrap();
        assert_eq!(config.soulseek.username, "peer");
        assert_eq!(config.soulseek.listen_port, 50300);
        assert!(!client_options(&config).unwrap().accept_distributed_children);
        assert_eq!(config.web.listen.to_string(), "[::1]:5031");
        assert_eq!(config.web.url_base, "/client");
        assert_eq!(config.downloads.retry_delay, Duration::from_secs(90));
        assert_eq!(config.uploads.limits.daily_files, 10);
        assert_eq!(config.uploads.limits.queued_files, 0);
        let friends = &config.groups.custom["friends"];
        assert_eq!(friends.priority, 1);
        assert_eq!(friends.slots, 20);
        assert_eq!(config.groups.default.priority, 500);
    }

    #[test]
    fn environment_overrides_the_file_and_cli_overrides_both() {
        let file =
            "[soulseek]\nusername = \"file\"\nconnect = true\n[web]\nlisten = \"127.0.0.1:1111\"\n";
        let env = sources(&[
            ("SLSKR_SOULSEEK_USERNAME", "env"),
            ("SLSKR_WEB_LISTEN", "127.0.0.1:2222"),
            ("SLSKR_SOULSEEK_DISTRIBUTED_NETWORK", "0"),
            ("SLSKR_UPLOADS_LIMITS_QUEUED_FILES", "7"),
            ("SLSKR_SHARES_DIRECTORIES", "/a, [B]/b ,"),
            ("SLSKR_GROUPS_LEECHERS_MIN_SHARED_FILES", "3"),
            ("SLSKR_RETENTION_SEARCHES", ""),
            ("SLSKR_CONTENT_PATH", "/ignored"),
            ("SLSKR_DATA_DIR", "/ignored"),
            ("SLSKR_UNRELATED", "x"),
        ])
        .unwrap();
        let config = env.resolve(Some(file)).unwrap();
        assert_eq!(config.soulseek.username, "env");
        assert_eq!(config.web.listen.port(), 2222);
        assert!(!config.soulseek.distributed_network);
        assert_eq!(config.uploads.limits.queued_files, 7);
        assert_eq!(config.shares.directories, ["/a", "[B]/b"]);
        assert_eq!(config.groups.leechers.min_shared_files, Some(3));
        assert_eq!(config.retention.searches, None);
        assert!(config.soulseek.connect);

        let cli = Sources::new(
            PathBuf::from("/data"),
            Vec::new(),
            Overrides { no_connect: true },
        )
        .unwrap();
        assert!(!cli.resolve(Some(file)).unwrap().soulseek.connect);
    }

    #[test]
    fn environment_errors_name_the_variable() {
        for (name, value, expected) in [
            (
                "SLSKR_SOULSEEK_USERNAMEX",
                "x",
                "SLSKR_SOULSEEK_USERNAMEX does not name",
            ),
            (
                "SLSKR_WEB_AUTH",
                "maybe",
                "SLSKR_WEB_AUTH must be true or false",
            ),
            ("SLSKR_WEB_LISTEN", "nowhere", "SLSKR_WEB_LISTEN:"),
            (
                "SLSKR_SOULSEEK_LISTEN_PORT",
                "70000",
                "SLSKR_SOULSEEK_LISTEN_PORT:",
            ),
            ("SLSKR_LOGGING_LEVEL", "loud", "SLSKR_LOGGING_LEVEL:"),
            ("SLSKR_WEB_API_KEYS", "k", "SLSKR_WEB_API_KEYS: API keys"),
            (
                "SLSKR_GROUPS_FRIENDS_MEMBERS",
                "a",
                "SLSKR_GROUPS_FRIENDS_MEMBERS does not",
            ),
        ] {
            let error = sources(&[(name, value)]).unwrap_err().to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn validation_errors_name_the_key() {
        for (text, expected) in [
            ("[web]\nlisten = \"5030\"", "web.listen"),
            ("[web]\nlisten = 5030", "web.listen"),
            ("[web]\nurl_base = \"/api\"", "web.url_base"),
            ("[web]\nusername = \" \"", "web.username"),
            (
                "[[web.api_keys]]\nname = \"a\"\nkey = \"short\"",
                "web.api_keys[0].key",
            ),
            (
                "[[web.api_keys]]\nname = \"a\"\nkey = \"0123456789abcdef\"\nallow = [\"10.0.0.0/33\"]",
                "web.api_keys[0].allow",
            ),
            (
                "[[web.api_keys]]\nname = \"a\"\nkey = \"0123456789abcdef\"\n[[web.api_keys]]\nname = \"a\"\nkey = \"0123456789abcdeg\"",
                "web.api_keys[1].name: duplicate",
            ),
            (
                "[[web.api_keys]]\nname = \"a\"\nkey = \"0123456789abcdef\"\nrole = \"root\"",
                "role",
            ),
            ("[soulseek]\nserver = \"nohost\"", "soulseek.server"),
            ("[soulseek]\nlisten_port = 80", "soulseek.listen_port"),
            ("[soulseek]\nproxy = \"http://x:1\"", "soulseek.proxy"),
            ("[soulseek]\nusernam = \"x\"", "usernam"),
            ("[downloads]\nretry_delay = \"5 parsecs\"", "retry_delay"),
            ("[downloads]\nfile_mode = \"bogus\"", "downloads.file_mode"),
            ("[uploads]\nslots = 0", "uploads.slots"),
            ("[shares]\nexclude = [\"(\"]", "shares.exclude[0]"),
            (
                "[shares]\ndirectories = [\"[a]/x\", \"[a]/y\"]",
                "shares.directories",
            ),
            (
                "[search]\nignore_queries = [\"[\"]",
                "search.ignore_queries[0]",
            ),
            (
                "[search]\nincoming_concurrency = 0",
                "search.incoming_concurrency",
            ),
            ("[retention]\nsearches = \"1m\"", "retention.searches"),
            ("[groups.privileged]\npriority = 1", "groups.privileged"),
            (
                "[groups.default]\nmembers = [\"a\"]",
                "groups.default.members",
            ),
            (
                "[groups.a]\nmembers = [\"x\"]\n[groups.b]\nmembers = [\"X\"]",
                "groups.b.members",
            ),
            ("[groups.a]\nmin_shared_files = 2", "groups.a"),
            ("[blacklist]\ncidrs = [\"nope\"]", "blacklist.cidrs[0]"),
            (
                "[metrics]\nenabled = true\npath = \"/api/metrics\"",
                "metrics.path",
            ),
            ("[metrics]\nusername = \"u\"", "metrics.username"),
            ("[logging]\nlevel = \"loud\"", "level"),
            (
                "[web]\nlisten = \"127.0.0.1:1\"\nlisten = \"127.0.0.1:2\"",
                "duplicate",
            ),
        ] {
            let error = error(text);
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_data_dir() {
        let config = resolve(
            r#"
            soulseek.picture = "me.png"
            downloads.directory = "music/new"
            shares.directories = ["shared", "[Alias]lib", "!shared/private", "/abs"]
            "#,
        )
        .unwrap();
        assert_eq!(config.soulseek.picture, "/data/me.png");
        assert_eq!(config.downloads.directory, "/data/music/new");
        assert_eq!(
            config.shares.directories,
            [
                "/data/shared",
                "[Alias]/data/lib",
                "!/data/shared/private",
                "/abs"
            ]
        );
    }

    #[test]
    fn restart_detection() {
        let startup = Config::default();
        let mut current = startup.clone();
        current.web.password = "changed".into();
        current.shares.directories.push("/music".into());
        current.soulseek.listen_port = 50301;
        current.uploads.speed_limit = 100;
        assert!(!restart_required(&startup, &current));
        current.web.url_base = "/client".into();
        assert!(restart_required(&startup, &current));
        let mut current = startup.clone();
        current.uploads.slots = 5;
        assert!(restart_required(&startup, &current));
    }

    #[test]
    fn example_file_parses_and_validates() {
        let text = include_str!("../../../slskr.example.toml");
        let config = resolve(text).unwrap();
        assert_eq!(config.web.api_keys.len(), 1);
        assert!(config.groups.custom.contains_key("friends"));
    }

    #[tokio::test]
    async fn write_round_trip_keeps_a_backup_and_refuses_blank_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let sources = Sources::new(dir.path().into(), Vec::new(), Overrides::default()).unwrap();
        let store = ConfigStore::load(path.clone(), sources).await.unwrap();
        assert_eq!(store.read_toml().unwrap(), "");
        let mut updates = store.subscribe();

        assert!(store.validate_toml("").is_some());
        assert!(store.apply_toml("  \n\t\n").await.is_err());
        assert!(store.apply_toml("[web]\nlisten = \"x\"\n").await.is_err());
        assert!(!path.exists());

        let first = "# mine\n[soulseek]\nusername = \"first\"\n";
        assert!(store.validate_toml(first).is_none());
        store.apply_toml(first).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert!(updates.has_changed().unwrap());
        assert_eq!(updates.borrow_and_update().soulseek.username, "first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let second = "[soulseek]\nusername = \"second\"\n";
        store.apply_toml(second).await.unwrap();
        assert_eq!(store.read_toml().unwrap(), second);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("slskr.toml.bak")).unwrap(),
            first
        );
        assert_eq!(store.get().soulseek.username, "second");
        assert_eq!(store.startup().soulseek.username, "");
    }
}
