//! Configuration sources: local defaults, slskd environment, YAML, CLI, runtime overlay.

use std::{
    ffi::OsString,
    io,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use serde_yaml::{Mapping, Value};
use slskr_api::options::{Options, UNLIMITED};
use soulseek::{
    ClientOptions, ClientOptionsPatch, ConnectionOptions, DiagnosticLevel, ProxyOptions,
};
use tokio::{io::AsyncWriteExt, sync::watch};
use tokio_util::sync::CancellationToken;

use crate::auth::{ApiKey, AuthConfig, parse_ranges};

#[derive(Clone)]
pub struct OptionsStore(Arc<Inner>);

struct Inner {
    path: PathBuf,
    defaults: Mapping,
    cli: Mapping,
    startup: Arc<Options>,
    sender: watch::Sender<Arc<Options>>,
    disk: tokio::sync::Mutex<()>,
    current: Mutex<Current>,
}

struct Current {
    base: Arc<Options>,
    // Paths changed by runtime patches, relative to the typed configuration tree.
    overlay: Vec<(Vec<String>, Value)>,
}

impl OptionsStore {
    pub(crate) async fn load(app_directory: &Path, path: PathBuf, cli: Mapping) -> Result<Self> {
        let mut local = Options::with_app_directory(
            &app_directory.to_string_lossy(),
            std::path::MAIN_SEPARATOR,
        );
        local.web.ip_address = Some("127.0.0.1".to_owned());
        local.web.https.disabled = true;
        if let Some(content) = std::env::var_os("SLSKR_CONTENT_PATH") {
            local.web.content_path = content.to_string_lossy().into_owned();
        }
        let mut defaults = options_mapping(&local);
        merge(&mut defaults, environment()?);
        let yaml = read_optional(&path).await?;
        let startup = Arc::new(resolve(
            &defaults,
            yaml.as_deref(),
            &cli,
            &local.app_directory,
            &path,
        )?);
        let (sender, _) = watch::channel(startup.clone());
        Ok(Self(Arc::new(Inner {
            path,
            defaults,
            cli,
            startup: startup.clone(),
            sender,
            disk: tokio::sync::Mutex::new(()),
            current: Mutex::new(Current {
                base: startup,
                overlay: Vec::new(),
            }),
        })))
    }

    pub fn get(&self) -> Arc<Options> {
        self.0.sender.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Options>> {
        self.0.sender.subscribe()
    }

    pub(crate) fn startup(&self) -> &Options {
        &self.0.startup
    }

    pub fn debug_view(&self) -> String {
        serde_yaml::to_string(&self.get().redact()).expect("Options serialization")
    }

    pub fn read_yaml(&self) -> io::Result<String> {
        std::fs::read_to_string(&self.0.path)
    }

    pub fn validate_yaml(yaml: &str) -> Option<String> {
        (|| -> Result<()> {
            ensure_document(yaml)?;
            let mut values = options_mapping(&Options::default());
            merge(&mut values, parse_document(yaml)?);
            validate(&typed(values)?)
        })()
        .err()
        .map(|e| e.to_string())
    }

    /// Validate and save the document, backing up an existing file before replacement.
    /// With `no_config_watch`, the disk configuration takes effect on restart only.
    pub async fn apply_yaml(&self, yaml: &str) -> Result<(), String> {
        self.write_yaml(yaml).await.map_err(|e| format!("{e:#}"))
    }

    async fn write_yaml(&self, yaml: &str) -> Result<()> {
        let inner = &self.0;
        ensure_document(yaml)?;
        let _guard = inner.disk.lock().await;
        let options = inner.resolve(Some(yaml))?;
        if let Some(previous) = read_optional(&inner.path).await? {
            let mut backup = inner.path.as_os_str().to_os_string();
            backup.push(".bak");
            replace_file(Path::new(&backup), &previous).await?;
        }
        replace_file(&inner.path, yaml).await?;
        if !inner.startup.flags.no_config_watch {
            inner.reload(options);
        }
        Ok(())
    }

    pub fn patch(&self, f: impl FnOnce(&mut Options)) {
        let mut current = self
            .0
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut options = (*self.get()).clone();
        f(&mut options);
        current.overlay.clear();
        diff(
            &Value::Mapping(options_mapping(&current.base)),
            Value::Mapping(options_mapping(&options)),
            &mut Vec::new(),
            &mut current.overlay,
        );
        self.0.publish(Arc::new(options));
    }

    pub(crate) fn watch_file(&self, shutdown: CancellationToken) {
        if !self.startup().flags.no_config_watch {
            tokio::spawn(watch_file(Arc::downgrade(&self.0), shutdown));
        }
    }
}

impl Inner {
    fn resolve(&self, yaml: Option<&str>) -> Result<Options> {
        resolve(
            &self.defaults,
            yaml,
            &self.cli,
            &self.startup.app_directory,
            &self.path,
        )
    }

    fn reload(&self, options: Options) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.base = Arc::new(options);
        let effective = if current.overlay.is_empty() {
            current.base.clone()
        } else {
            let mut root = options_mapping(&current.base);
            for (path, value) in &current.overlay {
                let keys = path.iter().map(String::as_str).collect::<Vec<_>>();
                insert(&mut root, &keys, value.clone());
            }
            let mut options = typed(root).expect("typed overlay");
            options
                .app_directory
                .clone_from(&current.base.app_directory);
            options
                .configuration_file
                .clone_from(&current.base.configuration_file);
            Arc::new(options)
        };
        self.publish(effective);
    }

    fn publish(&self, options: Arc<Options>) {
        self.sender.send_if_modified(|old| {
            if *old == options {
                false
            } else {
                *old = options;
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
            Ok(yaml) => inner.resolve(yaml.as_deref()).map_err(|e| format!("{e:#}")),
            Err(error) => Err(error.clone()),
        };
        observed = Some(next);
        match result {
            Ok(options) => inner.reload(options),
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

fn options_mapping(options: &Options) -> Mapping {
    let Value::Mapping(values) = serde_yaml::to_value(options).expect("Options serialization")
    else {
        unreachable!("Options is a struct")
    };
    values
}

fn resolve(
    defaults: &Mapping,
    yaml: Option<&str>,
    cli: &Mapping,
    dir: &str,
    path: &Path,
) -> Result<Options> {
    let mut result = defaults.clone();
    if let Some(yaml) = yaml {
        merge(&mut result, parse_document(yaml)?);
    }
    merge(&mut result, cli.clone());
    let mut options = typed(result)?;
    options.app_directory = dir.to_owned();
    options.configuration_file = path.to_string_lossy().into_owned();
    validate(&options)?;
    Ok(options)
}

fn parse_document(yaml: &str) -> Result<Mapping> {
    let mut value: Value = serde_yaml::from_str(yaml)?;
    value.apply_merge()?;
    match normalize(value, false)? {
        Value::Null => Ok(Mapping::new()),
        Value::Mapping(values) => Ok(values),
        _ => Err(anyhow!("configuration must be a YAML mapping")),
    }
}

/// A document sent to replace slskd.yml must not be blank (e.g. an emptied editor).
fn ensure_document(yaml: &str) -> Result<()> {
    ensure!(
        !yaml.trim().is_empty(),
        "configuration document may not be empty"
    );
    Ok(())
}

/// Binds merged configuration values to `Options` like the .NET configuration binder,
/// which sees every scalar as text, and normalizes `web.url_base` like slskd startup.
fn typed(values: Mapping) -> Result<Options, serde_yaml::Error> {
    use serde::Deserialize;
    let mut options = Options::deserialize(Lenient(Value::Mapping(values)))?;
    if !options.web.url_base.starts_with('/') {
        options.web.url_base.insert(0, '/');
    }
    Ok(options)
}

/// `serde_yaml::Value` deserializer that accepts YAML numbers and booleans for string
/// fields (`password: 12345678`) and numeric or boolean strings for typed scalars.
struct Lenient(Value);

impl<'de> serde::de::IntoDeserializer<'de, serde_yaml::Error> for Lenient {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

macro_rules! lenient_parse {
    ($($method:ident: $ty:ty),* $(,)?) => {$(
        fn $method<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            use serde::de::IntoDeserializer;
            if let Value::String(text) = &self.0
                && let Ok(value) = text.trim().parse::<$ty>()
            {
                return IntoDeserializer::<Self::Error>::into_deserializer(value)
                    .deserialize_any(visitor);
            }
            self.0.$method(visitor)
        }
    )*};
}

macro_rules! lenient_delegate {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: serde::de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.0.$method(visitor)
        }
    )*};
}

impl<'de> serde::Deserializer<'de> for Lenient {
    type Error = serde_yaml::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        use serde::de::value::{MapDeserializer, SeqDeserializer};
        match self.0 {
            Value::Sequence(items) => {
                let mut items = SeqDeserializer::new(items.into_iter().map(Lenient));
                let value = visitor.visit_seq(&mut items)?;
                items.end()?;
                Ok(value)
            }
            Value::Mapping(entries) => {
                let mut entries = MapDeserializer::new(
                    entries.into_iter().map(|(k, v)| (Lenient(k), Lenient(v))),
                );
                let value = visitor.visit_map(&mut entries)?;
                entries.end()?;
                Ok(value)
            }
            other => other.deserialize_any(visitor),
        }
    }

    fn deserialize_str<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Bool(value) => visitor.visit_string(value.to_string()),
            Value::Number(value) => visitor.visit_string(value.to_string()),
            other => other.deserialize_string(visitor),
        }
    }

    fn deserialize_bool<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if let Value::String(text) = &self.0 {
            let text = text.trim();
            if text.eq_ignore_ascii_case("true") {
                return visitor.visit_bool(true);
            }
            if text.eq_ignore_ascii_case("false") {
                return visitor.visit_bool(false);
            }
        }
        self.0.deserialize_bool(visitor)
    }

    lenient_parse! {
        deserialize_i8: i8, deserialize_i16: i16, deserialize_i32: i32, deserialize_i64: i64,
        deserialize_u8: u8, deserialize_u16: u16, deserialize_u32: u32, deserialize_u64: u64,
        deserialize_f32: f32, deserialize_f64: f64,
    }

    lenient_delegate! {
        deserialize_char, deserialize_bytes, deserialize_byte_buf, deserialize_unit,
        deserialize_identifier, deserialize_ignored_any,
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            other => visitor.visit_some(Lenient(other)),
        }
    }

    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_unit_struct(name, visitor)
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V: serde::de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_map<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }

    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_enum(name, variants, visitor)
    }
}

