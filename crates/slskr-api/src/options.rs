//! The slskd options tree (`GET /api/v0/options`, application hub `OPTIONS`).
//!
//! JSON keys are camelCase exactly as slskd emits them. YAML config uses snake_case keys;
//! the daemon converts those to camelCase before deserializing into these types.
//! Every struct is `#[serde(default)]`, so partial documents fill in slskd defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Replacement for `[Secret]` values in redacted output.
pub const REDACTED: &str = "*****";

/// Application name, used for default credentials.
pub const APP_NAME: &str = "slskd";

/// Value slskd uses for "unlimited" (`int.MaxValue`).
pub const UNLIMITED: i32 = i32::MAX;

fn redact_opt(value: &mut Option<String>) {
    if value.is_some() {
        *value = Some(REDACTED.to_string());
    }
}

fn redact_str(value: &mut String) {
    *value = REDACTED.to_string();
}

/// Root options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Options {
    #[serde(skip)]
    pub show_version: bool,
    #[serde(skip)]
    pub show_help: bool,
    #[serde(skip)]
    pub show_environment_variables: bool,
    #[serde(skip)]
    pub generate_certificate: bool,
    #[serde(skip)]
    pub generate_secret: bool,
    pub debug: bool,
    pub headless: bool,
    pub remote_configuration: bool,
    pub remote_file_management: bool,
    pub instance_name: String,
    pub flags: FlagsOptions,
    #[serde(skip)]
    pub app_directory: String,
    #[serde(skip)]
    pub configuration_file: String,
    pub relay: RelayOptions,
    /// Obsolete; non-null is a validation error in slskd.
    pub permissions: Option<serde_json::Value>,
    pub directories: DirectoriesOptions,
    pub shares: SharesOptions,
    /// Obsolete (moved under `transfers`).
    pub global: Option<serde_json::Value>,
    /// Obsolete (moved under `transfers`).
    pub groups: Option<serde_json::Value>,
    pub transfers: TransfersOptions,
    pub blacklist: BlacklistOptions,
    pub filters: FiltersOptions,
    pub rooms: Vec<String>,
    pub web: WebOptions,
    pub retention: RetentionOptions,
    pub throttling: ThrottlingOptions,
    pub logger: LoggerOptions,
    pub metrics: MetricsOptions,
    pub feature: FeatureOptions,
    pub soulseek: SoulseekOptions,
    /// Obsolete (renamed `integrations`).
    pub integration: Option<serde_json::Value>,
    pub integrations: IntegrationsOptions,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            show_version: false,
            show_help: false,
            show_environment_variables: false,
            generate_certificate: false,
            generate_secret: false,
            debug: false,
            headless: false,
            remote_configuration: false,
            remote_file_management: false,
            instance_name: "default".to_string(),
            flags: FlagsOptions::default(),
            app_directory: String::new(),
            configuration_file: String::new(),
            relay: RelayOptions::default(),
            permissions: None,
            directories: DirectoriesOptions::default(),
            shares: SharesOptions::default(),
            global: None,
            groups: None,
            transfers: TransfersOptions::default(),
            blacklist: BlacklistOptions::default(),
            filters: FiltersOptions::default(),
            rooms: Vec::new(),
            web: WebOptions::default(),
            retention: RetentionOptions::default(),
            throttling: ThrottlingOptions::default(),
            logger: LoggerOptions::default(),
            metrics: MetricsOptions::default(),
            feature: FeatureOptions::default(),
            soulseek: SoulseekOptions::default(),
            integration: None,
            integrations: IntegrationsOptions::default(),
        }
    }
}

impl Options {
    /// Defaults with paths derived from `app_directory` (`slskd.yml`, `downloads`, `incomplete`),
    /// as slskd's `Program` computes them. `sep` is the platform path separator.
    pub fn with_app_directory(app_directory: &str, sep: char) -> Self {
        let base = app_directory.trim_end_matches(sep);
        Self {
            app_directory: app_directory.to_string(),
            configuration_file: format!("{base}{sep}{APP_NAME}.yml"),
            directories: DirectoriesOptions {
                incomplete: format!("{base}{sep}incomplete"),
                downloads: format!("{base}{sep}downloads"),
            },
            ..Self::default()
        }
    }

