//! Authentication and authorization: server-side sign-in sessions (the `slskr_session`
//! cookie, see [`session`]), API keys (`X-API-Key`, CIDR restricted) and passthrough when
//! authentication is disabled.
//!
//! Cookie-authenticated requests that change state, and every WebSocket upgrade, must not
//! carry a foreign `Origin` (cross-site request forgery and WebSocket hijacking). API keys
//! are not ambient credentials and skip that check.

pub mod ip;
pub mod limit;
pub mod session;

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::warn;

pub use ip::{IpRange, parse_ranges};
pub use session::{COOKIE_NAME, SessionStore};

/// Name of the principal created by passthrough authentication.
pub const PASSTHROUGH_USERNAME: &str = "Anonymous";
/// Header carrying API keys.
pub const API_KEY_HEADER: &str = "X-API-Key";
/// CIDR list allowing every address.
pub const ALLOW_ALL_CIDR: &str = "0.0.0.0/0,::/0";
/// Delay before answering a failed sign-in.
pub const LOGIN_FAILURE_DELAY: Duration = Duration::from_millis(500);
/// Failed sign-ins allowed per address within [`LOGIN_FAILURE_WINDOW`]; after that the
/// address is refused, even with the right password, until the window ends.
pub const LOGIN_MAX_FAILURES: u32 = 10;
pub const LOGIN_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Addresses whose failed sign-ins are tracked at once.
const LOGIN_TRACKED_ADDRESSES: usize = 4096;
/// Password checks running at once; argon2 is slow on purpose, so a flood of sign-ins
/// would otherwise occupy every blocking thread.
const CONCURRENT_PASSWORD_CHECKS: usize = 4;

/// User roles; numeric values match slskd's `Role` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum Role {
    #[default]
    ReadOnly = 0,
    ReadWrite = 1,
    Administrator = 3,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ReadOnly",
            Self::ReadWrite => "ReadWrite",
            Self::Administrator => "Administrator",
        }
    }

    /// True when this role satisfies `required` (Administrator ⊃ ReadWrite ⊃ ReadOnly).
    pub fn satisfies(self, required: Role) -> bool {
        self >= required
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    /// Case-insensitive, like slskd's `ToEnum<Role>()`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        [Self::ReadOnly, Self::ReadWrite, Self::Administrator]
            .into_iter()
            .find(|role| role.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| {
                format!("invalid role '{s}'; expected readonly, readwrite or administrator")
            })
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Which authentication schemes an endpoint accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Policy {
    /// Session cookie or API key.
    #[default]
    Any = 0,
    /// Interactive sign-in only: API keys are refused.
    SessionOnly = 1,
    ApiKeyOnly = 2,
}

impl Policy {
    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::SessionOnly,
            2 => Self::ApiKeyOnly,
            _ => Self::Any,
        }
    }

    fn allows_session(self) -> bool {
        matches!(self, Self::Any | Self::SessionOnly)
    }

    fn allows_api_key(self) -> bool {
        matches!(self, Self::Any | Self::ApiKeyOnly)
    }
}

/// The secret of an API key: plaintext from `slskr.toml`, or the SHA-256 of a key created
/// from the web UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySecret {
    Plain(String),
    Sha256([u8; 32]),
}

impl ApiKeySecret {
    fn digest(&self) -> [u8; 32] {
        match self {
            Self::Plain(key) => sha256(key.as_bytes()),
            Self::Sha256(digest) => *digest,
        }
    }
}

impl From<&str> for ApiKeySecret {
    fn from(key: &str) -> Self {
        Self::Plain(key.to_owned())
    }
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    sha256(bytes).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// Parses a lowercase or uppercase hex SHA-256 digest.
pub fn parse_sha256_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(digest)
}

/// argon2id PHC hash of `password` with a random salt.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::{PasswordHasher, password_hash::SaltString};
    let salt = SaltString::encode_b64(&crate::random::<16>())
        .map_err(|e| anyhow::anyhow!("password salt: {e}"))?;
    Ok(argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
        .to_string())
}

/// True when `hash` is an argon2id PHC string.
pub fn valid_password_hash(hash: &str) -> bool {
    argon2::PasswordHash::new(hash)
        .is_ok_and(|hash| hash.algorithm == argon2::Algorithm::Argon2id.ident())
}