fn normalize(value: Value, named_map: bool) -> Result<Value> {
    Ok(match value {
        Value::Mapping(entries) => {
            let mut output = Mapping::new();
            for (key, value) in entries {
                let Value::String(key) = key else {
                    return Err(anyhow!("configuration keys must be strings"));
                };
                let key = if named_map { key } else { camel_key(&key) };
                let names = !named_map
                    && matches!(
                        key.as_str(),
                        "apiKeys" | "agents" | "userDefined" | "webhooks" | "scripts"
                    );
                ensure!(
                    output
                        .insert(Value::String(key.clone()), normalize(value, names)?)
                        .is_none(),
                    "duplicate configuration key: {key}"
                );
            }
            Value::Mapping(output)
        }
        Value::Sequence(values) => Value::Sequence(
            values
                .into_iter()
                .map(|v| normalize(v, false))
                .collect::<Result<_>>()?,
        ),
        _ => value,
    })
}

fn camel_key(key: &str) -> String {
    match key {
        "log_sql" => return "logSQL".to_owned(),
        "case_sensitive_regex" => return "caseSensitiveRegEx".to_owned(),
        _ => {}
    }
    let mut parts = key.split('_');
    let mut result = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let mut chars = part.chars();
        if let Some(c) = chars.next() {
            result.extend(c.to_uppercase());
            result.push_str(chars.as_str());
        }
    }
    result
}