    /// Copy with every `[Secret]` string that is set replaced by `"*****"`.
    pub fn redact(&self) -> Options {
        let mut o = self.clone();

        redact_opt(&mut o.relay.controller.api_key);
        redact_opt(&mut o.relay.controller.secret);
        for agent in o.relay.agents.values_mut() {
            redact_opt(&mut agent.instance_name);
            redact_opt(&mut agent.secret);
        }

        redact_opt(&mut o.logger.loki);
        redact_str(&mut o.metrics.authentication.password);

        redact_opt(&mut o.soulseek.password);
        redact_opt(&mut o.soulseek.connection.proxy.password);

        let auth = &mut o.web.authentication;
        redact_str(&mut auth.password);
        redact_opt(&mut auth.api_key);
        redact_str(&mut auth.jwt.key);
        for key in auth.api_keys.values_mut() {
            redact_opt(&mut key.key);
        }
        redact_opt(&mut o.web.https.certificate.password);

        let integrations = &mut o.integrations;
        redact_opt(&mut integrations.vpn.gluetun.password);
        redact_opt(&mut integrations.vpn.gluetun.api_key);
        for hook in integrations.webhooks.values_mut() {
            redact_opt(&mut hook.call.url);
            for header in &mut hook.call.headers {
                redact_opt(&mut header.value);
            }
        }
        for script in integrations.scripts.values_mut() {
            redact_opt(&mut script.run.command);
            redact_opt(&mut script.run.args);
        }
        redact_opt(&mut integrations.ftp.password);
        redact_opt(&mut integrations.pushbullet.access_token);

        o
    }
}

/// Feature/behaviour flags.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FlagsOptions {
    pub no_logo: bool,
    pub no_start: bool,
    pub no_config_watch: bool,
    pub no_connect: bool,
    pub no_share_scan: bool,
    pub force_share_scan: bool,
    pub force_migrations: bool,
    pub no_version_check: bool,
    #[serde(rename = "logSQL")]
    pub log_sql: bool,
    pub log_unobserved_exceptions: bool,
    pub experimental: bool,
    pub volatile: bool,
    pub case_sensitive_reg_ex: bool,
    pub legacy_windows_tcp_keepalive: bool,
    pub optimistic_relay_file_info: bool,
    pub no_sqlite_pooling: bool,
}

/// Relay options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayOptions {
    pub enabled: bool,
    /// `controller`, `agent` or `debug`.
    pub mode: String,
    pub controller: RelayControllerOptions,
    pub agents: BTreeMap<String, RelayAgentOptions>,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "controller".to_string(),
            controller: RelayControllerOptions::default(),
            agents: BTreeMap::new(),
        }
    }
}

/// Controller connection settings (agent mode).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayControllerOptions {
    pub address: Option<String>,
    pub ignore_certificate_errors: bool,
    pub api_key: Option<String>,
    pub secret: Option<String>,
    pub downloads: bool,
}

/// An agent permitted to connect (controller mode).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelayAgentOptions {
    pub instance_name: Option<String>,
    pub secret: Option<String>,
    pub cidr: String,
}

impl Default for RelayAgentOptions {
    fn default() -> Self {
        Self {
            instance_name: None,
            secret: None,
            cidr: "0.0.0.0/0,::/0".to_string(),
        }
    }
}

/// Download directories.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DirectoriesOptions {
    pub incomplete: String,
    pub downloads: String,
}

/// Shares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SharesOptions {
    pub directories: Vec<String>,
    pub filters: Vec<String>,
    pub cache: ShareCacheOptions,
}

/// Share cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ShareCacheOptions {
    /// `memory` or `disk`.
    pub storage_mode: String,
    pub workers: i32,
    pub retention: Option<i32>,
}

impl Default for ShareCacheOptions {
    fn default() -> Self {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            storage_mode: "memory".to_string(),
            workers: i32::try_from(workers).unwrap_or(1),
            retention: None,
        }
    }
}

/// Transfer options.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransfersOptions {
    pub upload: GlobalUploadOptions,
    pub download: GlobalDownloadOptions,
    pub groups: GroupsOptions,
}

/// Global upload limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GlobalUploadOptions {
    pub slots: i32,
    pub speed_limit: i32,
    pub limits: LimitsOptions,
}

