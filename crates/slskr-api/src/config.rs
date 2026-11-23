//! The slskr configuration (`slskr.toml`, `GET /api/v0/options`, application hub `OPTIONS`).
//!
//! Keys are snake_case in TOML and JSON alike. Every section is `#[serde(default)]`, so a
//! partial document fills in the defaults below, and `deny_unknown_fields`, so a typo is an
//! error instead of a silently ignored key. Durations are written as human readable strings
//! such as `"5s"`, `"90m"` or `"7d"` (see [`duration`]); optional durations use `""` for
//! "never". These types are shared with the WASM UI and do no I/O.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Replacement for secret values in redacted output.
pub const REDACTED: &str = "*****";

/// Group names with built-in meaning; they cannot be used for `[groups.<name>]`.
pub const RESERVED_GROUPS: [&str; 4] = ["default", "leechers", "privileged", "blacklisted"];

/// Root of `slskr.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub soulseek: SoulseekConfig,
    pub web: WebConfig,
    pub downloads: DownloadsConfig,
    pub uploads: UploadsConfig,
    pub shares: SharesConfig,
    pub groups: GroupsConfig,
    pub blacklist: BlacklistConfig,
    pub search: SearchConfig,
    pub retention: RetentionConfig,
    pub logging: LoggingConfig,
    pub metrics: MetricsConfig,
}

impl Config {
    /// Copy with every secret that is set replaced by [`REDACTED`].
    pub fn redact(&self) -> Config {
        fn hide(value: &mut String) {
            if !value.is_empty() {
                *value = REDACTED.to_owned();
            }
        }
        let mut c = self.clone();
        hide(&mut c.soulseek.password);
        if let Some((scheme, rest)) = c.soulseek.proxy.split_once("://")
            && let Some((_, host)) = rest.rsplit_once('@')
        {
            c.soulseek.proxy = format!("{scheme}://{REDACTED}@{host}");
        }
        hide(&mut c.web.password);
        for key in &mut c.web.api_keys {
            hide(&mut key.key);
        }
        hide(&mut c.metrics.password);
        c
    }
}

/// `[soulseek]`: the Soulseek network client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoulseekConfig {
    /// Empty means not configured; the client does not connect.
    pub username: String,
    pub password: String,
    /// Server as `host:port`.
    pub server: String,
    pub listen_address: IpAddr,
    pub listen_port: u16,
    pub description: String,
    /// Path to an image file; empty for none.
    pub picture: String,
    /// `false` runs without connecting to the server.
    pub connect: bool,
    /// SOCKS5 proxy URL, `socks5://[user:pass@]host:port`; empty for none.
    pub proxy: String,
    pub distributed_network: bool,
    /// Maximum distributed children; 0 accepts none.
    pub distributed_children: u32,
    /// Chat rooms joined after every login.
    pub rooms: Vec<String>,
}

impl Default for SoulseekConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            server: "server.slsknet.org:2242".to_owned(),
            listen_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            listen_port: 50300,
            description: String::new(),
            picture: String::new(),
            connect: true,
            proxy: String::new(),
            distributed_network: true,
            distributed_children: 25,
            rooms: Vec::new(),
        }
    }
}

/// `[web]`: HTTP server and authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub listen: SocketAddr,
    /// Path prefix the UI and API are served under; normalized to `/` or `/prefix`.
    pub url_base: String,
    pub username: String,
    /// Empty disables password login.
    pub password: String,
    /// `false` disables authentication entirely.
    pub auth: bool,
    pub log_requests: bool,
    pub api_keys: Vec<ApiKeyConfig>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5030),
            url_base: "/".to_owned(),
            username: "admin".to_owned(),
            password: String::new(),
            auth: true,
            log_requests: false,
            api_keys: Vec::new(),
        }
    }
}

/// `[[web.api_keys]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiKeyConfig {
    pub name: String,
    /// 16 to 255 characters.
    pub key: String,
    pub role: ApiKeyRole,
    /// CIDR ranges allowed to use the key.
    pub allow: Vec<String>,
}