fn merge(into: &mut Mapping, from: Mapping) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(Value::Mapping(base)), Value::Mapping(next)) => merge(base, next),
            // Empty sections and null directory placeholders in slskd.yml use default values.
            (Some(Value::Mapping(_)), Value::Null) => {}
            (Some(Value::String(_)), Value::Null)
                if matches!(key.as_str(), Some("downloads" | "incomplete")) => {}
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

fn insert(into: &mut Mapping, path: &[&str], value: Value) {
    let Some((&first, rest)) = path.split_first() else {
        return;
    };
    let key = Value::String(first.to_owned());
    if rest.is_empty() {
        into.insert(key, value);
    } else {
        let child = into
            .entry(key)
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if !child.is_mapping() {
            *child = Value::Mapping(Mapping::new());
        }
        insert(
            child.as_mapping_mut().expect("created mapping"),
            rest,
            value,
        );
    }
}

fn diff(old: &Value, new: Value, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, Value)>) {
    if *old == new {
        return;
    }
    if let (Some(left), Some(right)) = (old.as_mapping(), new.as_mapping())
        && left.len() == right.len()
        && left.keys().all(|key| right.contains_key(key))
    {
        let Value::Mapping(right) = new else {
            unreachable!()
        };
        for (key, value) in right {
            path.push(key.as_str().expect("Options key is a string").to_owned());
            diff(&left[&key], value, path, out);
            path.pop();
        }
    } else {
        out.push((path.clone(), new));
    }
}

pub(crate) fn cli_value(into: &mut Mapping, path: &str, value: impl serde::Serialize) {
    let keys = path.split('.').collect::<Vec<_>>();
    insert(
        into,
        &keys,
        serde_yaml::to_value(value).expect("CLI option serialization"),
    );
}

pub(crate) fn cli_assignment(into: &mut Mapping, argument: &str) -> Result<()> {
    let (path, value) = argument
        .split_once('=')
        .ok_or_else(|| anyhow!("--set expects key=value"))?;
    let keys = path.split('.').map(camel_key).collect::<Vec<_>>();
    ensure!(
        keys.iter().all(|key| !key.is_empty()),
        "invalid --set option path"
    );
    let keys = keys.iter().map(String::as_str).collect::<Vec<_>>();
    insert(into, &keys, serde_yaml::from_str(value)?);
    Ok(())
}

pub(crate) fn env_path(key: &str) -> Option<OsString> {
    std::env::var_os(format!("SLSKD_{key}")).filter(|v| !v.is_empty())
}