/// Checks `password` against an argon2 PHC hash (slow by design).
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::PasswordVerifier;
    argon2::PasswordHash::new(hash).is_ok_and(|hash| {
        argon2::Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

/// A configured API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub name: String,
    pub key: ApiKeySecret,
    pub role: Role,
    /// Comma separated list of address ranges allowed to use the key.
    pub cidr: String,
}

/// Authentication settings (`[web]`).
#[derive(Clone)]
pub struct AuthConfig {
    pub disabled: bool,
    pub username: String,
    /// Plaintext password from `slskr.toml` or the environment; takes precedence over
    /// `password_hash`.
    pub password: String,
    /// argon2id PHC hash of a password set from the web UI.
    pub password_hash: String,
    pub session_ttl: Duration,
    pub api_keys: Vec<ApiKey>,
}

impl AuthConfig {
    /// The password material sessions are bound to: changing it signs out every session.
    pub fn credential_secret(&self) -> String {
        credential_secret(&self.password, &self.password_hash)
    }
}

/// See [`AuthConfig::credential_secret`]; just the password while no hash is set, so
/// existing sessions survive the upgrade.
pub fn credential_secret(password: &str, password_hash: &str) -> String {
    if password_hash.is_empty() {
        password.to_owned()
    } else {
        format!("{password}\0{password_hash}")
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            username: "admin".into(),
            password: String::new(),
            password_hash: String::new(),
            session_ttl: session::DEFAULT_TTL,
            api_keys: Vec::new(),
        }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("disabled", &self.disabled)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("session_ttl", &self.session_ttl)
            .field(
                "api_keys",
                &self.api_keys.iter().map(|k| &k.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// How a principal was authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthMethod {
    Session,
    ApiKey,
    Passthrough,
}

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: Role,
    pub method: AuthMethod,
}

impl Principal {
    pub fn passthrough() -> Self {
        Self {
            name: PASSTHROUGH_USERNAME.into(),
            role: Role::Administrator,
            method: AuthMethod::Passthrough,
        }
    }

    /// The `GET /api/session` representation.
    pub fn session(&self) -> slskr_types::session::Session {
        use slskr_types::session::Role as ApiRole;
        slskr_types::session::Session {
            username: self.name.clone(),
            role: match self.role {
                Role::ReadOnly => ApiRole::ReadOnly,
                Role::ReadWrite => ApiRole::ReadWrite,
                Role::Administrator => ApiRole::Admin,
            },
        }
    }
}

/// Authorization failure: 401 (not authenticated) or 403 (role too low, or a cross-site
/// request carrying the session cookie), answered with a JSON error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
}

impl AuthError {
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        crate::http::error_response(self.status(), "")
    }
}

/// Reason an API key was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiKeyError {
    #[error("Unknown API key")]
    Unknown,
    #[error("IP address {ip} not included in CIDR range(s) for API key {name}; allowed: {allowed}")]
    OutOfRange {
        ip: IpAddr,
        name: String,
        allowed: String,
    },
}

/// Sign-in failure.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("Login disabled, check application logs for details")]
    Misconfigured,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    CrossSite,
    /// Too many failed sign-ins from this address; retry after the duration.
    #[error("Too many failed sign-ins; try again in {} s", limit::retry_seconds(*.0))]
    Limited(Duration),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<LoginError> for crate::http::ApiError {
    fn from(error: LoginError) -> Self {
        use crate::http::ApiError;
        match error {
            LoginError::BadRequest(message) => ApiError::BadRequest(message.into()),
            LoginError::Misconfigured => {
                ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
            LoginError::Unauthorized => ApiError::Unauthorized,
            LoginError::CrossSite => ApiError::Forbidden("Cross-site request refused".into()),
            LoginError::Limited(_) => {
                ApiError::Status(StatusCode::TOO_MANY_REQUESTS, error.to_string())
            }
            LoginError::Internal(error) => ApiError::Internal(error),
        }
    }
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        crate::http::ApiError::from(self).into_response()
    }
}

struct CompiledApiKey {
    name: String,
    /// SHA-256 of the key, so plaintext and hashed keys compare alike in constant time.
    digest: [u8; 32],
    role: Role,
    cidr: String,
    ranges: Vec<IpRange>,
}

struct Credentials {
    username: String,
    password: String,
    password_hash: String,
    api_keys: Vec<CompiledApiKey>,
}