impl Default for ApiKeyConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            key: String::new(),
            role: ApiKeyRole::ReadOnly,
            allow: vec!["0.0.0.0/0".to_owned(), "::/0".to_owned()],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyRole {
    #[default]
    ReadOnly,
    ReadWrite,
    Admin,
}

/// `[downloads]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadsConfig {
    /// Completed downloads; relative paths resolve against the data directory.
    pub directory: String,
    pub incomplete_directory: String,
    /// Concurrent downloads; 0 is unlimited.
    pub slots: u32,
    /// KiB/s; 0 is unlimited.
    pub speed_limit: u32,
    pub retry_attempts: u32,
    #[serde(with = "duration")]
    pub retry_delay: Duration,
    #[serde(with = "duration")]
    pub retry_max_delay: Duration,
    /// Resume partial files on retry instead of starting over.
    pub resume: bool,
    pub on_conflict: OnConflict,
    /// Subdirectory template below `directory`; empty for none. Tokens: `${SOURCE_DIRECTORY}`,
    /// `${SOURCE_PATH}`, `${SOURCE_USERNAME}`, `${BATCH_ID}`, `${BATCH_EXTERNAL_ID}`,
    /// `${SEARCH_ID}`, `${SEARCH_TEXT}`.
    pub subdirectory: String,
    /// Mode of completed files, octal (`"0644"`) or symbolic (`"rw-r--r--"`); empty keeps the
    /// default.
    pub file_mode: String,
}

impl Default for DownloadsConfig {
    fn default() -> Self {
        Self {
            directory: "downloads".to_owned(),
            incomplete_directory: "incomplete".to_owned(),
            slots: 0,
            speed_limit: 0,
            retry_attempts: 3,
            retry_delay: Duration::from_secs(5),
            retry_max_delay: Duration::from_secs(300),
            resume: true,
            on_conflict: OnConflict::Rename,
            subdirectory: "${SOURCE_DIRECTORY}".to_owned(),
            file_mode: String::new(),
        }
    }
}

/// What to do when a completed download's destination already exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    #[default]
    Rename,
    Overwrite,
}

/// `[uploads]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadsConfig {
    /// Concurrent uploads, at least 1.
    pub slots: u32,
    /// KiB/s; 0 is unlimited.
    pub speed_limit: u32,
    /// Per-user limits applied to every group that does not set its own.
    pub limits: LimitsConfig,
}

impl Default for UploadsConfig {
    fn default() -> Self {
        Self {
            slots: 20,
            speed_limit: 0,
            limits: LimitsConfig::default(),
        }
    }
}

/// Per-user upload limits; 0 is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub queued_files: u32,
    pub queued_megabytes: u32,
    pub daily_files: u32,
    pub daily_megabytes: u32,
    pub weekly_files: u32,
    pub weekly_megabytes: u32,
    pub daily_failures: u32,
    pub weekly_failures: u32,
}

/// `[shares]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SharesConfig {
    /// Shared roots. `[Alias]/path` sets the remote name, `!/path` excludes a subtree.
    pub directories: Vec<String>,
    /// Case-insensitive regexes matched against full local paths.
    pub exclude: Vec<String>,
    /// Rescan when the last scan is older than this; empty never rescans automatically.
    #[serde(with = "duration::optional")]
    pub rescan_interval: Option<Duration>,
}

/// `[groups]`: `default`, `leechers` and any number of user-defined `[groups.<name>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupsConfig {
    pub default: GroupConfig,
    pub leechers: GroupConfig,
    #[serde(flatten)]
    pub custom: BTreeMap<String, GroupConfig>,
}

impl Default for GroupsConfig {
    fn default() -> Self {
        Self {
            default: GroupConfig::default(),
            leechers: GroupConfig {
                priority: 999,
                min_shared_files: Some(1),
                min_shared_directories: Some(1),
                ..GroupConfig::default()
            },
            custom: BTreeMap::new(),
        }
    }
}