// The environment and CLI use slskd's names, which are not derived from the configuration path.
pub(crate) const SOURCES: &[(&str, &str, Option<char>, &str, char)] = &[
    ("DEBUG", "debug", Some('d'), "debug", 'b'),
    ("HEADLESS", "headless", Some('H'), "headless", 'b'),
    (
        "REMOTE_CONFIGURATION",
        "remote-configuration",
        None,
        "remoteConfiguration",
        'b',
    ),
    (
        "REMOTE_FILE_MANAGEMENT",
        "remote-file-management",
        None,
        "remoteFileManagement",
        'b',
    ),
    (
        "INSTANCE_NAME",
        "instance-name",
        Some('i'),
        "instanceName",
        's',
    ),
    ("ROOMS", "rooms", None, "rooms", 'l'),
    ("NO_LOGO", "no-logo", Some('n'), "flags.noLogo", 'b'),
    ("NO_START", "no-start", Some('x'), "flags.noStart", 'b'),
    (
        "NO_CONFIG_WATCH",
        "no-config-watch",
        None,
        "flags.noConfigWatch",
        'b',
    ),
    ("NO_CONNECT", "no-connect", None, "flags.noConnect", 'b'),
    (
        "NO_SHARE_SCAN",
        "no-share-scan",
        None,
        "flags.noShareScan",
        'b',
    ),
    (
        "FORCE_SHARE_SCAN",
        "force-share-scan",
        None,
        "flags.forceShareScan",
        'b',
    ),
    (
        "FORCE_MIGRATIONS",
        "force-migrations",
        None,
        "flags.forceMigrations",
        'b',
    ),
    (
        "NO_VERSION_CHECK",
        "no-version-check",
        None,
        "flags.noVersionCheck",
        'b',
    ),
    ("LOG_SQL", "log-sql", None, "flags.logSQL", 'b'),
    (
        "LOG_UNOBSERVED_EXCEPTIONS",
        "log-unobserved-exceptions",
        None,
        "flags.logUnobservedExceptions",
        'b',
    ),
    (
        "EXPERIMENTAL",
        "experimental",
        None,
        "flags.experimental",
        'b',
    ),
    ("VOLATILE", "volatile", None, "flags.volatile", 'b'),
    (
        "CASE_SENSITIVE_REGEX",
        "case-sensitive-regex",
        None,
        "flags.caseSensitiveRegEx",
        'b',
    ),
    (
        "LEGACY_WINDOWS_TCP_KEEPALIVE",
        "legacy-windows-tcp-keepalive",
        None,
        "flags.legacyWindowsTcpKeepalive",
        'b',
    ),
    (
        "OPTIMISTIC_RELAY_FILE_INFO",
        "optimistic-relay-file-info",
        None,
        "flags.optimisticRelayFileInfo",
        'b',
    ),
    (
        "NO_SQLITE_POOLING",
        "no-sqlite-pooling",
        None,
        "flags.noSqlitePooling",
        'b',
    ),
    ("RELAY", "relay", Some('r'), "relay.enabled", 'b'),
    ("RELAY_MODE", "relay-mode", Some('m'), "relay.mode", 's'),
    (
        "CONTROLLER_ADDRESS",
        "controller-address",
        None,
        "relay.controller.address",
        's',
    ),
    (
        "CONTROLLER_IGNORE_CERTIFICATE_ERRORS",
        "controller-ignore-certificate-errors",
        None,
        "relay.controller.ignoreCertificateErrors",
        'b',
    ),
    (
        "CONTROLLER_API_KEY",
        "controller-api-key",
        None,
        "relay.controller.apiKey",
        's',
    ),
    (
        "CONTROLLER_SECRET",
        "controller-secret",
        None,
        "relay.controller.secret",
        's',
    ),
    (
        "CONTROLLER_DOWNLOADS",
        "controller-downloads",
        None,
        "relay.controller.downloads",
        'b',
    ),
    (
        "INCOMPLETE_DIR",
        "incomplete",
        None,
        "directories.incomplete",
        's',
    ),
    (
        "DOWNLOADS_DIR",
        "downloads",
        Some('o'),
        "directories.downloads",
        's',
    ),
    ("SHARED_DIR", "shared", Some('s'), "shares.directories", 'l'),
    ("SHARE_FILTER", "share-filter", None, "shares.filters", 'l'),
    (
        "SHARE_CACHE_STORAGE_MODE",
        "share-cache-storage-mode",
        None,
        "shares.cache.storageMode",
        's',
    ),
    (
        "SHARE_CACHE_WORKERS",
        "share-cache-workers",
        None,
        "shares.cache.workers",
        'i',
    ),
    (
        "SHARE_CACHE_RETENTION",
        "share-cache-retention",
        None,
        "shares.cache.retention",
        'i',
    ),
    (
        "UPLOAD_SLOTS",
        "upload-slots",
        None,
        "transfers.upload.slots",
        'i',
    ),
    (
        "UPLOAD_SPEED_LIMIT",
        "upload-speed-limit",
        None,
        "transfers.upload.speedLimit",
        'i',
    ),
    (
        "DOWNLOAD_SLOTS",
        "download-slots",
        None,
        "transfers.download.slots",
        'i',
    ),
    (
        "DOWNLOAD_SPEED_LIMIT",
        "download-speed-limit",
        None,
        "transfers.download.speedLimit",
        'i',
    ),
    ("SWAGGER", "swagger", None, "feature.swagger", 'b'),
    (
        "BLACKLIST",
        "enable-blacklist",
        None,
        "blacklist.enabled",
        'b',
    ),
    (
        "BLACKLIST_FILE",
        "blacklist-file",
        None,
        "blacklist.file",
        's',
    ),
    (
        "SEARCH_REQUEST_FILTER",
        "search-request-filter",
        None,
        "filters.search.request",
        'l',
    ),
    ("LOKI", "loki", None, "logger.loki", 's'),
    (
        "NO_DISK_LOGGER",
        "no-disk-logger",
        None,
        "logger.noDisk",
        'b',
    ),
    ("NO_COLOR", "no-color", None, "logger.noColor", 'b'),
    (
        "THROTTLING_SEARCH_INCOMING_CONCURRENCY",
        "throttling-search-incoming-concurrency",
        None,
        "throttling.search.incoming.concurrency",
        'i',
    ),
    (
        "THROTTLING_SEARCH_INCOMING_CIRCUIT_BREAKER",
        "throttling-search-incoming-circuit-breaker",
        None,
        "throttling.search.incoming.circuitBreaker",
        'i',
    ),
    (
        "THROTTLING_SEARCH_INCOMING_RESPONSE_FILE_LIMIT",
        "throttling-search-incoming-response-file-limit",
        None,
        "throttling.search.incoming.responseFileLimit",
        'i',
    ),
    ("METRICS", "metrics", None, "metrics.enabled", 'b'),
    ("METRICS_URL", "metrics-url", None, "metrics.url", 's'),
    (
        "METRICS_NO_AUTH",
        "metrics-no-auth",
        None,
        "metrics.authentication.disabled",
        'b',
    ),
    (
        "METRICS_USERNAME",
        "metrics-username",
        None,
        "metrics.authentication.username",
        's',
    ),
    (
        "METRICS_PASSWORD",
        "metrics-password",
        None,
        "metrics.authentication.password",
        's',
    ),
    (
        "SLSK_ADDRESS",
        "slsk-address",
        None,
        "soulseek.address",
        's',
    ),
    ("SLSK_PORT", "slsk-port", None, "soulseek.port", 'i'),
    (
        "SLSK_USERNAME",
        "slsk-username",
        None,
        "soulseek.username",
        's',
    ),
    (
        "SLSK_PASSWORD",
        "slsk-password",
        None,
        "soulseek.password",
        's',
    ),
    (
        "SLSK_DESCRIPTION",
        "slsk-description",
        None,
        "soulseek.description",
        's',
    ),
    (
        "SLSK_PICTURE",
        "slsk-picture",
        None,
        "soulseek.picture",
        's',
    ),
    (
        "SLSK_LISTEN_IP_ADDRESS",
        "slsk-listen-ip-address",
        None,
        "soulseek.listenIpAddress",
        's',
    ),
    (
        "SLSK_LISTEN_PORT",
        "slsk-listen-port",
        None,
        "soulseek.listenPort",
        'i',
    ),
    (
        "SLSK_DIAG_LEVEL",
        "slsk-diag-level",
        None,
        "soulseek.diagnosticLevel",
        's',
    ),
    (
        "SLSK_SERVER_MTU",
        "slsk-server-mtu",
        None,
        "soulseek.connection.mtu",
        'i',
    ),
    (
        "SLSK_READ_BUFFER",
        "slsk-read-buffer",
        None,
        "soulseek.connection.buffer.read",
        'i',
    ),
    (
        "SLSK_WRITE_BUFFER",
        "slsk-write-buffer",
        None,
        "soulseek.connection.buffer.write",
        'i',
    ),
    (
        "SLSK_TRANSFER_BUFFER",
        "slsk-transfer-buffer",
        None,
        "soulseek.connection.buffer.transfer",
        'i',
    ),
    (
        "SLSK_WRITE_QUEUE",
        "slsk-write-queue",
        None,
        "soulseek.connection.buffer.writeQueue",
        'i',
    ),
    (
        "SLSK_CONNECTION_TIMEOUT",
        "slsk-connection-timeout",
        None,
        "soulseek.connection.timeout.connect",
        'i',
    ),
    (
        "SLSK_INACTIVITY_TIMEOUT",
        "slsk-inactivity-timeout",
        None,
        "soulseek.connection.timeout.inactivity",
        'i',
    ),
    (
        "SLSK_TRANSFER_TIMEOUT",
        "slsk-transfer-timeout",
        None,
        "soulseek.connection.timeout.transfer",
        'i',
    ),
    (
        "SLSK_PROXY_ENABLED",
        "slsk-proxy",
        None,
        "soulseek.connection.proxy.enabled",
        'b',
    ),
    (
        "SLSK_PROXY_ADDRESS",
        "slsk-proxy-address",
        None,
        "soulseek.connection.proxy.address",
        's',
    ),
    (
        "SLSK_PROXY_PORT",
        "slsk-proxy-port",
        None,
        "soulseek.connection.proxy.port",
        'i',
    ),
    (
        "SLSK_PROXY_USERNAME",
        "slsk-proxy-username",
        None,
        "soulseek.connection.proxy.username",
        's',
    ),
    (
        "SLSK_PROXY_PASSWORD",
        "slsk-proxy-password",
        None,
        "soulseek.connection.proxy.password",
        's',
    ),
    (
        "SLSK_NO_DNET",
        "slsk-no-dnet",
        None,
        "soulseek.distributedNetwork.disabled",
        'b',
    ),
    (
        "SLSK_DNET_NO_CHILDREN",
        "slsk-dnet-no-children",
        None,
        "soulseek.distributedNetwork.disableChildren",
        'b',
    ),
    (
        "SLSK_DNET_CHILDREN",
        "slsk-dnet-children",
        None,
        "soulseek.distributedNetwork.childLimit",
        'i',
    ),
    (
        "SLSK_DNET_LOGGING",
        "slsk-dnet-logging",
        None,
        "soulseek.distributedNetwork.logging",
        'b',
    ),
    ("HTTP_PORT", "http-port", Some('l'), "web.port", 'i'),
    (
        "HTTP_IP_ADDRESS",
        "http-ip-address",
        None,
        "web.ipAddress",
        's',
    ),
    ("HTTP_SOCKET", "http-socket", None, "web.socket", 's'),
    ("URL_BASE", "url-base", None, "web.urlBase", 's'),
    ("CONTENT_PATH", "content-path", None, "web.contentPath", 's'),
    ("HTTP_LOGGING", "http-logging", None, "web.logging", 'b'),
    (
        "NO_AUTH",
        "no-auth",
        Some('X'),
        "web.authentication.disabled",
        'b',
    ),
    (
        "USERNAME",
        "username",
        Some('u'),
        "web.authentication.username",
        's',
    ),
    (
        "PASSWORD",
        "password",
        Some('p'),
        "web.authentication.password",
        's',
    ),
    (
        "API_KEY",
        "api-key",
        Some('k'),
        "web.authentication.apiKey",
        's',
    ),
    (
        "JWT_KEY",
        "jwt-key",
        None,
        "web.authentication.jwt.key",
        's',
    ),
    (
        "JWT_TTL",
        "jwt-ttl",
        None,
        "web.authentication.jwt.ttl",
        'i',
    ),
    ("NO_HTTPS", "no-https", None, "web.https.disabled", 'b'),
    ("HTTPS_PORT", "https-port", Some('L'), "web.https.port", 'i'),
    (
        "HTTPS_IP_ADDRESS",
        "https-ip-address",
        None,
        "web.https.ipAddress",
        's',
    ),
    (
        "HTTPS_FORCE",
        "force-https",
        Some('f'),
        "web.https.force",
        'b',
    ),
    (
        "HTTPS_CERT_PFX",
        "https-cert-pfx",
        None,
        "web.https.certificate.pfx",
        's',
    ),
    (
        "HTTPS_CERT_PASSWORD",
        "https-cert-password",
        None,
        "web.https.certificate.password",
        's',
    ),
    ("VPN", "vpn", None, "integrations.vpn.enabled", 'b'),
    (
        "VPN_PORT_FORWARDING",
        "vpn-port-forwarding",
        None,
        "integrations.vpn.portForwarding",
        'b',
    ),
    (
        "VPN_POLLING_INTERVAL",
        "vpn-polling-interval",
        None,
        "integrations.vpn.pollingInterval",
        'i',
    ),
    (
        "VPN_GLUETUN_URL",
        "vpn-gluetun-url",
        None,
        "integrations.vpn.gluetun.url",
        's',
    ),
    (
        "VPN_GLUETUN_TIMEOUT",
        "vpn-gluetun-timeout",
        None,
        "integrations.vpn.gluetun.timeout",
        'i',
    ),
    (
        "VPN_GLUETUN_USERNAME",
        "vpn-gluetun-username",
        None,
        "integrations.vpn.gluetun.username",
        's',
    ),
    (
        "VPN_GLUETUN_PASSWORD",
        "vpn-gluetun-password",
        None,
        "integrations.vpn.gluetun.password",
        's',
    ),
    (
        "VPN_GLUETUN_API_KEY",
        "vpn-gluetun-api-key",
        None,
        "integrations.vpn.gluetun.apiKey",
        's',
    ),
    ("FTP", "ftp", None, "integrations.ftp.enabled", 'b'),
    (
        "FTP_ADDRESS",
        "ftp-address",
        None,
        "integrations.ftp.address",
        's',
    ),
    ("FTP_PORT", "ftp-port", None, "integrations.ftp.port", 'i'),
    (
        "FTP_ENCRYPTION_MODE",
        "ftp-encryption-mode",
        None,
        "integrations.ftp.encryptionMode",
        's',
    ),
    (
        "FTP_IGNORE_CERTIFICATE_ERRORS",
        "ftp-ignore-certificate-errors",
        None,
        "integrations.ftp.ignoreCertificateErrors",
        'b',
    ),
    (
        "FTP_USERNAME",
        "ftp-username",
        None,
        "integrations.ftp.username",
        's',
    ),
    (
        "FTP_PASSWORD",
        "ftp-password",
        None,
        "integrations.ftp.password",
        's',
    ),
    (
        "FTP_REMOTE_PATH",
        "ftp-remote-path",
        None,
        "integrations.ftp.remotePath",
        's',
    ),
    (
        "FTP_OVERWRITE_EXISTING",
        "ftp-overwrite-existing",
        None,
        "integrations.ftp.overwriteExisting",
        'b',
    ),
    (
        "FTP_CONNECTION_TIMEOUT",
        "ftp-connection-timeout",
        None,
        "integrations.ftp.connectionTimeout",
        'i',
    ),
    (
        "FTP_RETRY_ATTEMPTS",
        "ftp-retry-attempts",
        None,
        "integrations.ftp.retryAttempts",
        'i',
    ),
    (
        "PUSHBULLET",
        "pushbullet",
        None,
        "integrations.pushbullet.enabled",
        'b',
    ),
    (
        "PUSHBULLET_ACCESS_TOKEN",
        "pushbullet-token",
        None,
        "integrations.pushbullet.accessToken",
        's',
    ),
    (
        "PUSHBULLET_NOTIFICATION_PREFIX",
        "pushbullet-prefix",
        None,
        "integrations.pushbullet.notificationPrefix",
        's',
    ),
    (
        "PUSHBULLET_NOTIFY_ON_PRIVATE_MESSAGE",
        "pushbullet-notify-on-pm",
        None,
        "integrations.pushbullet.notifyOnPrivateMessage",
        'b',
    ),
    (
        "PUSHBULLET_NOTIFY_ON_ROOM_MENTION",
        "pushbullet-notify-on-room-mention",
        None,
        "integrations.pushbullet.notifyOnRoomMention",
        'b',
    ),
    (
        "PUSHBULLET_RETRY_ATTEMPTS",
        "pushbullet-retry-attempts",
        None,
        "integrations.pushbullet.retryAttempts",
        'i',
    ),
    (
        "PUSHBULLET_COOLDOWN_TIME",
        "pushbullet-cooldown",
        None,
        "integrations.pushbullet.cooldownTime",
        'i',
    ),
];

