//! Authentication and authorization: server-side sign-in sessions (the `slskr_session`
//! cookie, see [`session`]), API keys (`X-API-Key`, CIDR restricted) and passthrough when
//! authentication is disabled.
//!
//! Cookie-authenticated requests that change state, and every WebSocket upgrade, must not
//! carry a foreign `Origin` (cross-site request forgery and WebSocket hijacking). API keys
//! are not ambient credentials and skip that check.

pub mod ip;
pub mod session;

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
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

/// A configured API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub name: String,
    pub key: String,
    pub role: Role,
    /// Comma separated list of address ranges allowed to use the key.
    pub cidr: String,
}

/// Authentication settings (`[web]`).
#[derive(Clone)]
pub struct AuthConfig {
    pub disabled: bool,
    pub username: String,
    pub password: String,
    pub session_ttl: Duration,
    pub api_keys: Vec<ApiKey>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            username: "admin".into(),
            password: String::new(),
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

    /// The `GET /session` representation.
    pub fn session(&self) -> slskr_api::session::Session {
        slskr_api::session::Session {
            username: self.name.clone(),
            role: self.role.as_str().to_owned(),
        }
    }
}

/// Authorization failure: 401 (not authenticated) or 403 (role too low, or a cross-site
/// request carrying the session cookie). Both have empty bodies.
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
        self.status().into_response()
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
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, Json(message)).into_response(),
            Self::Misconfigured => {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(self.to_string())).into_response()
            }
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::CrossSite => StatusCode::FORBIDDEN.into_response(),
            Self::Internal(error) => crate::http::ApiError::Internal(error).into_response(),
        }
    }
}

struct CompiledApiKey {
    name: String,
    key: Vec<u8>,
    role: Role,
    cidr: String,
    ranges: Vec<IpRange>,
}

struct Credentials {
    username: String,
    password: String,
    api_keys: Vec<CompiledApiKey>,
}

impl Credentials {
    fn from_config(config: &AuthConfig) -> Self {
        let api_keys = config
            .api_keys
            .iter()
            .map(|k| CompiledApiKey {
                name: k.name.clone(),
                key: k.key.as_bytes().to_vec(),
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
            api_keys,
        }
    }
}

struct Inner {
    disabled: bool,
    credentials: RwLock<Credentials>,
    sessions: SessionStore,
    /// Cookie `Path`: `web.url_base`, `/` at the root.
    cookie_path: String,
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
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .or_else(|| uri.authority().map(|authority| authority.as_str()))
    else {
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
            let changed =
                credentials.username != config.username || credentials.password != config.password;
            *credentials = Credentials::from_config(config);
            changed
        };
        self.inner.sessions.set_ttl(config.session_ttl);
        if changed {
            self.inner
                .sessions
                .reset(&config.username, &config.password);
        }
    }

    /// True when authentication is enabled (`GET /session/enabled`).
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
        let candidate = key.as_bytes();

        let record = credentials
            .api_keys
            .iter()
            .find(|k| constant_time_eq(&k.key, candidate))
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
            // Nothing to steal, but a page on another site must not drive the daemon.
            return if cross_site && api_key.is_none() {
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

    /// Authorizer for [`crate::signalr::Hub::set_authorizer`] requiring `role` (policy `Any`).
    pub fn hub_authorizer(
        &self,
        role: Role,
    ) -> impl Fn(&Parts) -> Result<(), StatusCode> + Send + Sync + 'static {
        let auth = self.clone();
        move |parts| {
            auth.authorize(parts, remote_addr(parts), role, Policy::Any)
                .map(drop)
                .map_err(AuthError::status)
        }
    }

    /// Constant-time check of the configured web UI credentials.
    pub fn check_credentials(&self, username: &str, password: &str) -> bool {
        let credentials = self
            .inner
            .credentials
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let username_matches =
            constant_time_eq(credentials.username.as_bytes(), username.as_bytes());
        let password_matches =
            constant_time_eq(credentials.password.as_bytes(), password.as_bytes());
        username_matches & password_matches
    }

    /// Signs in with the web credentials: returns the new session's token and principal.
    /// Failures are answered after [`LOGIN_FAILURE_DELAY`].
    pub async fn login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(String, Principal), LoginError> {
        if username.trim().is_empty() || password.trim().is_empty() {
            return Err(LoginError::BadRequest(
                "Username and/or Password missing or invalid",
            ));
        }

        {
            let credentials = self
                .inner
                .credentials
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if credentials.username.is_empty() || credentials.password.is_empty() {
                tracing::error!(
                    "Authentication is misconfigured; when enabled, username and password must be at least one character. Login is disabled."
                );
                return Err(LoginError::Misconfigured);
            }
        }

        if !self.check_credentials(username, password) {
            tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
            return Err(LoginError::Unauthorized);
        }
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