impl Default for GlobalUploadOptions {
    fn default() -> Self {
        Self {
            slots: 10,
            speed_limit: UNLIMITED,
            limits: LimitsOptions::default(),
        }
    }
}

/// Global download options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GlobalDownloadOptions {
    pub slots: i32,
    pub speed_limit: i32,
    pub retry: DownloadRetryOptions,
    pub destination: DestinationOptions,
}

impl Default for GlobalDownloadOptions {
    fn default() -> Self {
        Self {
            slots: UNLIMITED,
            speed_limit: UNLIMITED,
            retry: DownloadRetryOptions::default(),
            destination: DestinationOptions::default(),
        }
    }
}

/// Download retry policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DownloadRetryOptions {
    pub attempts: i32,
    pub delay: i32,
    pub max_delay: i32,
    /// `resume` or `restart`.
    pub partial: String,
}

impl Default for DownloadRetryOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay: 5000,
            max_delay: 60_000,
            partial: "resume".to_string(),
        }
    }
}

/// Completed download destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DestinationOptions {
    pub subdirectory: Option<String>,
    /// `rename`, `overwrite`, ...
    pub exists: String,
    pub permissions: DestinationPermissionsOptions,
}

impl Default for DestinationOptions {
    fn default() -> Self {
        Self {
            subdirectory: Some("${SOURCE_DIRECTORY}".to_string()),
            exists: "rename".to_string(),
            permissions: DestinationPermissionsOptions::default(),
        }
    }
}

/// chmod-style permissions for downloaded files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DestinationPermissionsOptions {
    pub mode: Option<String>,
}

/// User groups.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GroupsOptions {
    pub default: BaseGroupOptions,
    pub leechers: LeecherOptions,
    pub blacklisted: BlacklistedOptions,
    pub user_defined: BTreeMap<String, UserDefinedOptions>,
}

/// Group with upload settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BaseGroupOptions {
    pub upload: GroupUploadOptions,
}

/// Per-group upload settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GroupUploadOptions {
    pub priority: i32,
    /// `roundrobin` or `firstinfirstout`.
    pub strategy: String,
    pub slots: i32,
    pub speed_limit: i32,
    pub limits: LimitsOptions,
}

impl Default for GroupUploadOptions {
    fn default() -> Self {
        Self {
            priority: 1,
            strategy: "roundrobin".to_string(),
            slots: UNLIMITED,
            speed_limit: UNLIMITED,
            limits: LimitsOptions::default(),
        }
    }
}

/// Leecher group.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeecherOptions {
    pub upload: GroupUploadOptions,
    pub thresholds: ThresholdOptions,
}

/// Leecher detection thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThresholdOptions {
    pub files: i32,
    pub directories: i32,
}

impl Default for ThresholdOptions {
    fn default() -> Self {
        Self {
            files: 1,
            directories: 1,
        }
    }
}

/// Blacklisted group.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlacklistedOptions {
    pub members: Vec<String>,
    pub patterns: Vec<String>,
    pub cidrs: Vec<String>,
}

/// User-defined group.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserDefinedOptions {
    pub upload: GroupUploadOptions,
    pub members: Vec<String>,
}

/// Queued/daily/weekly limits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LimitsOptions {
    pub queued: Limits,
    pub daily: Limits,
    pub weekly: Limits,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Limits {
    pub files: Option<i32>,
    pub megabytes: Option<i32>,
    pub failures: Option<i32>,
}

/// Feature toggles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FeatureOptions {
    pub swagger: bool,
}

/// IP blacklist file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlacklistOptions {
    pub enabled: bool,
    pub file: Option<String>,
}

/// Filters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FiltersOptions {
    pub search: SearchFilterOptions,
}

/// Incoming search request filters (regexes).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchFilterOptions {
    pub request: Vec<String>,
}

/// Logger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggerOptions {
    pub loki: Option<String>,
    pub no_disk: bool,
    pub no_color: bool,
}

/// Retention (minutes; logs in days).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionOptions {
    pub search: Option<i32>,
    pub transfers: TransferRetentionOptions,
    pub files: FileRetentionOptions,
    pub logs: i32,
}