fn environment() -> Result<Mapping> {
    let mut output = Mapping::new();
    for &(name, _, _, path, kind) in SOURCES {
        let name = format!("SLSKD_{name}");
        let value = match std::env::var(&name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => continue,
            Err(e) => return Err(e).context(name),
        };
        match kind {
            's' => cli_value(&mut output, path, value),
            'l' => cli_value(
                &mut output,
                path,
                value
                    .split(';')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>(),
            ),
            'b' => cli_value(
                &mut output,
                path,
                match value.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return Err(anyhow!("{name} must be a boolean")),
                },
            ),
            'i' => cli_value(
                &mut output,
                path,
                value
                    .trim()
                    .parse::<i32>()
                    .with_context(|| format!("{name} must be an integer"))?,
            ),
            _ => unreachable!("environment value kind"),
        }
    }
    Ok(output)
}

fn auth_keys(options: &Options) -> Result<Vec<ApiKey>> {
    let auth = &options.web.authentication;
    let mut keys = Vec::with_capacity(auth.api_keys.len() + usize::from(auth.api_key.is_some()));
    if let Some(value) = &auth.api_key {
        match ApiKey::parse_primary(value) {
            Some(Ok(key)) => keys.push(key),
            Some(Err(error)) => return Err(anyhow!("web.authentication.api_key: {error}")),
            None if !value.trim().is_empty() => {
                return Err(anyhow!("missing primary API key value"));
            }
            None => {}
        }
    }
    for (name, option) in &auth.api_keys {
        let key = option
            .key
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("missing web API key {name}"))?;
        keys.push(ApiKey {
            name: name.clone(),
            key: key.clone(),
            cidr: option.cidr.clone(),
            role: option
                .role
                .parse()
                .map_err(|e: String| anyhow!("web API key {name}: {e}"))?,
        });
    }
    for key in &keys {
        parse_ranges(&key.cidr).map_err(|e| anyhow!("web API key {}: {e}", key.name))?;
    }
    Ok(keys)
}