/// Upload settings of one user group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GroupConfig {
    /// Lower is served first.
    pub priority: u32,
    pub strategy: QueueStrategy,
    /// Upload slots for the group; 0 leaves only the global limit.
    pub slots: u32,
    /// KiB/s; 0 is unlimited.
    pub speed_limit: u32,
    /// Overrides `[uploads.limits]` where non-zero.
    pub limits: LimitsConfig,
    /// User-defined groups only.
    pub members: Vec<String>,
    /// `leechers` only: users sharing fewer files are leechers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_shared_files: Option<u32>,
    /// `leechers` only: users sharing fewer directories are leechers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_shared_directories: Option<u32>,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            priority: 500,
            strategy: QueueStrategy::RoundRobin,
            slots: 20,
            speed_limit: 0,
            limits: LimitsConfig::default(),
            members: Vec::new(),
            min_shared_files: None,
            min_shared_directories: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueStrategy {
    #[default]
    RoundRobin,
    FirstInFirstOut,
}

impl QueueStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RoundRobin => "roundrobin",
            Self::FirstInFirstOut => "firstinfirstout",
        }
    }
}

/// `[blacklist]`: users refused every service.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlacklistConfig {
    /// Usernames, case-insensitive.
    pub users: Vec<String>,
    /// Username wildcards (`*`, `?`), case-insensitive.
    pub patterns: Vec<String>,
    /// IP ranges. When set, search responses and downloads also look up the peer's address.
    pub cidrs: Vec<String>,
}

/// `[search]`: incoming search requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub incoming_concurrency: u32,
    pub incoming_circuit_breaker: u32,
    pub response_file_limit: u32,
    /// Case-insensitive regexes; matching incoming searches are dropped.
    pub ignore_queries: Vec<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            incoming_concurrency: 10,
            incoming_circuit_breaker: 500,
            response_file_limit: 500,
            ignore_queries: Vec::new(),
        }
    }
}

/// `[retention]`: how long finished records are kept; empty keeps them forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(with = "duration::optional")]
    pub searches: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub uploads_succeeded: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub uploads_errored: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub uploads_cancelled: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub uploads_failed: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub downloads_succeeded: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub downloads_errored: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub downloads_cancelled: Option<Duration>,
    #[serde(with = "duration::optional")]
    pub downloads_failed: Option<Duration>,
    /// Log files under `<data_dir>/logs`.
    #[serde(with = "duration::optional")]
    pub logs: Option<Duration>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            searches: Some(Duration::from_secs(7 * 86_400)),
            uploads_succeeded: None,
            uploads_errored: None,
            uploads_cancelled: None,
            uploads_failed: None,
            downloads_succeeded: None,
            downloads_errored: None,
            downloads_cancelled: None,
            downloads_failed: None,
            logs: Some(Duration::from_secs(30 * 86_400)),
        }
    }
}

/// `[logging]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `RUST_LOG`, when set, takes precedence.
    pub level: LogLevel,
    /// Write daily log files under `<data_dir>/logs`.
    pub file: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            file: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// `[metrics]`: Prometheus exposition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub path: String,
    /// Basic auth credentials; both empty serves metrics without authentication.
    pub username: String,
    pub password: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/metrics".to_owned(),
            username: String::new(),
            password: String::new(),
        }
    }
}