impl Credentials {
    fn from_config(config: &AuthConfig) -> Self {
        let api_keys = config
            .api_keys
            .iter()
            .map(|k| CompiledApiKey {
                name: k.name.clone(),
                digest: k.key.digest(),
                role: k.role,
                cidr: k.cidr.clone(),
                ranges: k
                    .cidr
                    .split(',')
                    .filter_map(|cidr| match IpRange::parse(cidr) {
                        Ok(range) => Some(range),
                        Err(e) => {
                            warn!(key = %k.name, cidr, "ignoring invalid API key CIDR: {e}");
                            None
                        }
                    })
                    .collect(),
            })
            .collect();

        Self {
            username: config.username.clone(),
            password: config.password.clone(),
            password_hash: config.password_hash.clone(),
            api_keys,
        }
    }

    fn login_configured(&self) -> bool {
        !self.username.is_empty() && (!self.password.is_empty() || !self.password_hash.is_empty())
    }
}

struct Inner {
    disabled: bool,
    credentials: RwLock<Credentials>,
    sessions: SessionStore,
    /// Cookie `Path`: `web.url_base`, `/` at the root.
    cookie_path: String,
    login_failures: limit::FailureLimiter,
    password_checks: tokio::sync::Semaphore,
}

/// Authentication service; cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

/// Compares two byte strings in time independent of their contents (lengths may leak).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Caller address from axum's `ConnectInfo`; unspecified when the server was not
/// started with `into_make_service_with_connect_info`. An unspecified address means
/// the caller is unknown and never satisfies a CIDR-restricted
/// API key.
pub fn remote_addr(parts: &Parts) -> SocketAddr {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), |info| info.0)
}

/// Extractor for the caller address (see [`remote_addr`]); never rejects.
pub struct RemoteAddr(pub SocketAddr);

impl<S: Send + Sync> FromRequestParts<S> for RemoteAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(remote_addr(parts)))
    }
}

/// Values of the session cookie in the request's `Cookie` headers.
pub fn session_tokens(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(header::COOKIE)
        .into_iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name.trim() == COOKIE_NAME).then(|| value.trim().trim_matches('"'))
        })
        .filter(|token| !token.is_empty())
}

/// True when the request arrived over HTTPS at a reverse proxy (`X-Forwarded-Proto`).
pub fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::UPGRADE)
        .into_iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|protocol| protocol.trim().eq_ignore_ascii_case("websocket"))
}

/// True for requests an ambient credential must not authorize cross-site: anything but
/// GET, HEAD and OPTIONS, and every WebSocket upgrade.
pub fn needs_origin_check(method: &Method, headers: &HeaderMap) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || is_websocket_upgrade(headers)
}

/// The request's `Host` header, or the authority of an absolute URI (HTTP/2).
pub fn request_host<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .or_else(|| uri.authority().map(|authority| authority.as_str()))
}

/// The name in a `Host` value, without the port or IPv6 brackets.
fn host_name(host: &str) -> Option<&str> {
    let host = host.trim();
    match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(name, _)| name),
        None => Some(host.rsplit_once(':').map_or(host, |(name, _)| name)),
    }
}

fn localhost(name: &str) -> bool {
    let name = name.trim_end_matches('.');
    name.eq_ignore_ascii_case("localhost")
        || name
            .len()
            .checked_sub(".localhost".len())
            .and_then(|start| name.get(start..))
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".localhost"))
}

/// True for `localhost` and loopback IP literals, with or without a port.
pub fn loopback_host(host: &str) -> bool {
    host_name(host).is_some_and(|name| {
        localhost(name)
            || name
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.to_canonical().is_loopback())
    })
}

/// True for a `Host` that a DNS-rebinding page cannot send: `localhost` or an IP literal.
/// Any other name could resolve to this daemon from an attacker's page, which the browser
/// then treats as same-origin.
pub fn unrebindable_host(host: &str) -> bool {
    host_name(host).is_some_and(|name| localhost(name) || name.parse::<IpAddr>().is_ok())
}