pub(crate) fn auth_config(options: &Options, startup_key: &[u8]) -> Result<AuthConfig> {
    let auth = &options.web.authentication;
    Ok(AuthConfig {
        disabled: auth.disabled,
        username: auth.username.clone(),
        password: auth.password.clone(),
        jwt_key: if auth.jwt.key.is_empty() {
            startup_key.to_vec()
        } else {
            auth.jwt.key.as_bytes().to_vec()
        },
        jwt_ttl_ms: auth.jwt.ttl.try_into()?,
        api_keys: auth_keys(options)?,
    })
}

pub(crate) fn http_ips(value: Option<&str>) -> Result<Vec<IpAddr>> {
    value
        .unwrap_or("127.0.0.1")
        .split(',')
        .map(|ip| {
            ip.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse()
                .context("invalid web.ip_address")
        })
        .collect()
}

fn validate(options: &Options) -> Result<()> {
    for (obsolete, set) in [
        ("permissions", options.permissions.is_some()),
        ("global", options.global.is_some()),
        ("groups", options.groups.is_some()),
        ("integration", options.integration.is_some()),
    ] {
        ensure!(!set, "obsolete configuration section `{obsolete}`");
    }
    let web = &options.web;
    u16::try_from(web.port).context("web.port must be between 0 and 65535")?;
    http_ips(web.ip_address.as_deref())?;
    if !web.https.disabled {
        u16::try_from(web.https.port).context("web.https.port must be between 0 and 65535")?;
        http_ips(web.https.ip_address.as_deref()).context("invalid web.https.ip_address")?;
    }
    ensure!(
        !web.https.force || !web.https.disabled,
        "forcing HTTPS requires an HTTPS listener"
    );
    let base = web.url_base.trim_end_matches('/');
    ensure!(
        web.url_base.starts_with('/') && (base.is_empty() || valid_web_path(base)),
        "web.url_base must be an absolute URL path"
    );
    if !base.is_empty() {
        ensure!(
            !reserved_web_path(base),
            "web.url_base may not use the reserved api, hub or health namespace"
        );
    }
    if options.metrics.enabled {
        let metrics = format!("/{}", options.metrics.url.trim_start_matches('/'));
        ensure!(
            metrics != "/"
                && valid_web_path(&metrics)
                && !reserved_web_path(&metrics)
                && metrics.trim_end_matches('/') != base,
            "metrics.url must be an absolute path outside the reserved api, hub, health and application roots"
        );
    }
    let auth = &web.authentication;
    ensure!(
        auth.disabled || (!auth.username.trim().is_empty() && !auth.password.trim().is_empty()),
        "web authentication username and password may not be blank"
    );
    // slskd: [StringLength(255, MinimumLength = 32)] Key (generated when unset here),
    // [Range(3600000, int.MaxValue)] Ttl, API keys between 16 and 255 characters.
    ensure!(
        auth.jwt.key.is_empty() || (32..=255).contains(&auth.jwt.key.chars().count()),
        "web.authentication.jwt.key must be between 32 and 255 characters"
    );
    ensure!(
        auth.jwt.ttl >= 3_600_000,
        "web.authentication.jwt.ttl must be at least 3600000 milliseconds"
    );
    for key in auth_keys(options)? {
        ensure!(
            (16..=255).contains(&key.key.chars().count()),
            "API key {} must be between 16 and 255 characters",
            key.name
        );
    }
    ensure!(
        !options.soulseek.address.trim().is_empty(),
        "soulseek.address may not be blank"
    );
    ensure!(
        (1..=65535).contains(&options.soulseek.port),
        "soulseek.port must be between 1 and 65535"
    );
    let _ = client_options(options)?;
    for pattern in &options.filters.search.request {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!options.flags.case_sensitive_reg_ex)
            .build()
            .context("invalid filters.search.request regular expression")?;
    }
    let incoming = &options.throttling.search.incoming;
    ensure!(
        incoming.concurrency > 0
            && incoming.concurrency as usize <= tokio::sync::Semaphore::MAX_PERMITS
            && incoming.circuit_breaker >= 0
            && incoming.response_file_limit >= 0,
        "invalid incoming search throttling options"
    );
    ensure!(
        options.retention.search.is_none_or(|minutes| minutes >= 5),
        "retention.search must be null or at least 5 minutes"
    );
    Ok(())
}

