//! Configuration layers, loading, validation, the settings API and file watching.
//!
//! Precedence, lowest first: built-in defaults, the app-managed settings store
//! (`<data_dir>/settings.json`, see [`store`]), the configuration file (`slskr.toml`),
//! `SLSKR_<SECTION>_<KEY>` environment variables, then command-line flags. A key set by the
//! file, the environment or the command line is *pinned*: the settings API refuses to change
//! it. The file is strict (unknown keys and invalid values fail startup); the store is
//! lenient (they are logged and ignored). Relative paths resolve against the data directory.
//! The file is polled for changes; [`restart_required`] decides which changes only take
//! effect after a restart.

pub mod store;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use serde_json::Value as JsonValue;
use slskr_api::{
    config::{ApiKeyRole, Config, LogLevel, ManagedApiKey, RESERVED_GROUPS},
    settings::{ApiKeyInfo, CreateApiKey, CreatedApiKey, Settings},
};
use soulseek::{
    ClientOptions, ClientOptionsPatch, ConnectionOptions, DiagnosticLevel, ProxyOptions,
};
use tokio::{io::AsyncWriteExt, sync::watch};
use tokio_util::sync::CancellationToken;
use toml::{Table, Value};

use crate::auth::{ApiKey, ApiKeySecret, AuthConfig, Role, parse_ranges, parse_sha256_hex};
use store::StoreFile;

/// Configuration file name inside the data directory.
pub const FILE_NAME: &str = "slskr.toml";
/// Prefix of configuration environment variables.
pub const ENV_PREFIX: &str = "SLSKR_";
/// `SLSKR_*` process variables that are not configuration keys.
pub const RESERVED_ENV: [&str; 3] = ["SLSKR_DATA_DIR", "SLSKR_CONFIG", "SLSKR_CONTENT_PATH"];
/// Longest name of an API key created from the settings API.
const API_KEY_NAME_LIMIT: usize = 64;

/// Command-line settings that override the file and environment.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub no_connect: bool,
}

/// The layers above the configuration file, captured once at startup so reloads are
/// reproducible.
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

    /// Builds the effective configuration from `file` (the file's text, if it exists)
    /// without a settings store.
    pub fn resolve(&self, file: Option<&str>) -> Result<Config> {
        self.build(&StoreFile::default(), &parse_file(file)?)
    }

    fn cli_layer(&self) -> Table {
        let mut layer = Table::new();
        if self.overrides.no_connect {
            insert(
                &mut layer,
                &["soulseek".into(), "connect".into()],
                Value::Boolean(false),
            );
        }
        layer
    }

    /// Dotted keys set by `file`, the environment or the command line, each with the
    /// highest-precedence layer that sets it.
    fn pinned(&self, file: &Table) -> BTreeMap<String, &'static str> {
        let mut pinned = BTreeMap::new();
        for (layer, source) in [
            (file, "slskr.toml"),
            (&self.env, "an SLSKR_* environment variable"),
            (&self.cli_layer(), "the command line"),
        ] {
            for key in table_leaves(layer) {
                pinned.insert(key, source);
            }
        }
        pinned
    }

    /// Merges every layer and validates the result. `file` is the parsed `slskr.toml`.
    fn build(&self, store: &StoreFile, file: &Table) -> Result<Config> {
        let mut table = store.table();
        merge(&mut table, file.clone());
        merge(&mut table, self.env.clone());
        merge(&mut table, self.cli_layer());
        let mut config: Config = Value::Table(table)
            .try_into()
            .context("invalid configuration")?;
        // A plaintext password from the file or the environment wins over the UI's.
        if !self.pinned(file).contains_key("web.password") {
            config.web.password_hash = store.web_password_hash.clone();
        }
        config.web.managed_api_keys = store.api_keys.clone();
        normalize(&mut config, &self.data_dir);
        validate(&config)?;
        Ok(config)
    }

    /// Like [`Self::build`], but entries of `store` that make the configuration invalid are
    /// logged and removed from it instead of failing. Errors only when the file and
    /// environment are invalid on their own.
    fn build_lenient(&self, store: &mut StoreFile, file: &Table) -> Result<Config> {
        let error = match self.build(store, file) {
            Ok(config) => return Ok(config),
            Err(error) => error,
        };
        let mut kept = StoreFile {
            version: store.version,
            web_password_hash: store.web_password_hash.clone(),
            ..StoreFile::default()
        };
        let mut config = self.build(&kept, file)?;
        tracing::warn!(error = %format!("{error:#}"), "the settings store does not fit the configuration; ignoring the conflicting settings");
        for (path, value) in store::leaves(&store.settings) {
            store::insert(&mut kept.settings, &path, value.clone());
            match self.build(&kept, file) {
                Ok(next) => config = next,
                Err(error) => {
                    store::remove(&mut kept.settings, &path);
                    tracing::warn!(key = %path.join("."), error = %format!("{error:#}"), "ignoring setting from the settings store");
                }
            }
        }
        for key in &store.api_keys {
            kept.api_keys.push(key.clone());
            match self.build(&kept, file) {
                Ok(next) => config = next,
                Err(error) => {
                    kept.api_keys.pop();
                    tracing::warn!(name = %key.name, error = %format!("{error:#}"), "ignoring API key from the settings store");
                }
            }
        }
        *store = kept;
        Ok(config)
    }
}