/// True when the request has no `Origin` header or its host and port equal the request
/// `Host` (forwarded host headers are not trusted).
pub fn same_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some((scheme, authority)) = origin.to_str().ok().and_then(|o| o.split_once("://")) else {
        // Includes the opaque origin "null".
        return false;
    };
    let default_port = if scheme.eq_ignore_ascii_case("http") {
        ":80"
    } else if scheme.eq_ignore_ascii_case("https") {
        ":443"
    } else {
        return false;
    };
    let Some(host) = request_host(headers, uri) else {
        return false;
    };
    let normalize = |value: &str| {
        let value = value.trim().to_ascii_lowercase();
        match value.strip_suffix(default_port) {
            Some(stripped) => stripped.to_owned(),
            None => value,
        }
    };
    !authority.contains(['/', '@']) && normalize(authority) == normalize(host)
}

fn cross_site(parts: &Parts) -> bool {
    needs_origin_check(&parts.method, &parts.headers) && !same_origin(&parts.headers, &parts.uri)
}

impl Auth {
    pub fn new(config: AuthConfig, sessions: SessionStore) -> Self {
        sessions.set_ttl(config.session_ttl);
        Self {
            inner: Arc::new(Inner {
                disabled: config.disabled,
                credentials: RwLock::new(Credentials::from_config(&config)),
                sessions,
                cookie_path: "/".into(),
                login_failures: limit::FailureLimiter::new(
                    LOGIN_MAX_FAILURES,
                    LOGIN_FAILURE_WINDOW,
                    LOGIN_TRACKED_ADDRESSES,
                ),
                password_checks: tokio::sync::Semaphore::new(CONCURRENT_PASSWORD_CHECKS),
            }),
        }
    }