fn valid_web_path(path: &str) -> bool {
    !path.contains("//")
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

pub(crate) fn speed_limit(limit: i32) -> u32 {
    if limit == UNLIMITED {
        0
    } else {
        limit.max(0) as u32
    }
}

pub(crate) fn client_options(options: &Options) -> Result<ClientOptions> {
    let s = &options.soulseek;
    let c = &s.connection;
    let d = &s.distributed_network;
    let listen_port = u16::try_from(s.listen_port).context("invalid soulseek.listen_port")?;
    let proxy = if c.proxy.enabled {
        let credentials = match (&c.proxy.username, &c.proxy.password) {
            (None, None) => None,
            (Some(u), Some(p)) => Some((u.clone(), p.clone())),
            _ => {
                return Err(anyhow!(
                    "Soulseek proxy requires both username and password"
                ));
            }
        };
        Some(ProxyOptions::new(
            c.proxy
                .address
                .as_deref()
                .context("missing soulseek.connection.proxy.address")?,
            c.proxy
                .port
                .context("missing soulseek.connection.proxy.port")?
                .try_into()?,
            credentials,
        )?)
    } else {
        None
    };
    ensure!(
        c.timeout.connect > 0 && c.timeout.inactivity > 0 && c.timeout.transfer > 0,
        "Soulseek connection timeouts must be positive"
    );
    ensure!(
        c.buffer.read > 0
            && c.buffer.write > 0
            && c.buffer.transfer > 0
            && c.buffer.write_queue > 0,
        "Soulseek connection buffers and write queue must be positive"
    );
    let connection = ConnectionOptions {
        read_buffer_size: c.buffer.read as usize,
        write_buffer_size: c.buffer.write as usize,
        write_queue_size: c.buffer.write_queue as usize,
        connect_timeout: Duration::from_millis(c.timeout.connect as u64),
        inactivity_timeout: Some(Duration::from_millis(c.timeout.inactivity as u64)),
        proxy,
    };
    let transfer = ConnectionOptions {
        read_buffer_size: c.buffer.transfer as usize,
        write_buffer_size: c.buffer.transfer as usize,
        inactivity_timeout: Some(Duration::from_millis(c.timeout.transfer as u64)),
        ..connection.clone()
    };
    let result = ClientOptions {
        listen_ip_address: s
            .listen_ip_address
            .parse()
            .context("invalid soulseek.listen_ip_address")?,
        listen_port: Some(listen_port),
        enable_distributed_network: !d.disabled,
        accept_distributed_children: !d.disable_children,
        distributed_child_limit: d
            .child_limit
            .try_into()
            .context("invalid distributed child limit")?,
        maximum_concurrent_uploads: options
            .transfers
            .upload
            .slots
            .try_into()
            .context("invalid upload slots")?,
        maximum_concurrent_downloads: options
            .transfers
            .download
            .slots
            .try_into()
            .context("invalid download slots")?,
        maximum_upload_speed: speed_limit(options.transfers.upload.speed_limit),
        maximum_download_speed: speed_limit(options.transfers.download.speed_limit),
        auto_acknowledge_private_messages: false,
        minimum_diagnostic_level: DiagnosticLevel::parse(&s.diagnostic_level)
            .context("invalid soulseek.diagnostic_level")?,
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

pub(crate) fn client_patch(old: &ClientOptions, options: &Options) -> Result<ClientOptionsPatch> {
    let new = client_options(options)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_yaml(yaml: &str, cli: &Mapping) -> Result<Options> {
        let defaults = options_mapping(&Options::default());
        resolve(
            &defaults,
            Some(yaml),
            cli,
            "/app",
            Path::new("/app/slskd.yml"),
        )
    }

    #[test]
    fn scalars_bind_to_string_fields_like_the_dotnet_binder() {
        let options = resolve_yaml(
            "soulseek:\n  username: 1234\n  password: 12345678\nweb:\n  port: \"5031\"\n  authentication:\n    password: true\n",
            &Mapping::new(),
        )
        .unwrap();
        assert_eq!(options.soulseek.username.as_deref(), Some("1234"));
        assert_eq!(options.soulseek.password.as_deref(), Some("12345678"));
        assert_eq!(options.web.port, 5031);
        assert_eq!(options.web.authentication.password, "true");

        let mut cli = Mapping::new();
        cli_assignment(&mut cli, "soulseek.password=87654321").unwrap();
        cli_assignment(&mut cli, "web.url_base=client").unwrap();
        let options = resolve_yaml("", &cli).unwrap();
        assert_eq!(options.soulseek.password.as_deref(), Some("87654321"));
        assert_eq!(options.web.url_base, "/client");
    }

    #[test]
    fn authentication_limits_match_slskd() {
        let cli = Mapping::new();
        let auth = |body: &str| resolve_yaml(&format!("web:\n  authentication:\n{body}"), &cli);
        assert!(auth("    jwt:\n      key: short\n").is_err());
        assert!(auth(&format!("    jwt:\n      key: {}\n", "k".repeat(32))).is_ok());
        assert!(auth(&format!("    jwt:\n      key: {}\n", "k".repeat(256))).is_err());
        assert!(auth("    jwt:\n      ttl: 3599999\n").is_err());
        assert!(auth("    jwt:\n      ttl: 3600000\n").is_ok());
        assert!(auth("    api_key: too-short\n").is_err());
        assert!(auth("    api_key: role=readonly;cidr=10.0.0.0/8;0123456789abcdef\n").is_ok());
        assert!(auth("    api_keys:\n      bot:\n        key: short\n").is_err());
        assert!(auth("    api_keys:\n      bot:\n        key: 0123456789abcdef\n").is_ok());
    }

    #[test]
    fn blank_documents_are_refused() {
        assert!(OptionsStore::validate_yaml("").is_some());
        assert!(OptionsStore::validate_yaml("  \n\t\n").is_some());
        assert!(OptionsStore::validate_yaml("web:\n  port: 5030\n").is_none());
    }
}