impl Default for RetentionOptions {
    fn default() -> Self {
        Self {
            search: None,
            transfers: TransferRetentionOptions::default(),
            files: FileRetentionOptions::default(),
            logs: 14,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferRetentionOptions {
    pub upload: TransferTypeRetentionOptions,
    pub download: TransferTypeRetentionOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferTypeRetentionOptions {
    pub succeeded: Option<i32>,
    pub errored: Option<i32>,
    pub cancelled: Option<i32>,
    pub failed: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileRetentionOptions {
    pub complete: Option<i32>,
    pub incomplete: Option<i32>,
}

/// Throttling.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThrottlingOptions {
    pub search: SearchThrottlingOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchThrottlingOptions {
    pub incoming: IncomingSearchThrottlingOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IncomingSearchThrottlingOptions {
    pub concurrency: i32,
    pub circuit_breaker: i32,
    pub response_file_limit: i32,
}

impl Default for IncomingSearchThrottlingOptions {
    fn default() -> Self {
        Self {
            concurrency: 10,
            circuit_breaker: 500,
            response_file_limit: 500,
        }
    }
}

/// Prometheus metrics endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MetricsOptions {
    pub enabled: bool,
    pub url: String,
    pub authentication: MetricsAuthenticationOptions,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "/metrics".to_string(),
            authentication: MetricsAuthenticationOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MetricsAuthenticationOptions {
    pub disabled: bool,
    pub username: String,
    pub password: String,
}

impl Default for MetricsAuthenticationOptions {
    fn default() -> Self {
        Self {
            disabled: false,
            username: APP_NAME.to_string(),
            password: APP_NAME.to_string(),
        }
    }
}

/// Soulseek network client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SoulseekOptions {
    pub address: String,
    pub port: i32,
    pub username: Option<String>,
    pub password: Option<String>,
    pub description: String,
    pub picture: Option<String>,
    pub listen_ip_address: String,
    pub listen_port: i32,
    /// `none`, `warning`, `info` or `debug`.
    pub diagnostic_level: String,
    pub distributed_network: DistributedNetworkOptions,
    pub connection: ConnectionOptions,
}

impl Default for SoulseekOptions {
    fn default() -> Self {
        Self {
            address: "vps.slsknet.org".to_string(),
            port: 2271,
            username: None,
            password: None,
            description: "A slskd user. https://github.com/slskd/slskd".to_string(),
            picture: None,
            listen_ip_address: "0.0.0.0".to_string(),
            listen_port: 50300,
            diagnostic_level: "info".to_string(),
            distributed_network: DistributedNetworkOptions::default(),
            connection: ConnectionOptions::default(),
        }
    }
}

/// Connection tuning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionOptions {
    pub timeout: TimeoutOptions,
    pub buffer: BufferOptions,
    pub proxy: ProxyOptions,
    pub mtu: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BufferOptions {
    pub read: i32,
    pub write: i32,
    pub transfer: i32,
    pub write_queue: i32,
}

impl Default for BufferOptions {
    fn default() -> Self {
        Self {
            read: 16384,
            write: 16384,
            transfer: 262_144,
            write_queue: 50,
        }
    }
}

/// Timeouts in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TimeoutOptions {
    pub connect: i32,
    pub inactivity: i32,
    pub transfer: i32,
}

impl Default for TimeoutOptions {
    fn default() -> Self {
        Self {
            connect: 10_000,
            inactivity: 15_000,
            transfer: 60_000,
        }
    }
}

/// SOCKS5 proxy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyOptions {
    pub enabled: bool,
    pub address: Option<String>,
    pub port: Option<i32>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Distributed network participation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DistributedNetworkOptions {
    pub disabled: bool,
    pub disable_children: bool,
    pub child_limit: i32,
    pub logging: bool,
}

impl Default for DistributedNetworkOptions {
    fn default() -> Self {
        Self {
            disabled: false,
            disable_children: false,
            child_limit: 25,
            logging: false,
        }
    }
}

/// Web server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebOptions {
    pub port: i32,
    pub ip_address: Option<String>,
    pub socket: Option<String>,
    pub https: HttpsOptions,
    pub url_base: String,
    pub content_path: String,
    pub logging: bool,
    pub authentication: WebAuthenticationOptions,
}

impl Default for WebOptions {
    fn default() -> Self {
        Self {
            port: 5030,
            ip_address: None,
            socket: None,
            https: HttpsOptions::default(),
            url_base: "/".to_string(),
            content_path: "wwwroot".to_string(),
            logging: false,
            authentication: WebAuthenticationOptions::default(),
        }
    }
}

/// Web authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebAuthenticationOptions {
    pub disabled: bool,
    pub username: String,
    pub password: String,
    pub jwt: JwtOptions,
    pub api_key: Option<String>,
    pub api_keys: BTreeMap<String, ApiKeyOptions>,
}

impl Default for WebAuthenticationOptions {
    fn default() -> Self {
        Self {
            disabled: false,
            username: APP_NAME.to_string(),
            password: APP_NAME.to_string(),
            jwt: JwtOptions::default(),
            api_key: None,
            api_keys: BTreeMap::new(),
        }
    }
}

/// JWT signing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JwtOptions {
    /// slskd defaults this to 32 random bytes (base62); the daemon must generate it when empty.
    pub key: String,
    /// Token lifetime in milliseconds.
    pub ttl: i32,
}

impl Default for JwtOptions {
    fn default() -> Self {
        Self {
            key: String::new(),
            ttl: 604_800_000,
        }
    }
}

/// A named API key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApiKeyOptions {
    pub key: Option<String>,
    /// `ReadOnly`, `ReadWrite` or `Administrator`.
    pub role: String,
    pub cidr: String,
}