    /// Sets `web.url_base`, the session cookie's path. Call before cloning.
    pub fn with_url_base(mut self, url_base: &str) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("Auth configured before sharing");
        let base = url_base.trim_end_matches('/');
        inner.cookie_path = if base.is_empty() {
            "/".into()
        } else {
            base.to_owned()
        };
        self
    }

    /// The session registry.
    pub fn sessions(&self) -> &SessionStore {
        &self.inner.sessions
    }

    /// Applies hot-reloadable settings (username, password, session TTL, API keys).
    /// Changing the username or password signs out every session. `disabled` requires a
    /// restart.
    pub fn update(&self, config: &AuthConfig) {
        let changed = {
            let mut credentials = self
                .inner
                .credentials
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let changed = credentials.username != config.username
                || credentials.password != config.password
                || credentials.password_hash != config.password_hash;
            *credentials = Credentials::from_config(config);
            changed
        };
        self.inner.sessions.set_ttl(config.session_ttl);
        if changed {
            self.inner
                .sessions
                .reset(&config.username, &config.credential_secret());
        }
    }

    /// True when authentication is enabled (`GET /api/auth`).
    pub fn enabled(&self) -> bool {
        !self.inner.disabled
    }

    /// Checks an API key and the caller's address against its CIDR ranges.
    pub fn authenticate_api_key(
        &self,
        key: &str,
        remote: IpAddr,
    ) -> Result<Principal, ApiKeyError> {
        let credentials = self
            .inner
            .credentials
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let candidate = sha256(key.as_bytes());

        let record = credentials
            .api_keys
            .iter()
            .find(|k| constant_time_eq(&k.digest, &candidate))
            .ok_or(ApiKeyError::Unknown)?;

        let remote = remote.to_canonical();
        // An unspecified address marks an unknown caller; keys open to a whole address
        // family still accept it.
        let allowed = if remote.is_unspecified() {
            record.ranges.iter().any(IpRange::is_full)
        } else {
            record.ranges.iter().any(|range| range.contains(remote))
        };
        if !allowed {
            return Err(ApiKeyError::OutOfRange {
                ip: remote,
                name: record.name.clone(),
                allowed: record.cidr.clone(),
            });
        }

        Ok(Principal {
            name: record.name.clone(),
            role: record.role,
            method: AuthMethod::ApiKey,
        })
    }

    fn api_key_scheme(&self, parts: &Parts, remote: IpAddr) -> Option<Principal> {
        let key = parts.headers.get(API_KEY_HEADER)?;
        let key = key.to_str().unwrap_or_default();

        self.authenticate_api_key(key, remote)
            .inspect_err(|e| warn!("Unauthorized request from IP address {remote}: {e}"))
            .ok()
    }

    /// The principal of the first live session among the request's session cookies.
    pub fn session_principal(&self, headers: &HeaderMap) -> Option<Principal> {
        session_tokens(headers).find_map(|token| {
            self.inner.sessions.lookup(token).map(|record| Principal {
                name: record.username,
                role: record.role,
                method: AuthMethod::Session,
            })
        })
    }

    /// Authenticates a request with the `Any` policy.
    pub fn authenticate(&self, parts: &Parts, remote: SocketAddr) -> Result<Principal, AuthError> {
        self.authenticate_with_policy(parts, remote, Policy::Any)
    }

    /// Authenticates using the schemes allowed by `policy`; the strongest role wins when
    /// both schemes succeed. A session presented cross-site is refused with 403 unless an
    /// API key authenticates the request.
    pub fn authenticate_with_policy(
        &self,
        parts: &Parts,
        remote: SocketAddr,
        policy: Policy,
    ) -> Result<Principal, AuthError> {
        let cross_site = cross_site(parts);
        let api_key = policy
            .allows_api_key()
            .then(|| self.api_key_scheme(parts, remote.ip()))
            .flatten();

        if self.inner.disabled {
            // Without credentials, only the Host keeps another site out: a page on a name
            // the attacker rebinds to this address is same-origin to the browser, so only
            // `localhost` and IP literals are served.
            let rebindable =
                !request_host(&parts.headers, &parts.uri).is_some_and(unrebindable_host);
            return if (cross_site || rebindable) && api_key.is_none() {
                Err(AuthError::Forbidden)
            } else {
                Ok(Principal::passthrough())
            };
        }

        let session = policy
            .allows_session()
            .then(|| self.session_principal(&parts.headers))
            .flatten();
        match (api_key, session) {
            (Some(key), Some(session)) if !cross_site && session.role > key.role => Ok(session),
            (Some(key), _) => Ok(key),
            (None, Some(_)) if cross_site => Err(AuthError::Forbidden),
            (None, Some(session)) => Ok(session),
            (None, None) => Err(AuthError::Unauthorized),
        }
    }

    /// Authenticates using the caller address from `ConnectInfo` (see [`remote_addr`]).
    pub fn authenticate_request(&self, parts: &Parts) -> Result<Principal, AuthError> {
        self.authenticate(parts, remote_addr(parts))
    }

    /// Authenticates and checks the role: 401 when unauthenticated, 403 when the role is
    /// insufficient or a session is used cross-site.
    pub fn authorize(
        &self,
        parts: &Parts,
        remote: SocketAddr,
        role: Role,
        policy: Policy,
    ) -> Result<Principal, AuthError> {
        let principal = self.authenticate_with_policy(parts, remote, policy)?;
        if principal.role.satisfies(role) {
            Ok(principal)
        } else {
            Err(AuthError::Forbidden)
        }
    }

    /// Checks the web UI credentials: the plaintext password from the file or environment
    /// in constant time, otherwise the argon2 hash of a password set from the UI (slow;
    /// call from a blocking context). The password is checked even when the username does
    /// not match, so timing does not reveal the username.
    pub fn check_credentials(&self, username: &str, password: &str) -> bool {
        let (expected_username, plain, hash) = {
            let credentials = self
                .inner
                .credentials
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            (
                credentials.username.clone(),
                credentials.password.clone(),
                credentials.password_hash.clone(),
            )
        };
        let username_matches = constant_time_eq(expected_username.as_bytes(), username.as_bytes());
        let password_matches = if !plain.is_empty() {
            constant_time_eq(plain.as_bytes(), password.as_bytes())
        } else if !hash.is_empty() {
            verify_password(password, &hash)
        } else {
            false
        };
        username_matches & password_matches
    }

    /// Signs in with the web credentials for a caller at `remote`: returns the new session's
    /// token and principal. Failures are answered after [`LOGIN_FAILURE_DELAY`] and counted
    /// against the address, which is refused for a while after [`LOGIN_MAX_FAILURES`].
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        remote: IpAddr,
    ) -> Result<(String, Principal), LoginError> {
        if let Some(wait) = self.inner.login_failures.limited(remote, Instant::now()) {
            return Err(LoginError::Limited(wait));
        }
        if username.trim().is_empty() || password.trim().is_empty() {
            return Err(LoginError::BadRequest(
                "Username and/or Password missing or invalid",
            ));
        }

        let configured = self
            .inner
            .credentials
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .login_configured();
        if !configured {
            tracing::error!(
                "Authentication is misconfigured; when enabled, username and password must be at least one character. Login is disabled."
            );
            return Err(LoginError::Misconfigured);
        }

        let matches = {
            let _permit = self
                .inner
                .password_checks
                .acquire()
                .await
                .map_err(|e| anyhow::anyhow!("checking credentials: {e}"))?;
            let (auth, username, password) =
                (self.clone(), username.to_owned(), password.to_owned());
            tokio::task::spawn_blocking(move || auth.check_credentials(&username, &password))
                .await
                .map_err(|e| anyhow::anyhow!("checking credentials: {e}"))?
        };
        if !matches {
            if let Err(wait) = self.inner.login_failures.fail(remote, Instant::now()) {
                return Err(LoginError::Limited(wait));
            }
            warn!(%remote, "failed web sign-in");
            tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
            return Err(LoginError::Unauthorized);
        }
        self.inner.login_failures.clear(remote);
        let token = self
            .inner
            .sessions
            .create(username, Role::Administrator)
            .await?;
        Ok((
            token,
            Principal {
                name: username.to_owned(),
                role: Role::Administrator,
                method: AuthMethod::Session,
            },
        ))
    }

    /// Deletes every session named by the request's cookies.
    pub async fn logout(&self, headers: &HeaderMap) -> anyhow::Result<()> {
        for token in session_tokens(headers) {
            self.inner.sessions.remove(token).await?;
        }
        Ok(())
    }

    fn cookie(&self, value: &str, max_age: u64, secure: bool) -> HeaderValue {
        let secure = if secure { "; Secure" } else { "" };
        let cookie = format!(
            "{COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path={}; Max-Age={max_age}{secure}",
            self.inner.cookie_path
        );
        HeaderValue::from_str(&cookie).expect("session cookie is a valid header value")
    }

    /// `Set-Cookie` value carrying `token` for the session TTL; `Secure` when `secure`.
    pub fn session_cookie(&self, token: &str, secure: bool) -> HeaderValue {
        self.cookie(token, self.inner.sessions.ttl().as_secs(), secure)
    }

    /// `Set-Cookie` value deleting the session cookie.
    pub fn clear_cookie(&self, secure: bool) -> HeaderValue {
        self.cookie("", 0, secure)
    }
}