/// Refusal of a settings API change.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// 409: the key is set by the file, the environment or the command line.
    #[error("{0}")]
    Pinned(String),
    /// 400: the change makes the configuration invalid; the message names the key.
    #[error("{0}")]
    Invalid(String),
    /// 404.
    #[error("{0}")]
    NotFound(String),
    /// 409: e.g. an API key name already in use.
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct ConfigStore(Arc<Inner>);

struct Inner {
    path: PathBuf,
    settings_path: PathBuf,
    sources: Sources,
    startup: Arc<Config>,
    sender: watch::Sender<Arc<Config>>,
    /// The file and store layers behind the current configuration. Held while either file
    /// is read or written, so changes apply in order.
    layers: tokio::sync::Mutex<Layers>,
}

#[derive(Default)]
struct Layers {
    /// Parsed `slskr.toml` (empty when it does not exist).
    file: Table,
    store: StoreFile,
}

impl ConfigStore {
    /// Loads `path` (starting from defaults when it does not exist) and the settings store
    /// in the data directory.
    pub async fn load(path: PathBuf, sources: Sources) -> Result<Self> {
        let settings_path = sources.data_dir.join(store::FILE_NAME);
        let text = read_optional(&path).await?;
        let file =
            parse_file(text.as_deref()).with_context(|| format!("loading {}", path.display()))?;
        let mut store = StoreFile::load(&settings_path).await;
        let config = sources
            .build_lenient(&mut store, &file)
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(Self::with_layers(
            path,
            settings_path,
            sources,
            config,
            Layers { file, store },
        ))
    }

    /// A store whose current and startup configuration is `config`, without file or
    /// settings store layers.
    pub fn from_config(path: PathBuf, sources: Sources, config: Config) -> Self {
        let settings_path = sources.data_dir.join(store::FILE_NAME);
        Self::with_layers(path, settings_path, sources, config, Layers::default())
    }