impl Default for ApiKeyOptions {
    fn default() -> Self {
        Self {
            key: None,
            role: "ReadOnly".to_string(),
            cidr: "0.0.0.0/0,::/0".to_string(),
        }
    }
}

/// HTTPS listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HttpsOptions {
    pub disabled: bool,
    pub port: i32,
    pub ip_address: Option<String>,
    pub force: bool,
    pub certificate: CertificateOptions,
}

impl Default for HttpsOptions {
    fn default() -> Self {
        Self {
            disabled: false,
            port: 5031,
            ip_address: None,
            force: false,
            certificate: CertificateOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CertificateOptions {
    pub pfx: Option<String>,
    pub password: Option<String>,
}

/// Third-party integrations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IntegrationsOptions {
    pub vpn: VpnOptions,
    pub webhooks: BTreeMap<String, WebhookOptions>,
    pub scripts: BTreeMap<String, ScriptOptions>,
    pub ftp: FtpOptions,
    pub pushbullet: PushbulletOptions,
}

/// VPN integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VpnOptions {
    pub enabled: bool,
    pub port_forwarding: bool,
    pub polling_interval: i32,
    pub gluetun: GluetunVpnOptions,
}

impl Default for VpnOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            port_forwarding: false,
            polling_interval: 2500,
            gluetun: GluetunVpnOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GluetunVpnOptions {
    pub version: i32,
    pub url: Option<String>,
    pub timeout: i32,
    pub username: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
}

impl Default for GluetunVpnOptions {
    fn default() -> Self {
        Self {
            version: 1,
            url: None,
            timeout: 1000,
            username: None,
            password: None,
            api_key: None,
        }
    }
}

/// A webhook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookOptions {
    /// Event type names.
    pub on: Vec<String>,
    pub call: WebhookHttpOptions,
    pub timeout: i32,
    pub retry: WebhookRetryOptions,
}

