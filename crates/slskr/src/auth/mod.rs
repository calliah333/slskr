//! Authentication and authorization with slskd semantics: JWT bearer tokens, API keys
//! (`X-API-Key`, CIDR restricted) and passthrough when authentication is disabled.

pub mod ip;

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts, OriginalUri, Query};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tracing::warn;

pub use ip::{IpRange, parse_ranges};

/// JWT issuer, as slskd sets it (`Program.AppName`).
pub const ISSUER: &str = "slskd";
/// Clock skew tolerated when validating token lifetimes (ASP.NET `ClockSkew`).
pub const CLOCK_SKEW_SECS: u64 = 300;
/// Default JWT lifetime in milliseconds (7 days).
pub const DEFAULT_JWT_TTL_MS: u64 = 604_800_000;
/// Name of the principal created by passthrough authentication.
pub const PASSTHROUGH_USERNAME: &str = "Anonymous";
/// Header carrying API keys.
pub const API_KEY_HEADER: &str = "X-API-Key";
/// Name given to the primary (`web.authentication.api_key`) key.
pub const PRIMARY_API_KEY_NAME: &str = "PrimaryApiKey";
/// CIDR list allowing every address.
pub const ALLOW_ALL_CIDR: &str = "0.0.0.0/0,::/0";

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

/// Which authentication schemes an endpoint accepts (slskd `AuthPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Policy {
    /// JWT or API key.
    #[default]
    Any = 0,
    JwtOnly = 1,
    ApiKeyOnly = 2,
}

impl Policy {
    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::JwtOnly,
            2 => Self::ApiKeyOnly,
            _ => Self::Any,
        }
    }

    fn allows_jwt(self) -> bool {
        matches!(self, Self::Any | Self::JwtOnly)
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

impl ApiKey {
    /// Parses slskd's primary key syntax `[role=<role>;][cidr=<cidrs>;]<key>`
    /// (defaults: Administrator, all addresses). Returns `None` for blank input.
    pub fn parse_primary(value: &str) -> Option<Result<Self, String>> {
        if value.trim().is_empty() {
            return None;
        }

        let mut key = None;
        let mut role = None;
        let mut cidr = None;

        for tuple in value.split(';') {
            let lower = tuple.to_ascii_lowercase();
            if lower.starts_with("role=") {
                role = tuple.rsplit('=').next().map(str::to_owned);
            } else if lower.starts_with("cidr=") {
                cidr = tuple.rsplit('=').next().map(str::to_owned);
            } else {
                key = Some(tuple.to_owned());
            }
        }

        let key = key.filter(|k| !k.is_empty())?;

        let role = match role.filter(|r| !r.trim().is_empty()) {
            Some(role) => match role.parse() {
                Ok(role) => role,
                Err(e) => return Some(Err(e)),
            },
            None => Role::Administrator,
        };

        let cidr = cidr
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| ALLOW_ALL_CIDR.to_owned());

        Some(Ok(Self {
            name: PRIMARY_API_KEY_NAME.to_owned(),
            key,
            role,
            cidr,
        }))
    }
}

/// Authentication settings (slskd `web.authentication`).
#[derive(Clone)]
pub struct AuthConfig {
    pub disabled: bool,
    pub username: String,
    pub password: String,
    /// HMAC-SHA256 signing key (slskd uses the UTF-8 bytes of `jwt.key`).
    pub jwt_key: Vec<u8>,
    pub jwt_ttl_ms: u64,
    pub api_keys: Vec<ApiKey>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            username: "slskd".into(),
            password: "slskd".into(),
            jwt_key: rand::random::<[u8; 32]>().to_vec(),
            jwt_ttl_ms: DEFAULT_JWT_TTL_MS,
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
            .field("jwt_key", &"<redacted>")
            .field("jwt_ttl_ms", &self.jwt_ttl_ms)
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
    Jwt,
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
    fn passthrough() -> Self {
        Self {
            name: PASSTHROUGH_USERNAME.into(),
            role: Role::Administrator,
            method: AuthMethod::Passthrough,
        }
    }
}