/// Human readable durations: one or more `<integer><unit>` terms, units `ms`, `s`, `m`, `h`,
/// `d` and `w` (e.g. `"90s"`, `"1h30m"`, `"7d"`).
pub mod duration {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    const UNITS: [(&str, u64); 6] = [
        ("w", 7 * 86_400_000),
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1_000),
        ("ms", 1),
    ];

    pub fn parse(text: &str) -> Result<Duration, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("empty duration".to_owned());
        }
        let mut rest = text;
        let mut total: u64 = 0;
        while !rest.is_empty() {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if digits == 0 {
                return Err(format!(
                    "invalid duration {text:?}; expected e.g. \"30s\", \"5m\" or \"7d\""
                ));
            }
            let value: u64 = rest[..digits]
                .parse()
                .map_err(|_| format!("duration {text:?} is too large"))?;
            rest = &rest[digits..];
            let unit = rest
                .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
                .unwrap_or(rest.len());
            let millis = UNITS
                .iter()
                .find(|(name, _)| *name == &rest[..unit])
                .map(|(_, millis)| *millis)
                .ok_or_else(|| {
                    format!("invalid duration unit in {text:?}; use ms, s, m, h, d or w")
                })?;
            rest = rest[unit..].trim_start();
            total = value
                .checked_mul(millis)
                .and_then(|millis| total.checked_add(millis))
                .ok_or_else(|| format!("duration {text:?} is too large"))?;
        }
        Ok(Duration::from_millis(total))
    }

    pub fn format(duration: Duration) -> String {
        let mut millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        if millis == 0 {
            return "0s".to_owned();
        }
        let mut text = String::new();
        for (unit, size) in UNITS {
            if millis >= size {
                text.push_str(&(millis / size).to_string());
                text.push_str(unit);
                millis %= size;
            }
        }
        text
    }

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(*value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }

    /// `Option<Duration>` written as `""` when `None`.
    pub mod optional {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(
            value: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&value.map(super::format).unwrap_or_default())
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            let text = String::deserialize(deserializer)?;
            if text.trim().is_empty() {
                return Ok(None);
            }
            super::parse(&text)
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_trip() {
        for (text, secs) in [
            ("5s", 5),
            ("5m", 300),
            ("7d", 604_800),
            ("1h30m", 5_400),
            ("1h 30m", 5_400),
            ("2w", 1_209_600),
        ] {
            assert_eq!(
                duration::parse(text),
                Ok(Duration::from_secs(secs)),
                "{text}"
            );
        }
        assert_eq!(duration::parse("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(duration::format(Duration::from_secs(5_400)), "1h30m");
        assert_eq!(duration::format(Duration::from_secs(604_800)), "1w");
        assert_eq!(duration::format(Duration::ZERO), "0s");
        for bad in ["", "5", "s", "5x", "-5s", "5.5s", "99999999999999999999d"] {
            assert!(duration::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn json_uses_snake_case_and_string_durations() {
        let v = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(v["web"]["url_base"], "/");
        assert_eq!(v["web"]["listen"], "127.0.0.1:5030");
        assert_eq!(v["soulseek"]["listen_address"], "0.0.0.0");
        assert_eq!(v["downloads"]["retry_delay"], "5s");
        assert_eq!(v["downloads"]["on_conflict"], "rename");
        assert_eq!(v["retention"]["searches"], "1w");
        assert_eq!(v["retention"]["uploads_succeeded"], "");
        assert_eq!(v["groups"]["default"]["strategy"], "roundrobin");
        assert_eq!(v["groups"]["leechers"]["min_shared_files"], 1);
        assert!(v["groups"]["default"].get("min_shared_files").is_none());
        let back: Config = serde_json::from_value(v).unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn partial_and_unknown_keys() {
        let c: Config = serde_json::from_str(
            r#"{"soulseek":{"username":"u"},"groups":{"friends":{"members":["a"]}}}"#,
        )
        .unwrap();
        assert_eq!(c.soulseek.username, "u");
        assert_eq!(c.soulseek.listen_port, 50300);
        assert_eq!(c.groups.custom["friends"].members, ["a"]);
        assert_eq!(c.groups.custom["friends"].priority, 500);
        assert!(serde_json::from_str::<Config>(r#"{"soulseek":{"usernme":"u"}}"#).is_err());
        assert!(serde_json::from_str::<Config>(r#"{"nope":{}}"#).is_err());
        assert!(serde_json::from_str::<Config>(r#"{"groups":{"friends":{"membrs":[]}}}"#).is_err());
    }

    #[test]
    fn redact_secrets() {
        let mut c = Config::default();
        c.soulseek.password = "pw".into();
        c.soulseek.proxy = "socks5://user:pass@proxy:1080".into();
        c.web.api_keys.push(ApiKeyConfig {
            name: "a".into(),
            key: "x".repeat(16),
            ..Default::default()
        });
        let r = c.redact();
        assert_eq!(r.soulseek.password, REDACTED);
        assert_eq!(r.soulseek.proxy, format!("socks5://{REDACTED}@proxy:1080"));
        assert_eq!(r.soulseek.username, "");
        assert_eq!(r.web.password, "");
        assert_eq!(r.web.api_keys[0].key, REDACTED);
        assert_eq!(r.web.api_keys[0].role, ApiKeyRole::ReadOnly);
        assert_eq!(r.metrics.password, "");
    }
}