impl Default for WebhookOptions {
    fn default() -> Self {
        Self {
            on: Vec::new(),
            call: WebhookHttpOptions::default(),
            timeout: 5000,
            retry: WebhookRetryOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookHttpOptions {
    pub url: Option<String>,
    pub headers: Vec<WebhookHttpHeader>,
    pub ignore_certificate_errors: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookHttpHeader {
    pub name: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookRetryOptions {
    pub attempts: i32,
}

impl Default for WebhookRetryOptions {
    fn default() -> Self {
        Self { attempts: 1 }
    }
}

/// A script hook.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScriptOptions {
    /// Event type names.
    pub on: Vec<String>,
    pub run: ScriptRunOptions,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScriptRunOptions {
    pub command: Option<String>,
    pub executable: Option<String>,
    pub args: Option<String>,
    pub arglist: Option<Vec<String>>,
}

/// FTP upload of completed downloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FtpOptions {
    pub enabled: bool,
    pub address: Option<String>,
    pub port: i32,
    /// `none`, `implicit`, `explicit` or `auto`.
    pub encryption_mode: String,
    pub ignore_certificate_errors: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub remote_path: String,
    pub overwrite_existing: bool,
    pub connection_timeout: i32,
    pub retry_attempts: i32,
}

impl Default for FtpOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            address: None,
            port: 21,
            encryption_mode: "auto".to_string(),
            ignore_certificate_errors: false,
            username: None,
            password: None,
            remote_path: "/".to_string(),
            overwrite_existing: true,
            connection_timeout: 5000,
            retry_attempts: 3,
        }
    }
}

/// Pushbullet notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PushbulletOptions {
    pub enabled: bool,
    pub access_token: Option<String>,
    pub notification_prefix: String,
    pub notify_on_private_message: bool,
    pub notify_on_room_mention: bool,
    pub retry_attempts: i32,
    pub cooldown_time: i32,
}

impl Default for PushbulletOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token: None,
            notification_prefix: "From slskd:".to_string(),
            notify_on_private_message: true,
            notify_on_room_mention: true,
            retry_attempts: 3,
            cooldown_time: 900_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names() {
        let v = serde_json::to_value(Options::default()).unwrap();
        assert!(v.get("showVersion").is_none());
        assert!(v.get("appDirectory").is_none());
        assert_eq!(v["instanceName"], "default");
        assert_eq!(v["permissions"], serde_json::Value::Null);
        assert!(v["flags"].get("logSQL").is_some());
        assert!(v["flags"].get("caseSensitiveRegEx").is_some());
        assert_eq!(v["web"]["urlBase"], "/");
        assert_eq!(v["web"]["authentication"]["jwt"]["ttl"], 604_800_000);
        assert!(v["web"]["authentication"].get("apiKeys").is_some());
        assert_eq!(v["soulseek"]["listenIpAddress"], "0.0.0.0");
        assert_eq!(v["soulseek"]["connection"]["buffer"]["writeQueue"], 50);
        assert_eq!(v["transfers"]["upload"]["speedLimit"], i32::MAX);
        assert_eq!(
            v["transfers"]["groups"]["default"]["upload"]["strategy"],
            "roundrobin"
        );
        assert!(v["transfers"]["groups"].get("userDefined").is_some());
        assert_eq!(
            v["transfers"]["download"]["destination"]["subdirectory"],
            "${SOURCE_DIRECTORY}"
        );
        assert_eq!(v["throttling"]["search"]["incoming"]["circuitBreaker"], 500);
        assert_eq!(
            v["integrations"]["pushbullet"]["notificationPrefix"],
            "From slskd:"
        );
    }

    #[test]
    fn partial_deserialize_keeps_defaults() {
        let o: Options =
            serde_json::from_str(r#"{"soulseek":{"username":"u"},"web":{"port":1}}"#).unwrap();
        assert_eq!(o.soulseek.username.as_deref(), Some("u"));
        assert_eq!(o.soulseek.port, 2271);
        assert_eq!(o.web.port, 1);
        assert_eq!(o.web.https.port, 5031);
    }

    #[test]
    fn redact_secrets() {
        let mut o = Options::default();
        o.soulseek.password = Some("pw".into());
        o.web.authentication.jwt.key = "k".into();
        o.web.authentication.api_keys.insert(
            "a".into(),
            ApiKeyOptions {
                key: Some("x".into()),
                ..Default::default()
            },
        );
        let r = o.redact();
        assert_eq!(r.soulseek.password.as_deref(), Some(REDACTED));
        assert_eq!(r.soulseek.username, None);
        assert_eq!(r.web.authentication.password, REDACTED);
        assert_eq!(r.web.authentication.jwt.key, REDACTED);
        assert_eq!(
            r.web.authentication.api_keys["a"].key.as_deref(),
            Some(REDACTED)
        );
        assert_eq!(r.web.authentication.api_keys["a"].role, "ReadOnly");
        assert_eq!(r.logger.loki, None);
        assert_eq!(r.metrics.authentication.password, REDACTED);
    }
}