/// Login response, serialised exactly like slskd's `TokenResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    /// Expiry, unix seconds.
    pub expires: i64,
    /// Issue time (`ValidFrom`), unix seconds.
    pub issued: i64,
    pub name: String,
    pub not_before: i64,
    pub token: String,
    pub token_type: String,
}

/// Claims emitted with the names .NET's `JwtSecurityTokenHandler` produces for slskd's claims.
#[derive(Serialize)]
struct OutgoingClaims<'a> {
    unique_name: &'a str,
    nameid: String,
    role: &'static str,
    name: &'a str,
    iat: i64,
    nbf: i64,
    exp: i64,
    iss: &'static str,
}

#[derive(Deserialize)]
struct IncomingClaims {
    #[serde(default)]
    unique_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<Value>,
}

/// Authorization failure, rendered like ASP.NET: 401 (no body) or 403 (no body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => {
                let mut res = StatusCode::UNAUTHORIZED.into_response();
                res.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                res
            }
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
        }
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

/// Login failure, rendered like slskd's `SessionController.Login`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("Login disabled, check application logs for details")]
    Misconfigured,
    #[error("unauthorized")]
    Unauthorized,
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, Json(message)).into_response(),
            Self::Misconfigured => {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(self.to_string())).into_response()
            }
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
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
    ttl_ms: u64,
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
    credentials: RwLock<Credentials>,
}

/// Authentication service; cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Compares two byte strings in time independent of their contents (lengths may leak).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Value of an `Authorization: Bearer <token>` header (scheme matched case-insensitively).
fn bearer_token(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if !value.get(..7)?.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    value
        .rsplit(' ')
        .next()
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// slskd only reads `access_token` (and bearer API keys) for SignalR hub paths.
fn is_hub_request(parts: &Parts) -> bool {
    let path = parts
        .extensions
        .get::<OriginalUri>()
        .map_or_else(|| parts.uri.path(), |uri| uri.0.path());
    path.contains("/hub/") || path.ends_with("/hub")
}

/// Caller address from axum's `ConnectInfo`; unspecified when the server was not
/// started with `into_make_service_with_connect_info`.
pub fn remote_addr(parts: &Parts) -> SocketAddr {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), |info| info.0)
}

impl Auth {
    pub fn new(config: AuthConfig) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = CLOCK_SKEW_SECS;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = false;
        validation.set_issuer(&[ISSUER]);
        validation.set_required_spec_claims(&["exp", "iss"]);