/// Middleware re-sending the session cookie when a request extended its session, so the
/// browser's copy slides along with the server-side expiry.
pub async fn refresh_session_cookie(
    State(auth): State<Auth>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let refreshed = if auth.inner.disabled || !same_origin(headers, request.uri()) {
        None
    } else {
        session_tokens(headers)
            .find(|token| auth.inner.sessions.touch(token))
            .map(str::to_owned)
    };
    let secure = forwarded_https(headers);
    let mut response = next.run(request).await;
    if let Some(token) = refreshed {
        let prefix = format!("{COOKIE_NAME}=");
        let sets_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .into_iter()
            .any(|value| value.as_bytes().starts_with(prefix.as_bytes()));
        if !sets_cookie {
            response
                .headers_mut()
                .append(header::SET_COOKIE, auth.session_cookie(&token, secure));
        }
    }
    response
}

/// Extractor requiring an authenticated caller with at least role `R` (a [`Role`] as `u8`)
/// via policy `P` (a [`Policy`] as `u8`). Rejects with 401/403. Needs `Auth: FromRef<S>`.
#[derive(Debug, Clone)]
pub struct RequireRole<const R: u8, const P: u8 = { Policy::Any as u8 }>(pub Principal);

/// Any authenticated caller.
pub type Authenticated = RequireRole<{ Role::ReadOnly as u8 }>;
/// Read-write or administrator.
pub type RequireReadWrite = RequireRole<{ Role::ReadWrite as u8 }>;
/// Administrator.
pub type RequireAdmin = RequireRole<{ Role::Administrator as u8 }>;
/// Administrator signed in interactively (API keys refused).
pub type RequireAdminSession =
    RequireRole<{ Role::Administrator as u8 }, { Policy::SessionOnly as u8 }>;

const fn role_from_u8(v: u8) -> Role {
    match v {
        0 => Role::ReadOnly,
        1 | 2 => Role::ReadWrite,
        _ => Role::Administrator,
    }
}

impl<S, const R: u8, const P: u8> FromRequestParts<S> for RequireRole<R, P>
where
    Auth: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = Auth::from_ref(state);
        let remote = remote_addr(parts);
        auth.authorize(parts, remote, role_from_u8(R), Policy::from_u8(P))
            .map(Self)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