    fn with_layers(
        path: PathBuf,
        settings_path: PathBuf,
        sources: Sources,
        config: Config,
        layers: Layers,
    ) -> Self {
        let startup = Arc::new(config);
        Self(Arc::new(Inner {
            path,
            settings_path,
            sources,
            sender: watch::channel(startup.clone()).0,
            startup,
            layers: tokio::sync::Mutex::new(layers),
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

    /// The configuration file (`slskr.toml`).
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// The settings store (`settings.json`).
    pub fn settings_path(&self) -> &Path {
        &self.0.settings_path
    }

    /// The `GET /api/v0/settings` view of the current configuration.
    pub async fn settings(&self) -> Settings {
        let layers = self.0.layers.lock().await;
        self.view(&layers, &self.get())
    }

    fn view(&self, layers: &Layers, config: &Config) -> Settings {
        let web = &config.web;
        let api_keys = web
            .api_keys
            .iter()
            .map(|key| ApiKeyInfo {
                name: key.name.clone(),
                role: key.role,
                allow: key.allow.clone(),
                pinned: true,
            })
            .chain(web.managed_api_keys.iter().map(|key| ApiKeyInfo {
                name: key.name.clone(),
                role: key.role,
                allow: key.allow.clone(),
                pinned: false,
            }))
            .collect();
        Settings {
            values: config.clone(),
            pinned: self.0.sources.pinned(&layers.file).into_keys().collect(),
            secrets_set: config.secrets_set(),
            pending_restart: restart_required(self.startup(), config),
            api_keys,
        }
    }

    /// Deep-merges `patch` (a sparse object shaped like [`Config`]) into the settings
    /// store, validates the merged configuration, saves the store and applies it. `null`
    /// resets a key to its default; `web.password` is stored as an argon2id hash.
    pub async fn patch(&self, patch: JsonValue) -> Result<Settings, SettingsError> {
        let JsonValue::Object(patch) = patch else {
            return Err(SettingsError::Invalid(
                "expected a JSON object of settings".into(),
            ));
        };
        let mut layers = self.0.layers.lock().await;
        let pinned = self.0.sources.pinned(&layers.file);
        let mut next = layers.store.clone();
        let mut password = None;
        for (path, value) in store::leaves(&patch) {
            let key = path.join(".");
            check_pinned(&pinned, &key)?;
            match (key.as_str(), value) {
                ("web.password", JsonValue::String(text)) if !text.trim().is_empty() => {
                    password = Some(text.clone());
                }
                ("web.password", _) => {
                    return Err(SettingsError::Invalid(
                        "web.password must be a non-empty string".into(),
                    ));
                }
                ("web.api_keys", _) => {
                    return Err(SettingsError::Invalid(
                        "web.api_keys: create and delete API keys with /api/v0/settings/api-keys"
                            .into(),
                    ));
                }
                (_, JsonValue::Null) if known_key(&path) => {
                    store::remove(&mut next.settings, &path);
                }
                (_, JsonValue::Null) => {
                    return Err(SettingsError::Invalid(format!("unknown setting {key}")));
                }
                (_, value) => {
                    store::check_leaf(&path, value).map_err(SettingsError::Invalid)?;
                    store::insert(&mut next.settings, &path, value.clone());
                }
            }
        }
        if let Some(password) = password {
            next.web_password_hash =
                tokio::task::spawn_blocking(move || crate::auth::hash_password(&password))
                    .await
                    .map_err(|e| anyhow!("hashing password: {e}"))??;
        }
        self.commit(&mut layers, next).await
    }

    /// Removes `key` (a dotted key or a whole section) from the settings store, returning it
    /// to the default or to what the file sets.
    pub async fn reset(&self, key: &str) -> Result<Settings, SettingsError> {
        let path = store::split_key(key)
            .filter(|path| known_key(path))
            .ok_or_else(|| SettingsError::NotFound(format!("unknown setting {key}")))?;
        let mut layers = self.0.layers.lock().await;
        check_pinned(&self.0.sources.pinned(&layers.file), key)?;
        let mut next = layers.store.clone();
        match key {
            "web.password" if self.get().web.auth => {
                return Err(SettingsError::Invalid(
                    "web.password can not be removed while web.auth is enabled; set a new password instead"
                        .into(),
                ));
            }
            "web.password" => next.web_password_hash.clear(),
            "web.api_keys" => {
                return Err(SettingsError::Invalid(
                    "web.api_keys: delete API keys with /api/v0/settings/api-keys/{name}".into(),
                ));
            }
            _ => {
                store::remove(&mut next.settings, &path);
            }
        }
        self.commit(&mut layers, next).await
    }

    /// Creates an API key and returns it; only its SHA-256 is stored.
    pub async fn create_api_key(
        &self,
        request: CreateApiKey,
    ) -> Result<CreatedApiKey, SettingsError> {
        let name = request.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > API_KEY_NAME_LIMIT {
            return Err(SettingsError::Invalid(format!(
                "name must be between 1 and {API_KEY_NAME_LIMIT} characters"
            )));
        }
        let allow = if request.allow.is_empty() {
            vec!["0.0.0.0/0".to_owned(), "::/0".to_owned()]
        } else {
            request.allow
        };
        parse_ranges(&allow.join(","))
            .map_err(|e| SettingsError::Invalid(format!("allow: {e}")))?;
        let mut layers = self.0.layers.lock().await;
        let current = self.get();
        let web = &current.web;
        if web.api_keys.iter().any(|key| key.name == name)
            || web.managed_api_keys.iter().any(|key| key.name == name)
        {
            return Err(SettingsError::Conflict(format!(
                "an API key named {name:?} already exists"
            )));
        }
        let key =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let mut next = layers.store.clone();
        next.api_keys.push(ManagedApiKey {
            name: name.clone(),
            sha256: crate::auth::sha256_hex(key.as_bytes()),
            role: request.role,
            allow: allow.clone(),
        });
        self.commit(&mut layers, next).await?;
        Ok(CreatedApiKey {
            name,
            role: request.role,
            allow,
            key,
        })
    }

    /// Deletes an API key created from the settings API.
    pub async fn delete_api_key(&self, name: &str) -> Result<Settings, SettingsError> {
        let mut layers = self.0.layers.lock().await;
        if self.get().web.api_keys.iter().any(|key| key.name == name) {
            return Err(SettingsError::Pinned(format!(
                "API key {name:?} is set by slskr.toml and can not be deleted here"
            )));
        }
        let mut next = layers.store.clone();
        next.api_keys.retain(|key| key.name != name);
        if next.api_keys.len() == layers.store.api_keys.len() {
            return Err(SettingsError::NotFound(format!(
                "no API key named {name:?}"
            )));
        }
        self.commit(&mut layers, next).await
    }

    /// Validates `next` on top of the file and environment, saves it and publishes the
    /// result.
    async fn commit(
        &self,
        layers: &mut Layers,
        next: StoreFile,
    ) -> Result<Settings, SettingsError> {
        let config = self
            .0
            .sources
            .build(&next, &layers.file)
            .map_err(|e| SettingsError::Invalid(format!("{e:#}")))?;
        if next != layers.store {
            replace_file(&self.0.settings_path, &next.to_text()).await?;
            layers.store = next;
        }
        let view = self.view(layers, &config);
        self.0.publish(config);
        Ok(view)
    }

    pub(crate) fn watch_file(&self, shutdown: CancellationToken) {
        tokio::spawn(watch_file(Arc::downgrade(&self.0), shutdown));
    }
}

/// Refuses changing `key` when it, a key below it or a key above it is pinned.
fn check_pinned(pinned: &BTreeMap<String, &'static str>, key: &str) -> Result<(), SettingsError> {
    let below = format!("{key}.");
    let hit = pinned.iter().find(|(pinned, _)| {
        pinned.as_str() == key
            || pinned.starts_with(&below)
            || key.starts_with(&format!("{pinned}."))
    });
    match hit {
        Some((pinned, source)) => Err(SettingsError::Pinned(format!(
            "{pinned} is set by {source} and can not be changed here"
        ))),
        None => Ok(()),
    }
}

/// True when `path` names a configuration key or section. Any `groups.<name>` is a group.
fn known_key(path: &[String]) -> bool {
    let defaults =
        Value::Table(Table::try_from(Config::default()).expect("Config serializes to TOML"));
    // `leechers` has every group key, including the leechers-only ones.
    let (mut node, rest) = match path {
        [first, _, rest @ ..] if first == "groups" => (&defaults["groups"]["leechers"], rest),
        _ => (&defaults, path),
    };
    for segment in rest {
        match node.get(segment.as_str()) {
            Some(child) => node = child,
            None => return false,
        }
    }
    true
}

/// Dotted paths of the non-table values of `table`.
fn table_leaves(table: &Table) -> Vec<String> {
    fn walk(table: &Table, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                Value::Table(child) => walk(child, &path, out),
                _ => out.push(path),
            }
        }
    }
    let mut out = Vec::new();
    walk(table, "", &mut out);
    out
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
        let mut layers = inner.layers.lock().await;
        let next = read_optional(&inner.path)
            .await
            .map_err(|e| format!("{e:#}"));
        if observed.as_ref() == Some(&next) {
            continue;
        }
        let result = match &next {
            Ok(text) => parse_file(text.as_deref()).and_then(|file| {
                let mut store = layers.store.clone();
                let config = inner.sources.build_lenient(&mut store, &file)?;
                Ok((file, store, config))
            }),
            Err(error) => Err(anyhow!("{error}")),
        }
        .map_err(|e| format!("{e:#}"));
        // The first poll only records the file the store was loaded from.
        let first = observed.is_none();
        observed = Some(next);
        match result {
            Ok((file, store, config)) => {
                layers.file = file;
                layers.store = store;
                inner.publish(config);
            }
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

/// Writes `content` to a temporary file (mode 0600) next to `path` and renames it over
/// `path`.
async fn replace_file(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    let temp = parent.join(format!(".slskr-settings-{}.tmp", uuid::Uuid::new_v4()));
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

/// Parses the configuration file strictly: unknown keys and invalid values are errors that
/// name the key and position. `None` (no file) is an empty table.
fn parse_file(text: Option<&str>) -> Result<Table> {
    let Some(text) = text else {
        return Ok(Table::new());
    };
    // Deserializing the text directly reports errors with line and column.
    if let Err(error) = toml::from_str::<Config>(text) {
        return Err(
            match error.span().and_then(|span| key_at(text, span.start)) {
                Some(key) => anyhow!("{key}: {error}"),
                None => error.into(),
            },
        );
    }
    Ok(toml::from_str(text)?)
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
            "web.url_base may not use the reserved api or health namespace"
        );
    }
    ensure!(
        !web.auth || !web.username.trim().is_empty(),
        "web.username may not be blank while web.auth is enabled"
    );
    ensure!(
        (Duration::from_secs(300)..=Duration::from_secs(365 * 86_400)).contains(&web.session_ttl),
        "web.session_ttl must be between 5m and 365d"
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
    for key in &web.managed_api_keys {
        let at = format!("API key {:?} (settings store)", key.name);
        ensure!(!key.name.trim().is_empty(), "{at}: name may not be blank");
        ensure!(
            names.insert(key.name.as_str()),
            "{at}: duplicate API key name"
        );
        ensure!(
            parse_sha256_hex(&key.sha256).is_some(),
            "{at}: invalid digest"
        );
        ensure!(
            !key.allow.is_empty(),
            "{at}: allow must list at least one CIDR"
        );
        parse_ranges(&key.allow.join(",")).map_err(|e| anyhow!("{at}: allow: {e}"))?;
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
            "metrics.path must be an absolute path outside the reserved api, health and web.url_base roots"
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
            c.is_whitespace()
                || matches!(
                    c,
                    '?' | '#' | '{' | '}' | '*' | '\\' | '"' | '<' | '>' | ';'
                )
        })
        && path.parse::<axum::http::Uri>().is_ok()
}

fn reserved_web_path(path: &str) -> bool {
    matches!(
        path.trim_start_matches('/').split('/').next(),
        Some("api" | "health")
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

pub(crate) fn auth_config(c: &Config) -> AuthConfig {
    let web = &c.web;
    let role = |role| match role {
        ApiKeyRole::ReadOnly => Role::ReadOnly,
        ApiKeyRole::ReadWrite => Role::ReadWrite,
        ApiKeyRole::Admin => Role::Administrator,
    };
    let file_keys = web.api_keys.iter().map(|key| ApiKey {
        name: key.name.clone(),
        key: ApiKeySecret::Plain(key.key.clone()),
        role: role(key.role),
        cidr: key.allow.join(","),
    });
    let managed_keys = web.managed_api_keys.iter().filter_map(|key| {
        Some(ApiKey {
            name: key.name.clone(),
            key: ApiKeySecret::Sha256(parse_sha256_hex(&key.sha256)?),
            role: role(key.role),
            cidr: key.allow.join(","),
        })
    });
    AuthConfig {
        disabled: !web.auth,
        username: web.username.clone(),
        password: web.password.clone(),
        password_hash: web.password_hash.clone(),
        session_ttl: web.session_ttl,
        api_keys: file_keys.chain(managed_keys).collect(),
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
            ("[web]\nsession_ttl = \"1m\"", "web.session_ttl"),
            ("[web]\nsession_ttl = \"400d\"", "web.session_ttl"),
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
        assert_eq!(config.web.listen.to_string(), "127.0.0.1:5030");
        // The example stays short: infrastructure keys only.
        assert!(text.lines().count() <= 60, "slskr.example.toml grew");
    }

    /// Every configuration key is documented in docs/CONFIGURATION.md, so the reference can
    /// not drift from the code.
    #[test]
    fn configuration_reference_lists_every_key() {
        let doc = include_str!("../../../docs/CONFIGURATION.md");
        let defaults = Table::try_from(Config::default()).unwrap();
        let mut missing = Vec::new();
        for key in table_leaves(&defaults) {
            // Group keys are documented once as groups.<name>.*, and their limits as one row.
            let key = match key.strip_prefix("groups.") {
                Some(rest) => {
                    let (_, rest) = rest.split_once('.').unwrap();
                    match rest.strip_prefix("limits.") {
                        Some(_) => "groups.<name>.limits".to_owned(),
                        None => format!("groups.<name>.{rest}"),
                    }
                }
                None => key,
            };
            if !doc.contains(&format!("`{key}`")) {
                missing.push(key);
            }
        }
        assert!(missing.is_empty(), "undocumented keys: {missing:?}");
        for key in ["groups.<name>.members", "web.api_keys"] {
            assert!(doc.contains(&format!("`{key}")), "{key}");
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        vars: Vec<(String, String)>,
        overrides: Overrides,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                vars: Vec::new(),
                overrides: Overrides::default(),
            }
        }

        fn file(&self, text: &str) -> &Self {
            std::fs::write(self.dir.path().join(FILE_NAME), text).unwrap();
            self
        }

        fn store_text(&self, text: &str) -> &Self {
            std::fs::write(self.dir.path().join(store::FILE_NAME), text).unwrap();
            self
        }

        fn store_on_disk(&self) -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(self.dir.path().join(store::FILE_NAME)).unwrap(),
            )
            .unwrap()
        }

        async fn load(&self) -> Result<ConfigStore> {
            let sources = Sources::new(
                self.dir.path().into(),
                self.vars.clone(),
                self.overrides.clone(),
            )?;
            ConfigStore::load(self.dir.path().join(FILE_NAME), sources).await
        }
    }

    fn invalid(result: Result<Settings, SettingsError>) -> String {
        match result {
            Err(SettingsError::Invalid(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn layering_precedence_and_pinned_keys() {
        let mut fixture = Fixture::new();
        fixture.vars = vec![("SLSKR_SOULSEEK_DESCRIPTION".into(), "env".into())];
        fixture.overrides.no_connect = true;
        fixture
            .store_text(
                &serde_json::json!({"version": 1, "settings": {
                    "uploads": {"slots": 5},
                    "downloads": {"speed_limit": 7},
                    "soulseek": {"description": "store", "connect": true},
                }})
                .to_string(),
            )
            .file("[downloads]\nspeed_limit = 8\n[soulseek]\ndescription = \"file\"\n");
        let store = fixture.load().await.unwrap();
        let config = store.get();
        assert_eq!(config.uploads.slots, 5, "store over default");
        assert_eq!(config.downloads.speed_limit, 8, "file over store");
        assert_eq!(config.soulseek.description, "env", "env over file");
        assert!(!config.soulseek.connect, "command line over store");
        assert_eq!(config.uploads.speed_limit, 0, "default");

        let settings = store.settings().await;
        assert_eq!(
            settings.pinned,
            [
                "downloads.speed_limit",
                "soulseek.connect",
                "soulseek.description"
            ]
        );
        assert!(!settings.pending_restart);

        let pinned = store
            .patch(serde_json::json!({"downloads": {"speed_limit": 1}}))
            .await;
        match pinned {
            Err(SettingsError::Pinned(message)) => {
                assert!(message.contains("downloads.speed_limit"), "{message}");
                assert!(message.contains("slskr.toml"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        match store
            .patch(serde_json::json!({"soulseek": {"description": "x"}}))
            .await
        {
            Err(SettingsError::Pinned(message)) => assert!(message.contains("SLSKR_"), "{message}"),
            other => panic!("{other:?}"),
        }
        // A whole pinned section can not be patched or reset either.
        assert!(matches!(
            store.patch(serde_json::json!({"downloads": null})).await,
            Err(SettingsError::Pinned(_))
        ));
        assert!(matches!(
            store.reset("soulseek.connect").await,
            Err(SettingsError::Pinned(_))
        ));
    }

    #[tokio::test]
    async fn patch_validates_persists_and_resets() {
        let fixture = Fixture::new();
        fixture.file("[web]\nlisten = \"127.0.0.1:5999\"\n");
        let store = fixture.load().await.unwrap();
        let mut updates = store.subscribe();

        for (patch, key) in [
            (
                serde_json::json!({"uploads": {"slots": 0}}),
                "uploads.slots",
            ),
            (
                serde_json::json!({"uploads": {"slots": "many"}}),
                "uploads.slots",
            ),
            (
                serde_json::json!({"uploads": {"slotz": 1}}),
                "uploads.slotz",
            ),
            (serde_json::json!({"bogus": null}), "bogus"),
            (
                serde_json::json!({"downloads": {"retry_delay": "1h"}}),
                "downloads.retry_delay",
            ),
            (serde_json::json!({"web": {"api_keys": []}}), "web.api_keys"),
            (serde_json::json!({"web": {"password": ""}}), "web.password"),
        ] {
            let message = invalid(store.patch(patch.clone()).await);
            assert!(message.contains(key), "{patch}: {message}");
        }
        assert!(store.patch(serde_json::json!([1])).await.is_err());
        assert!(!store.settings_path().exists(), "nothing written on errors");

        let settings = store
            .patch(serde_json::json!({
                "uploads": {"slots": 4, "limits": {"daily_files": 9}},
                "groups": {"friends": {"members": ["alice"], "priority": 1}},
                "downloads": {"directory": "music"},
            }))
            .await
            .unwrap();
        assert_eq!(settings.values.uploads.slots, 4);
        assert!(settings.pending_restart, "uploads.slots needs a restart");
        assert_eq!(settings.pinned, ["web.listen"]);
        assert!(updates.has_changed().unwrap());
        let config = updates.borrow_and_update().clone();
        assert_eq!(config.uploads.limits.daily_files, 9);
        assert_eq!(config.groups.custom["friends"].priority, 1);
        let music = fixture.dir.path().join("music");
        assert_eq!(config.downloads.directory, music.to_string_lossy());
        assert_eq!(
            fixture.store_on_disk(),
            serde_json::json!({"version": 1, "settings": {
                "uploads": {"slots": 4, "limits": {"daily_files": 9}},
                "groups": {"friends": {"members": ["alice"], "priority": 1}},
                "downloads": {"directory": "music"},
            }})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.settings_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Survives a restart.
        let reloaded = fixture.load().await.unwrap();
        assert_eq!(*reloaded.get(), *store.get());

        // null and DELETE reset keys; whole sections and groups can be reset.
        store
            .patch(serde_json::json!({"uploads": {"limits": {"daily_files": null}}}))
            .await
            .unwrap();
        assert_eq!(store.get().uploads.limits.daily_files, 0);
        store.reset("uploads.slots").await.unwrap();
        assert_eq!(store.get().uploads.slots, 20);
        store.reset("groups.friends").await.unwrap();
        assert!(store.get().groups.custom.is_empty());
        assert!(matches!(
            store.reset("uploads.nope").await,
            Err(SettingsError::NotFound(_))
        ));
        assert!(
            store
                .reset("groups.leechers.min_shared_files")
                .await
                .is_ok()
        );
        assert!(invalid(store.reset("web.password").await).contains("web.auth"));
        assert_eq!(
            fixture.store_on_disk()["settings"],
            serde_json::json!({"downloads": {"directory": "music"}})
        );
    }

    #[tokio::test]
    async fn invalid_store_entries_do_not_fail_startup() {
        let fixture = Fixture::new();
        let defaults = fixture.load().await.unwrap().get();
        for garbage in ["{", "[]", "\u{0}", "{\"settings\": 5}"] {
            fixture.store_text(garbage);
            assert_eq!(fixture.load().await.unwrap().get(), defaults, "{garbage:?}");
        }

        // Unknown keys, type errors and values the merged configuration rejects are dropped;
        // the rest is kept.
        fixture
            .store_text(
                &serde_json::json!({"version": 1, "unknown": true, "settings": {
                    "uploads": {"slots": 0, "speed_limit": 50},
                    "downloads": {"retry_delay": "10m", "resume": false},
                    "future": {"key": 1},
                    "web": {"listen": 5},
                }})
                .to_string(),
            )
            .file("[downloads]\nretry_max_delay = \"1m\"\n");
        let store = fixture.load().await.unwrap();
        let config = store.get();
        assert_eq!(config.uploads.slots, 20);
        assert_eq!(config.uploads.speed_limit, 50);
        assert_eq!(config.downloads.retry_delay, Duration::from_secs(5));
        assert!(!config.downloads.resume);
        assert_eq!(config.web.listen.port(), 5030);
        // Later writes keep only the usable entries.
        store
            .patch(serde_json::json!({"uploads": {"speed_limit": 60}}))
            .await
            .unwrap();
        assert_eq!(
            fixture.store_on_disk()["settings"],
            serde_json::json!({"uploads": {"speed_limit": 60}, "downloads": {"resume": false}})
        );

        // The file stays strict.
        fixture.file("[uploads]\nslotz = 1\n");
        assert!(fixture.load().await.is_err());
    }

    #[tokio::test]
    async fn secrets_are_hashed_or_hidden() {
        let fixture = Fixture::new();
        fixture.file("[metrics]\nusername = \"m\"\npassword = \"metrics-secret\"\n");
        let store = fixture.load().await.unwrap();
        assert_eq!(store.settings().await.secrets_set, ["metrics.password"]);

        let settings = store
            .patch(serde_json::json!({
                "web": {"password": "ui-password"},
                "soulseek": {"username": "peer", "password": "slsk-secret"},
            }))
            .await
            .unwrap();
        assert_eq!(
            settings.secrets_set,
            ["soulseek.password", "web.password", "metrics.password"]
        );
        let body = serde_json::to_string(&settings).unwrap();
        for secret in ["ui-password", "slsk-secret", "metrics-secret", "argon2"] {
            assert!(!body.contains(secret), "{secret} in {body}");
        }
        let on_disk = fixture.store_on_disk();
        let hash = on_disk["web_password_hash"].as_str().unwrap().to_owned();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(!on_disk.to_string().contains("ui-password"));
        // Needed to log in to Soulseek, so stored as given (the file is 0600).
        assert_eq!(on_disk["settings"]["soulseek"]["password"], "slsk-secret");

        // An absent secret is unchanged.
        store
            .patch(serde_json::json!({"soulseek": {"description": "hi"}}))
            .await
            .unwrap();
        assert_eq!(store.get().web.password_hash, hash);
        assert_eq!(store.get().soulseek.password, "slsk-secret");
        assert!(matches!(
            store
                .patch(serde_json::json!({"metrics": {"password": "x"}}))
                .await,
            Err(SettingsError::Pinned(_))
        ));

        // A password from the file wins over the UI's hash, and pins the key.
        fixture.file("[web]\npassword = \"file-password\"\n");
        let store = fixture.load().await.unwrap();
        assert_eq!(store.get().web.password, "file-password");
        assert_eq!(store.get().web.password_hash, "");
        assert!(matches!(
            store
                .patch(serde_json::json!({"web": {"password": "again"}}))
                .await,
            Err(SettingsError::Pinned(_))
        ));
    }

    #[tokio::test]
    async fn password_change_signs_out_and_logs_in_with_the_hash() {
        use crate::auth::{Auth, COOKIE_NAME, SessionStore};
        let fixture = Fixture::new();
        fixture.file("[web]\nusername = \"admin\"\n");
        let store = fixture.load().await.unwrap();
        store
            .patch(serde_json::json!({"web": {"password": "first"}}))
            .await
            .unwrap();
        let auth = Auth::new(
            auth_config(&store.get()),
            SessionStore::memory(Duration::from_secs(3_600)),
        );
        let (token, _) = auth.login("admin", "first").await.unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("{COOKIE_NAME}={token}").parse().unwrap(),
        );
        assert!(auth.session_principal(&headers).is_some());

        // Unrelated changes keep the session.
        store
            .patch(serde_json::json!({"uploads": {"speed_limit": 1}}))
            .await
            .unwrap();
        auth.update(&auth_config(&store.get()));
        assert!(auth.session_principal(&headers).is_some());

        store
            .patch(serde_json::json!({"web": {"password": "second"}}))
            .await
            .unwrap();
        auth.update(&auth_config(&store.get()));
        assert!(auth.session_principal(&headers).is_none());
        assert!(auth.login("admin", "first").await.is_err());
        assert!(auth.login("admin", "second").await.is_ok());
    }

    #[tokio::test]
    async fn api_keys_are_created_used_and_deleted() {
        use crate::auth::{Auth, SessionStore};
        let fixture = Fixture::new();
        fixture.file(
            "[[web.api_keys]]\nname = \"file\"\nkey = \"0123456789abcdef0123\"\nrole = \"readonly\"\n",
        );
        let store = fixture.load().await.unwrap();
        let created = store
            .create_api_key(CreateApiKey {
                name: " automation ".into(),
                role: ApiKeyRole::Admin,
                allow: vec!["127.0.0.0/8".into()],
            })
            .await
            .unwrap();
        assert_eq!(created.name, "automation");
        assert!(created.key.len() >= 40);
        let on_disk = fixture.store_on_disk();
        assert!(!on_disk.to_string().contains(&created.key));
        assert_eq!(
            on_disk["api_keys"][0]["sha256"],
            crate::auth::sha256_hex(created.key.as_bytes())
        );
        let settings = store.settings().await;
        assert_eq!(
            settings
                .api_keys
                .iter()
                .map(|k| (k.name.as_str(), k.pinned))
                .collect::<Vec<_>>(),
            [("file", true), ("automation", false)]
        );
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("0123456789abcdef0123")
        );

        let auth = Auth::new(
            auth_config(&store.get()),
            SessionStore::memory(Duration::from_secs(3_600)),
        );
        let local = "127.0.0.1".parse().unwrap();
        let principal = auth.authenticate_api_key(&created.key, local).unwrap();
        assert_eq!(principal.role, Role::Administrator);
        assert!(
            auth.authenticate_api_key(&created.key, "10.0.0.1".parse().unwrap())
                .is_err()
        );
        assert!(
            auth.authenticate_api_key("0123456789abcdef0123", local)
                .is_ok()
        );

        for name in ["automation", "file"] {
            assert!(matches!(
                store
                    .create_api_key(CreateApiKey {
                        name: name.into(),
                        role: ApiKeyRole::ReadOnly,
                        allow: Vec::new(),
                    })
                    .await,
                Err(SettingsError::Conflict(_))
            ));
        }
        assert!(matches!(
            store
                .create_api_key(CreateApiKey {
                    name: "bad".into(),
                    role: ApiKeyRole::ReadOnly,
                    allow: vec!["10.0.0.0/33".into()],
                })
                .await,
            Err(SettingsError::Invalid(_))
        ));
        // Survives a restart.
        assert_eq!(
            fixture.load().await.unwrap().get().web.managed_api_keys,
            store.get().web.managed_api_keys
        );

        assert!(matches!(
            store.delete_api_key("file").await,
            Err(SettingsError::Pinned(_))
        ));
        store.delete_api_key("automation").await.unwrap();
        assert!(matches!(
            store.delete_api_key("automation").await,
            Err(SettingsError::NotFound(_))
        ));
        auth.update(&auth_config(&store.get()));
        assert!(auth.authenticate_api_key(&created.key, local).is_err());
    }

    #[tokio::test]
    async fn file_reload_keeps_the_store_layer() {
        let fixture = Fixture::new();
        let store = fixture.load().await.unwrap();
        store
            .patch(serde_json::json!({"uploads": {"speed_limit": 3}}))
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        store.watch_file(shutdown.clone());
        let mut updates = store.subscribe();
        updates.borrow_and_update();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        fixture.file("[downloads]\nspeed_limit = 2\n");
        tokio::time::timeout(Duration::from_secs(5), updates.changed())
            .await
            .unwrap()
            .unwrap();
        let config = store.get();
        assert_eq!(config.downloads.speed_limit, 2);
        assert_eq!(config.uploads.speed_limit, 3);
        assert_eq!(store.settings().await.pinned, ["downloads.speed_limit"]);
        shutdown.cancel();
    }
}