        Self {
            inner: Arc::new(Inner {
                disabled: config.disabled,
                ttl_ms: config.jwt_ttl_ms,
                encoding: EncodingKey::from_secret(&config.jwt_key),
                decoding: DecodingKey::from_secret(&config.jwt_key),
                validation,
                credentials: RwLock::new(Credentials::from_config(&config)),
            }),
        }
    }

    /// Applies hot-reloadable settings (username, password, API keys). JWT key/TTL and
    /// `disabled` require a restart, as in slskd.
    pub fn update(&self, config: &AuthConfig) {
        *self
            .inner
            .credentials
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Credentials::from_config(config);
    }

    /// True when authentication is enabled (`GET /session/enabled`).
    pub fn enabled(&self) -> bool {
        !self.inner.disabled
    }

    /// Issues a JWT with the configured TTL.
    pub fn issue_token(&self, username: &str, role: Role) -> TokenResponse {
        self.issue_token_with_ttl(username, role, Duration::from_millis(self.inner.ttl_ms))
    }

    /// Issues a JWT valid for `ttl`.
    pub fn issue_token_with_ttl(&self, username: &str, role: Role, ttl: Duration) -> TokenResponse {
        self.issue_at(
            username,
            role,
            now_ms(),
            i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
        )
    }

    fn issue_at(&self, username: &str, role: Role, issued_ms: i64, ttl_ms: i64) -> TokenResponse {
        let issued = issued_ms.div_euclid(1000);
        let expires = issued_ms.saturating_add(ttl_ms).div_euclid(1000);

        let claims = OutgoingClaims {
            unique_name: username,
            nameid: uuid::Uuid::new_v4().to_string(),
            role: role.as_str(),
            name: username,
            iat: issued,
            nbf: issued,
            exp: expires,
            iss: ISSUER,
        };

        // HMAC signing of a serialisable struct cannot fail
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &self.inner.encoding,
        )
        .unwrap_or_default();

        TokenResponse {
            expires,
            issued,
            name: username.to_owned(),
            not_before: issued,
            token,
            token_type: "Bearer".into(),
        }
    }

    /// Validates signature, issuer and lifetime (5 minute skew) of a JWT.
    pub fn validate_token(&self, token: &str) -> Option<Principal> {
        let data = jsonwebtoken::decode::<IncomingClaims>(
            token,
            &self.inner.decoding,
            &self.inner.validation,
        )
        .ok()?;

        let claims = data.claims;
        let name = claims.unique_name.or(claims.name)?;
        let role = match claims.role? {
            Value::String(role) => role.parse().ok()?,
            Value::Array(roles) => roles
                .iter()
                .filter_map(|r| r.as_str().and_then(|r| r.parse::<Role>().ok()))
                .max()?,
            _ => return None,
        };

        Some(Principal {
            name,
            role,
            method: AuthMethod::Jwt,
        })
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
        if !record.ranges.iter().any(|range| range.contains(remote)) {
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

    fn jwt_scheme(&self, parts: &Parts, remote: IpAddr) -> Option<Principal> {
        if is_hub_request(parts) {
            // SignalR: `access_token` query parameter wins; a bearer value may also be an API key
            let query = Query::<HashMap<String, String>>::try_from_uri(&parts.uri).ok();
            if let Some(token) = query.as_ref().and_then(|q| q.0.get("access_token")) {
                return self.validate_token(token);
            }

            let token = bearer_token(parts)?;
            if let Ok(principal) = self.authenticate_api_key(token, remote) {
                return Some(principal);
            }
            return self.validate_token(token);
        }

        self.validate_token(bearer_token(parts)?)
    }

    /// Authenticates a request with the `Any` policy.
    pub fn authenticate(&self, parts: &Parts, remote: SocketAddr) -> Option<Principal> {
        self.authenticate_with_policy(parts, remote, Policy::Any)
    }

    /// Authenticates using the schemes allowed by `policy`; the strongest role wins when
    /// several schemes succeed (ASP.NET merges the identities).
    pub fn authenticate_with_policy(
        &self,
        parts: &Parts,
        remote: SocketAddr,
        policy: Policy,
    ) -> Option<Principal> {
        if self.inner.disabled {
            return Some(Principal::passthrough());
        }

        let remote = remote.ip();
        let api_key = policy
            .allows_api_key()
            .then(|| self.api_key_scheme(parts, remote))
            .flatten();
        let jwt = policy
            .allows_jwt()
            .then(|| self.jwt_scheme(parts, remote))
            .flatten();

        match (api_key, jwt) {
            (Some(a), Some(j)) => Some(if j.role > a.role { j } else { a }),
            (a, j) => a.or(j),
        }
    }

    /// Authenticates using the caller address from `ConnectInfo` (see [`remote_addr`]).
    pub fn authenticate_request(&self, parts: &Parts) -> Option<Principal> {
        self.authenticate(parts, remote_addr(parts))
    }

    /// Authenticates and checks the role: 401 when unauthenticated, 403 when the role is insufficient.
    pub fn authorize(
        &self,
        parts: &Parts,
        remote: SocketAddr,
        role: Role,
        policy: Policy,
    ) -> Result<Principal, AuthError> {
        let principal = self
            .authenticate_with_policy(parts, remote, policy)
            .ok_or(AuthError::Unauthorized)?;
        if principal.role.satisfies(role) {
            Ok(principal)
        } else {
            Err(AuthError::Forbidden)
        }
    }

    /// Authorizer for [`crate::signalr::Hub::set_authorizer`] requiring `role` (policy `Any`).
    pub fn hub_authorizer(&self, role: Role) -> impl Fn(&Parts) -> bool + Send + Sync + 'static {
        let auth = self.clone();
        move |parts| {
            auth.authenticate_request(parts)
                .is_some_and(|p| p.role.satisfies(role))
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

    /// slskd `POST /session` semantics: validates input and issues an Administrator token.
    /// Headless mode is the caller's concern.
    pub fn login(&self, username: &str, password: &str) -> Result<TokenResponse, LoginError> {
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

        if self.check_credentials(username, password) {
            Ok(self.issue_token(username, Role::Administrator))
        } else {
            Err(LoginError::Unauthorized)
        }
    }
}

/// Extractor requiring an authenticated caller with at least role `R` (a [`Role`] as `u8`)
/// via policy `P` (a [`Policy`] as `u8`). Rejects with 401/403. Needs `Auth: FromRef<S>`.
#[derive(Debug, Clone)]
pub struct RequireRole<const R: u8, const P: u8 = { Policy::Any as u8 }>(pub Principal);

/// Any authenticated caller (`AuthPolicy.Any`).
pub type Authenticated = RequireRole<{ Role::ReadOnly as u8 }>;
/// `AuthRole.ReadWriteOrAdministrator`.
pub type RequireReadWrite = RequireRole<{ Role::ReadWrite as u8 }>;
/// `AuthRole.AdministratorOnly`.
pub type RequireAdmin = RequireRole<{ Role::Administrator as u8 }>;
/// `AuthPolicy.JwtOnly` + `AuthRole.AdministratorOnly`.
pub type RequireAdminJwt = RequireRole<{ Role::Administrator as u8 }, { Policy::JwtOnly as u8 }>;
/// `AuthPolicy.ApiKeyOnly` + `AuthRole.Any`.
pub type RequireApiKey = RequireRole<{ Role::ReadOnly as u8 }, { Policy::ApiKeyOnly as u8 }>;
/// `AuthPolicy.ApiKeyOnly` + `AuthRole.ReadWriteOrAdministrator`.
pub type RequireReadWriteApiKey =
    RequireRole<{ Role::ReadWrite as u8 }, { Policy::ApiKeyOnly as u8 }>;

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
mod tests {
    use super::*;
    use axum::http::Request;

    fn config() -> AuthConfig {
        AuthConfig {
            disabled: false,
            username: "admin".into(),
            password: "hunter2".into(),
            jwt_key: b"0123456789abcdef0123456789abcdef".to_vec(),
            jwt_ttl_ms: DEFAULT_JWT_TTL_MS,
            api_keys: vec![
                ApiKey {
                    name: "local".into(),
                    key: "local-key-0123456789".into(),
                    role: Role::ReadWrite,
                    cidr: "127.0.0.0/8,::1/128".into(),
                },
                ApiKey {
                    name: "anywhere".into(),
                    key: "anywhere-key-0123456789".into(),
                    role: Role::ReadOnly,
                    cidr: ALLOW_ALL_CIDR.into(),
                },
            ],
        }
    }

    fn parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::get(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn addr(s: &str) -> SocketAddr {
        SocketAddr::new(s.parse().unwrap(), 1234)
    }

    fn decode_payload(token: &str) -> Value {
        use base64::Engine as _;
        let payload = token.split('.').nth(1).unwrap();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn token_round_trip() {
        let auth = Auth::new(config());
        let response = auth.issue_token("admin", Role::Administrator);

        assert_eq!(response.token_type, "Bearer");
        assert_eq!(response.name, "admin");
        assert_eq!(response.issued, response.not_before);
        assert_eq!(response.expires - response.issued, 604_800);

        let principal = auth.validate_token(&response.token).unwrap();
        assert_eq!(
            principal,
            Principal {
                name: "admin".into(),
                role: Role::Administrator,
                method: AuthMethod::Jwt
            }
        );

        let payload = decode_payload(&response.token);
        assert_eq!(payload["iss"], "slskd");
        assert_eq!(payload["name"], "admin");
        assert_eq!(payload["unique_name"], "admin");
        assert_eq!(payload["role"], "Administrator");
        assert_eq!(payload["exp"], response.expires);
        assert_eq!(payload["nbf"], response.issued);
        assert!(payload["nameid"].as_str().is_some_and(|id| id.len() == 36));

        let json = serde_json::to_value(&response).unwrap();
        for field in [
            "expires",
            "issued",
            "name",
            "notBefore",
            "token",
            "tokenType",
        ] {
            assert!(json.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn token_rejected_with_other_key_or_tampering() {
        let auth = Auth::new(config());
        let token = auth.issue_token("admin", Role::ReadOnly).token;

        let other = Auth::new(AuthConfig {
            jwt_key: b"another-key-another-key-another!".to_vec(),
            ..config()
        });
        assert!(other.validate_token(&token).is_none());

        let mut tampered = token.clone();
        tampered.pop();
        tampered.push(if token.ends_with('A') { 'B' } else { 'A' });
        assert!(auth.validate_token(&tampered).is_none());
        assert!(auth.validate_token("garbage").is_none());
    }

    #[test]
    fn token_expiry_and_not_before() {
        let auth = Auth::new(config());
        let now = now_ms();

        let expired = auth
            .issue_at("admin", Role::Administrator, now - 3_600_000, 1_000)
            .token;
        assert!(auth.validate_token(&expired).is_none());

        // within the 5 minute clock skew
        let recently_expired = auth
            .issue_at("admin", Role::Administrator, now - 61_000, 1_000)
            .token;
        assert!(auth.validate_token(&recently_expired).is_some());

        let future = auth
            .issue_at("admin", Role::Administrator, now + 3_600_000, 3_600_000)
            .token;
        assert!(auth.validate_token(&future).is_none());

        let short = auth.issue_token_with_ttl("admin", Role::ReadOnly, Duration::from_secs(1));
        assert_eq!(short.expires - short.issued, 1);
    }

    #[test]
    fn api_key_cidr() {
        let auth = Auth::new(config());

        let p = auth
            .authenticate_api_key("local-key-0123456789", "127.0.0.1".parse().unwrap())
            .unwrap();
        assert_eq!(
            p,
            Principal {
                name: "local".into(),
                role: Role::ReadWrite,
                method: AuthMethod::ApiKey
            }
        );
        assert!(
            auth.authenticate_api_key("local-key-0123456789", "::ffff:127.0.0.1".parse().unwrap())
                .is_ok()
        );
        assert!(
            auth.authenticate_api_key("local-key-0123456789", "::1".parse().unwrap())
                .is_ok()
        );
        assert!(matches!(
            auth.authenticate_api_key("local-key-0123456789", "10.0.0.1".parse().unwrap()),
            Err(ApiKeyError::OutOfRange { .. })
        ));
        assert!(
            auth.authenticate_api_key("anywhere-key-0123456789", "8.8.8.8".parse().unwrap())
                .is_ok()
        );
        assert!(
            auth.authenticate_api_key("anywhere-key-0123456789", "2001:db8::1".parse().unwrap())
                .is_ok()
        );
        assert_eq!(
            auth.authenticate_api_key("nope", "127.0.0.1".parse().unwrap()),
            Err(ApiKeyError::Unknown)
        );
    }

    #[test]
    fn primary_api_key_parsing() {
        let plain = ApiKey::parse_primary("abcdefghijklmnop").unwrap().unwrap();
        assert_eq!(plain.role, Role::Administrator);
        assert_eq!(plain.cidr, ALLOW_ALL_CIDR);
        assert_eq!(plain.name, PRIMARY_API_KEY_NAME);

        let full = ApiKey::parse_primary("role=readonly;cidr=10.0.0.0/8,::1/128;abcdefghijklmnop")
            .unwrap()
            .unwrap();
        assert_eq!(full.key, "abcdefghijklmnop");
        assert_eq!(full.role, Role::ReadOnly);
        assert_eq!(full.cidr, "10.0.0.0/8,::1/128");

        assert!(
            ApiKey::parse_primary("role=root;abcdefghijklmnop")
                .unwrap()
                .is_err()
        );
        assert!(ApiKey::parse_primary("  ").is_none());
    }

    #[test]
    fn authenticate_schemes_and_roles() {
        let auth = Auth::new(config());
        let token = auth.issue_token("admin", Role::Administrator).token;
        let local = addr("127.0.0.1");

        let bearer = format!("Bearer {token}");
        let p = parts("/api/v0/session", &[("authorization", bearer.as_str())]);
        assert_eq!(
            auth.authenticate(&p, local).unwrap().method,
            AuthMethod::Jwt
        );
        assert!(
            auth.authenticate_with_policy(&p, local, Policy::ApiKeyOnly)
                .is_none()
        );

        let p = parts("/api/v0/session", &[("x-api-key", "local-key-0123456789")]);
        let principal = auth.authenticate(&p, local).unwrap();
        assert_eq!(principal.method, AuthMethod::ApiKey);
        assert!(
            auth.authenticate_with_policy(&p, local, Policy::JwtOnly)
                .is_none()
        );
        assert!(auth.authenticate(&p, addr("192.168.1.1")).is_none());

        assert_eq!(
            auth.authorize(&p, local, Role::ReadWrite, Policy::Any)
                .unwrap()
                .role,
            Role::ReadWrite
        );
        assert_eq!(
            auth.authorize(&p, local, Role::Administrator, Policy::Any),
            Err(AuthError::Forbidden)
        );

        let none = parts("/api/v0/session", &[]);
        assert_eq!(
            auth.authorize(&none, local, Role::ReadOnly, Policy::Any),
            Err(AuthError::Unauthorized)
        );

        // access_token only honoured for hubs
        let query = format!("/api/v0/session?access_token={token}");
        assert!(auth.authenticate(&parts(&query, &[]), local).is_none());
        let query = format!("/hub/application?id=x&access_token={token}");
        assert_eq!(
            auth.authenticate(&parts(&query, &[]), local).unwrap().role,
            Role::Administrator
        );

        // hubs accept API keys as bearer values
        let p = parts(
            "/hub/logs/negotiate",
            &[("authorization", "Bearer anywhere-key-0123456789")],
        );
        assert_eq!(
            auth.authenticate(&p, local).unwrap().method,
            AuthMethod::ApiKey
        );
        let p = parts(
            "/api/v0/session",
            &[("authorization", "Bearer anywhere-key-0123456789")],
        );
        assert!(auth.authenticate(&p, local).is_none());
    }

    #[test]
    fn passthrough_when_disabled() {
        let auth = Auth::new(AuthConfig {
            disabled: true,
            ..config()
        });
        assert!(!auth.enabled());
        let p = parts("/api/v0/transfers", &[]);
        let principal = auth
            .authorize(&p, addr("10.0.0.1"), Role::Administrator, Policy::JwtOnly)
            .unwrap();
        assert_eq!(principal, Principal::passthrough());
    }

    #[test]
    fn login_semantics() {
        let auth = Auth::new(config());
        assert!(auth.check_credentials("admin", "hunter2"));
        assert!(!auth.check_credentials("admin", "hunter3"));
        assert!(!auth.check_credentials("Admin", "hunter2"));

        let token = auth.login("admin", "hunter2").unwrap();
        assert_eq!(
            auth.validate_token(&token.token).unwrap().role,
            Role::Administrator
        );
        assert_eq!(auth.login("admin", "wrong"), Err(LoginError::Unauthorized));
        assert!(matches!(
            auth.login("", "x"),
            Err(LoginError::BadRequest(_))
        ));

        auth.update(&AuthConfig {
            password: "changed".into(),
            ..config()
        });
        assert!(auth.login("admin", "changed").is_ok());
    }

    #[test]
    fn roles() {
        assert_eq!(
            "administrator".parse::<Role>().unwrap(),
            Role::Administrator
        );
        assert_eq!("READWRITE".parse::<Role>().unwrap(), Role::ReadWrite);
        assert!("root".parse::<Role>().is_err());
        assert!(Role::Administrator.satisfies(Role::ReadWrite));
        assert!(!Role::ReadOnly.satisfies(Role::ReadWrite));
        assert_eq!(
            serde_json::to_string(&Role::ReadWrite).unwrap(),
            "\"ReadWrite\""
        );
        assert_eq!(role_from_u8(Role::Administrator as u8), Role::Administrator);
    }

    #[test]
    fn constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
