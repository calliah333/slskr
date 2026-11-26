//! First-run setup: creating the web login on a fresh install.
//!
//! Setup is *required* while web authentication is enabled and no web password exists in any
//! configuration layer (no hash in `settings.json`, no plaintext in `slskr.toml` or the
//! environment). While it is, [`gate`] answers every API route except the setup routes and
//! `GET /api/auth` with `503 {"error": "setup required"}`.
//!
//! Whenever setup becomes required (at startup, or when a configuration change removes the
//! password) a random one-time setup code is generated, kept in memory only, and written to
//! the log and stderr with the URL to open. Callers on the daemon's own machine (a loopback
//! peer address, a loopback `Host` and no forwarding headers) need no code; everyone else
//! sends it in the request body or the `X-Setup-Code` header. Wrong codes are delayed and
//! limited per address (IPv6 per /64).
//!
//! `POST /api/setup` writes the web credentials (and optionally Soulseek credentials and
//! shares) through the same settings store as `PATCH /api/settings`, then signs the caller in.
//! The Soulseek login test and the directory picker are open to setup callers while setup is
//! required, and to administrators afterwards.

use std::{
    collections::BTreeSet,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use slskr_types::{
    config::Config,
    setup::{
        Credentials, DirectoryEntry, DirectoryListing, LoginTestFailure, LoginTestResult,
        SETUP_CODE_HEADER, SETUP_REQUIRED, SetupRequest, SetupStatus,
    },
};
use soulseek::{ClientOptions, ClientState, DiagnosticLevel, SoulseekClient};

use crate::{
    app::AppState,
    auth::{self, AuthMethod, Policy, Principal, Role, limit::FailureLimiter},
    config::{self, store},
    core_api::settings_error,
    http::{ApiError, ApiResult, Json, Query, Routes, error_response, optional_json},
};

/// Characters of setup codes: no `0/O`, `1/I/L`, so codes can be read off a log.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// Groups of [`CODE_GROUP`] characters (12 characters, about 59 bits).
const CODE_GROUPS: usize = 3;
const CODE_GROUP: usize = 4;
/// Wrong codes allowed per address within [`FAILURE_WINDOW`].
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Delay before answering a wrong code.
const FAILURE_DELAY: Duration = Duration::from_millis(500);
/// Addresses tracked at once; when full of recent failures, new addresses are refused too.
const TRACKED_ADDRESSES: usize = 4096;
/// Longest Soulseek login test.
pub const LOGIN_TEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Soulseek usernames longer than this are refused by the server (`INVALIDUSERNAME`).
pub const SOULSEEK_USERNAME_LIMIT: usize = 30;
/// Shortest web password accepted by setup.
pub const MIN_PASSWORD_LENGTH: usize = 8;
/// Most subdirectories returned by `GET /api/fs/dirs`, and entries read to find them.
const DIRECTORY_LIMIT: usize = 1000;
const DIRECTORY_SCAN_LIMIT: usize = 20_000;
/// Request headers set by reverse proxies; their presence means the peer is not the caller.
const FORWARDING_HEADERS: [&str; 4] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
];
/// API paths that answer while setup is required (below `/api`, any method).
const OPEN_DURING_SETUP: [&str; 4] = [
    "/api/auth",
    "/api/setup",
    "/api/soulseek/test-login",
    "/api/fs/dirs",
];

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/setup", status)
        .post("/api/setup", complete)
        .post("/api/soulseek/test-login", test_login)
        .get("/api/fs/dirs", list_dirs)
}

/// The setup code and its attempt limits; cheap to clone.
#[derive(Clone)]
pub struct Setup(Arc<Inner>);

impl Default for Setup {
    fn default() -> Self {
        Self(Arc::new(Inner {
            code: Mutex::default(),
            failures: FailureLimiter::new(MAX_FAILURES, FAILURE_WINDOW, TRACKED_ADDRESSES),
            completing: tokio::sync::Mutex::default(),
            login_test: tokio::sync::Mutex::default(),
        }))
    }
}

struct Inner {
    /// The normalized code (no separators) while setup is required.
    code: Mutex<Option<String>>,
    failures: FailureLimiter,
    /// Serializes `POST /api/setup`.
    completing: tokio::sync::Mutex<()>,
    /// One Soulseek login test at a time.
    login_test: tokio::sync::Mutex<()>,
}

/// A refused setup code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeError {
    Missing,
    Wrong,
    /// Too many wrong codes from this address; retry after the duration.
    Limited(Duration),
}

impl Setup {
    pub fn new() -> Self {
        Self::default()
    }

    fn stored(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.0.code.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Creates a code when `required` and none exists, returning it (formatted) to announce;
    /// forgets the code and the failure counts when setup is not required.
    pub fn refresh(&self, required: bool) -> Option<String> {
        let mut stored = self.stored();
        if !required {
            *stored = None;
            self.0.failures.reset();
            return None;
        }
        if stored.is_some() {
            return None;
        }
        let code = generate_code();
        *stored = Some(normalize_code(&code));
        Some(code)
    }

    /// The current setup code, formatted as announced.
    pub fn code(&self) -> Option<String> {
        self.stored().as_deref().map(format_code)
    }

    /// Checks `candidate` for a caller at `ip`: refused while the address is over its limit,
    /// otherwise compared in constant time; a wrong code counts against the address.
    pub fn check(
        &self,
        ip: IpAddr,
        candidate: Option<&str>,
        now: Instant,
    ) -> Result<(), CodeError> {
        if let Some(wait) = self.0.failures.limited(ip, now) {
            return Err(CodeError::Limited(wait));
        }
        let Some(candidate) = candidate.map(normalize_code).filter(|c| !c.is_empty()) else {
            return Err(CodeError::Missing);
        };
        let matches = self
            .stored()
            .as_deref()
            .is_some_and(|code| auth::constant_time_eq(code.as_bytes(), candidate.as_bytes()));
        if matches {
            return Ok(());
        }
        self.0.failures.fail(ip, now).map_err(CodeError::Limited)?;
        Err(CodeError::Wrong)
    }
}

fn generate_code() -> String {
    // Rejection sampling keeps every character equally likely: bytes at or above the
    // largest multiple of the alphabet size are drawn again.
    let limit = 256 - 256 % CODE_ALPHABET.len();
    let mut raw = String::with_capacity(CODE_GROUPS * CODE_GROUP);
    while raw.len() < CODE_GROUPS * CODE_GROUP {
        let bytes: [u8; 32] = crate::random();
        raw.extend(
            bytes
                .iter()
                .filter(|&&byte| usize::from(byte) < limit)
                .map(|&byte| char::from(CODE_ALPHABET[usize::from(byte) % CODE_ALPHABET.len()]))
                .take(CODE_GROUPS * CODE_GROUP - raw.len()),
        );
    }
    format_code(&raw)
}

fn format_code(raw: &str) -> String {
    raw.as_bytes()
        .chunks(CODE_GROUP)
        .map(|group| String::from_utf8_lossy(group).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Uppercase without separators or whitespace, so `abcd efgh-jkmn` matches.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// True when setup is required: authentication is enabled and no web password is set.
pub fn required(app: &AppState) -> bool {
    app.auth.enabled() && password_missing(&app.options.get())
}

fn password_missing(config: &Config) -> bool {
    config.web.password.is_empty() && config.web.password_hash.is_empty()
}

/// Brings the setup code in line with the configuration: a new code is generated and
/// announced when setup becomes required, and forgotten when it no longer is. Returns
/// whether setup is required.
pub fn sync(app: &AppState) -> bool {
    let required = required(app);
    if let Some(code) = app.setup.refresh(required) {
        announce(&open_url(app.options.startup()), &code);
    }
    required
}

/// `http://<web.listen><url_base>/setup`, with `localhost` for a wildcard listen address.
fn open_url(config: &Config) -> String {
    let listen = config.web.listen;
    let host = if listen.ip().is_unspecified() {
        format!("localhost:{}", listen.port())
    } else {
        listen.to_string()
    };
    format!(
        "http://{host}{}/setup",
        config.web.url_base.trim_end_matches('/')
    )
}

fn announce(url: &str, code: &str) {
    tracing::warn!(
        "First-run setup required: no web password is set. Open {url} to create the web login. \
         Setup code (needed when not opened on this machine): {code}"
    );
    // Process logs go to the log buffer and files only; containers show stderr.
    eprintln!(
        "slskr: ================================================================\n\
         slskr: First-run setup: no web password is set.\n\
         slskr: Open {url} to create the web login.\n\
         slskr: Setup code (needed when not opened on this machine): {code}\n\
         slskr: ================================================================"
    );
}

/// Middleware answering `503 {"error": "setup required"}` for every API route but
/// `OPEN_DURING_SETUP` until setup is done. Paths are relative to `web.url_base`.
pub async fn gate(State(app): State<AppState>, request: Request, next: Next) -> Response {
    if required(&app) && !OPEN_DURING_SETUP.contains(&request.uri().path()) {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, SETUP_REQUIRED);
    }
    next.run(request).await
}

/// What setup needs to know about a request: its peer address and headers.
pub struct Caller {
    pub remote: SocketAddr,
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
}

impl Caller {
    fn from_parts(parts: &Parts) -> Self {
        Self {
            remote: auth::remote_addr(parts),
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
        }
    }

    /// True when the request comes from the daemon's own machine: a loopback peer, a
    /// loopback `Host` (so a DNS-rebound name does not count) and no reverse proxy headers.
    pub fn local(&self) -> bool {
        let host = auth::request_host(&self.headers, &self.uri);
        self.remote.ip().to_canonical().is_loopback()
            && !FORWARDING_HEADERS
                .iter()
                .any(|name| self.headers.contains_key(*name))
            && host.is_some_and(auth::loopback_host)
    }

    fn cross_site(&self) -> bool {
        auth::needs_origin_check(&self.method, &self.headers)
            && !auth::same_origin(&self.headers, &self.uri)
    }

    fn header_code(&self) -> Option<&str> {
        self.headers
            .get(SETUP_CODE_HEADER)
            .and_then(|value| value.to_str().ok())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(parts))
    }
}

/// A refused setup request; boxed because responses are large.
pub struct Refused(Box<Response>);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<ApiError> for Refused {
    fn from(error: ApiError) -> Self {
        Self(Box::new(error.into_response()))
    }
}

impl From<auth::AuthError> for Refused {
    fn from(error: auth::AuthError) -> Self {
        Self(Box::new(error.into_response()))
    }
}

fn cross_site_refused() -> Refused {
    ApiError::Forbidden("Cross-site request refused".into()).into()
}

/// Lets a setup caller through: same-origin, and local or with the right code.
async fn authorize_code(
    app: &AppState,
    caller: &Caller,
    code: Option<&str>,
) -> Result<(), Refused> {
    if caller.cross_site() {
        return Err(cross_site_refused());
    }
    if caller.local() {
        return Ok(());
    }
    let ip = caller.remote.ip().to_canonical();
    match app.setup.check(ip, code, Instant::now()) {
        Ok(()) => Ok(()),
        Err(CodeError::Missing) => Err(ApiError::Forbidden(
            "Setup code required; it is printed in the daemon's log".into(),
        )
        .into()),
        Err(CodeError::Wrong) => {
            tracing::warn!(%ip, "wrong first-run setup code");
            tokio::time::sleep(FAILURE_DELAY).await;
            Err(ApiError::Forbidden("Invalid setup code".into()).into())
        }
        Err(CodeError::Limited(retry)) => {
            let seconds = auth::limit::retry_seconds(retry);
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many wrong setup codes; try again in {seconds} s"),
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
            Err(Refused(Box::new(response)))
        }
    }
}

/// Access to the setup helpers (login test, directory picker): while setup is required, a
/// local caller or one sending the code in `X-Setup-Code`; afterwards an administrator
/// (session or API key).
pub struct SetupAccess;

impl FromRequestParts<AppState> for SetupAccess {
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Refused> {
        let caller = Caller::from_parts(parts);
        if sync(app) {
            authorize_code(app, &caller, caller.header_code()).await?;
        } else {
            app.auth
                .authorize(parts, caller.remote, Role::Administrator, Policy::Any)?;
        }
        Ok(Self)
    }
}

/// `GET /api/setup`: public.
async fn status(State(app): State<AppState>, caller: Caller) -> Json<SetupStatus> {
    let required = sync(&app);
    let pinned = app.options.pinned().await;
    let config = app.options.get();
    Json(SetupStatus {
        required,
        code_required: required && !caller.local(),
        web_username_pinned: pinned.contains("web.username"),
        web_password_pinned: pinned.contains("web.password"),
        soulseek_configured: crate::app::credentials(&config).is_some(),
        soulseek_pinned: pinned.contains("soulseek.username")
            || pinned.contains("soulseek.password"),
    })
}

/// `POST /api/setup`: saves the web login (and optionally Soulseek credentials and shares),
/// then signs the caller in (201 with the session cookie).
async fn complete(
    State(app): State<AppState>,
    caller: Caller,
    body: Bytes,
) -> Result<Response, Refused> {
    if caller.cross_site() {
        return Err(cross_site_refused());
    }
    let request = optional_json::<SetupRequest>(&caller.headers, &body)?
        .ok_or_else(|| ApiError::BadRequest("No setup request provided".into()))?;
    let _completing = app.setup.0.completing.lock().await;
    if !sync(&app) {
        return Err(ApiError::Conflict("Setup is already complete".into()).into());
    }
    let code = request.code.as_deref().or(caller.header_code());
    authorize_code(&app, &caller, code).await?;

    // A pinned username can be left blank: the pinned name is kept.
    let mut username = request.web.username.trim().to_owned();
    if username.is_empty() && app.options.pinned().await.contains("web.username") {
        username = app.options.get().web.username.clone();
    }
    let mut errors = web_errors(&username, &request.web.password);
    if let Some(soulseek) = &request.soulseek {
        errors.extend(soulseek_errors(soulseek));
    }
    let shares = request.shares.clone().unwrap_or_default();
    if !shares.is_empty() {
        let paths = shares.clone();
        errors.extend(
            tokio::task::spawn_blocking(move || share_errors(&paths))
                .await
                .map_err(|e| ApiError::Internal(anyhow::anyhow!("checking shares: {e}")))?,
        );
    }
    if !errors.is_empty() {
        return Err(ApiError::BadRequest(errors.join("; ")).into());
    }

    let patch = setup_patch(
        &app.options.pinned().await,
        &app.options.get(),
        &username,
        &request,
        &shares,
    );
    app.options
        .patch(Value::Object(patch))
        .await
        .map_err(settings_error)?;
    let config = app.options.get();
    app.auth.update(&config::auth_config(&config));
    sync(&app);

    let principal = Principal {
        name: config.web.username.clone(),
        role: Role::Administrator,
        method: AuthMethod::Session,
    };
    let token = app
        .auth
        .sessions()
        .create(&principal.name, principal.role)
        .await
        .map_err(ApiError::Internal)?;
    let cookie = app
        .auth
        .session_cookie(&token, auth::forwarded_https(&caller.headers));
    tracing::info!(username = %principal.name, "first-run setup completed");

    if !shares.is_empty() {
        rescan(&app);
    }
    if request.soulseek.is_some()
        && config.soulseek.connect
        && let Err(error) = app.connect().await
    {
        tracing::warn!(%error, "could not connect to Soulseek after setup");
    }
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(principal.session()),
    )
        .into_response())
}

/// The settings patch for a setup request. Pinned keys whose value would not change are left
/// out; any other pinned key stays in, so the settings store refuses it (409).
fn setup_patch(
    pinned: &BTreeSet<String>,
    current: &Config,
    username: &str,
    request: &SetupRequest,
    shares: &[String],
) -> Map<String, Value> {
    let mut patch = Map::new();
    let mut set = |key: &str, value: Value, current: Value| {
        if pinned.contains(key) && value == current {
            return;
        }
        if let Some(path) = store::split_key(key) {
            store::insert(&mut patch, &path, value);
        }
    };
    set(
        "web.username",
        username.into(),
        current.web.username.as_str().into(),
    );
    set(
        "web.password",
        request.web.password.as_str().into(),
        current.web.password.as_str().into(),
    );
    if let Some(soulseek) = &request.soulseek {
        set(
            "soulseek.username",
            soulseek.username.as_str().into(),
            current.soulseek.username.as_str().into(),
        );
        set(
            "soulseek.password",
            soulseek.password.as_str().into(),
            current.soulseek.password.as_str().into(),
        );
    }
    if !shares.is_empty() {
        let mut directories = current.shares.directories.clone();
        for share in shares {
            if !directories.contains(share) {
                directories.push(share.clone());
            }
        }
        set(
            "shares.directories",
            directories.into(),
            current.shares.directories.clone().into(),
        );
    }
    patch
}

/// Starts a share scan once the running one (if any) has finished.
fn rescan(app: &AppState) {
    let app = app.clone();
    tokio::spawn(async move {
        while !app.shutdown.is_cancelled() {
            if app
                .services
                .shares
                .launch_scan(app.shutdown.child_token())
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

fn web_errors(username: &str, password: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if username.is_empty() {
        errors.push("web.username may not be blank".to_owned());
    }
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        errors.push(format!(
            "web.password must be at least {MIN_PASSWORD_LENGTH} characters"
        ));
    }
    errors
}

/// Why the Soulseek server would refuse `username`, if it would: blank, surrounded by spaces,
/// or longer than [`SOULSEEK_USERNAME_LIMIT`] characters.
pub fn soulseek_username_error(username: &str) -> Option<String> {
    if username.trim().is_empty() {
        Some("soulseek.username may not be blank".into())
    } else if username.trim() != username {
        Some("soulseek.username may not start or end with a space".into())
    } else if username.chars().count() > SOULSEEK_USERNAME_LIMIT {
        Some(format!(
            "soulseek.username may not be longer than {SOULSEEK_USERNAME_LIMIT} characters"
        ))
    } else if username.chars().any(char::is_control) {
        Some("soulseek.username may not contain control characters".into())
    } else {
        None
    }
}

fn soulseek_errors(credentials: &Credentials) -> Vec<String> {
    let mut errors: Vec<String> = soulseek_username_error(&credentials.username)
        .into_iter()
        .collect();
    if credentials.password.is_empty() {
        errors.push("soulseek.password may not be blank".into());
    }
    errors
}

/// One message per share path that is not an existing, readable, absolute directory.
fn share_errors(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| {
            let local = Path::new(path);
            let problem = if path.trim().is_empty() {
                return Some("shares: a path is blank".to_owned());
            } else if !local.is_absolute() {
                "is not an absolute path".to_owned()
            } else {
                match std::fs::metadata(local) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        "does not exist".to_owned()
                    }
                    Err(error) => format!("can not be read: {error}"),
                    Ok(meta) if !meta.is_dir() => "is not a directory".to_owned(),
                    Ok(_) => match std::fs::read_dir(local) {
                        Ok(_) => return None,
                        Err(error) => format!("can not be read: {error}"),
                    },
                }
            };
            Some(format!("shares: {path} {problem}"))
        })
        .collect()
}

/// `POST /api/soulseek/test-login`: logs in with the given credentials on a separate,
/// short-lived connection and reports the outcome.
async fn test_login(
    State(app): State<AppState>,
    _: SetupAccess,
    Json(request): Json<Credentials>,
) -> ApiResult<Json<LoginTestResult>> {
    let config = app.options.get();
    if !config.soulseek.connect {
        return Err(ApiError::Conflict(
            "soulseek.connect is false: slskr does not contact the Soulseek server".into(),
        ));
    }
    let errors = soulseek_errors(&request);
    if !errors.is_empty() {
        return Err(ApiError::BadRequest(errors.join("; ")));
    }
    if app.client.state() != ClientState::DISCONNECTED
        && app.client.username().as_deref() == Some(request.username.as_str())
    {
        return Err(ApiError::Conflict(format!(
            "Already connected as {}; a test login would disconnect that session",
            request.username
        )));
    }
    let Ok(_testing) = app.setup.0.login_test.try_lock() else {
        return Err(ApiError::Status(
            StatusCode::TOO_MANY_REQUESTS,
            "A Soulseek login test is already running".into(),
        ));
    };
    let (host, port) = config::server_endpoint(&config.soulseek.server)?;
    let options = login_test_options(config::client_options(&config)?);
    tracing::info!(username = %request.username, server = %config.soulseek.server, "testing a Soulseek login");
    let result = login_test(
        options,
        &host,
        port,
        &request.username,
        &request.password,
        LOGIN_TEST_TIMEOUT,
    )
    .await;
    tracing::info!(
        username = %request.username,
        ok = result.ok,
        reason = ?result.reason,
        "Soulseek login test finished"
    );
    Ok(Json(result))
}

/// Client options for a login test: no listener, no distributed network, no diagnostics.
fn login_test_options(options: ClientOptions) -> ClientOptions {
    ClientOptions {
        enable_listener: false,
        listen_port: None,
        enable_distributed_network: false,
        accept_distributed_children: false,
        distributed_child_limit: 0,
        minimum_diagnostic_level: DiagnosticLevel::None,
        ..options
    }
}

/// Logs in to `host:port` with a client of its own and disconnects again.
pub(crate) async fn login_test(
    options: ClientOptions,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    limit: Duration,
) -> LoginTestResult {
    let client = SoulseekClient::new(options, Arc::new(soulseek::resolvers::DefaultResolvers));
    let result = tokio::time::timeout(limit, client.connect(host, port, username, password)).await;
    client.disconnect("Login test finished", None).await;
    match result {
        Ok(Ok(())) => LoginTestResult {
            ok: true,
            reason: None,
            message: None,
        },
        Ok(Err(error)) => login_failure(&error),
        Err(_) => LoginTestResult {
            ok: false,
            reason: Some(LoginTestFailure::Timeout),
            message: Some(format!(
                "The Soulseek server did not answer within {} s",
                limit.as_secs()
            )),
        },
    }
}

/// Maps a failed login to a [`LoginTestFailure`]; the server's rejection reasons are
/// `INVALIDPASS`, `INVALIDUSERNAME`, `SVRFULL`, `SVRPRIVATE` and `INVALIDVERSION`.
fn login_failure(error: &soulseek::Error) -> LoginTestResult {
    use soulseek::Error as E;
    let (reason, message) = match error {
        E::LoginRejected(message) => {
            let message = message
                .strip_prefix("The server rejected login attempt: ")
                .unwrap_or(message);
            let upper = message.to_ascii_uppercase();
            let reason = if upper.contains("INVALIDPASS") || upper.contains("EMPTYPASSWORD") {
                LoginTestFailure::InvalidPassword
            } else if upper.contains("INVALIDUSERNAME") {
                LoginTestFailure::InvalidUsername
            } else if upper.contains("SVRFULL") {
                LoginTestFailure::ServerFull
            } else if upper.contains("SVRPRIVATE") {
                LoginTestFailure::ServerPrivate
            } else if upper.contains("INVALIDVERSION") {
                LoginTestFailure::InvalidVersion
            } else {
                LoginTestFailure::Rejected
            };
            (reason, message.to_owned())
        }
        E::KickedFromServer => (LoginTestFailure::Rejected, error.to_string()),
        E::Timeout(message) => (LoginTestFailure::Timeout, message.clone()),
        // Resolution, connection and read failures (`Failed to connect: ...`).
        E::Address(message) | E::Connection(message) | E::Other(message) => {
            (LoginTestFailure::Unreachable, message.clone())
        }
        E::Io(_) => (LoginTestFailure::Unreachable, error.to_string()),
        other => (LoginTestFailure::Error, other.to_string()),
    };
    LoginTestResult {
        ok: false,
        reason: Some(reason),
        message: Some(message),
    }
}

#[derive(Deserialize)]
struct DirsQuery {
    #[serde(default)]
    path: Option<String>,
}

/// `GET /api/fs/dirs?path=`: the subdirectories of a local directory, for choosing shares.
async fn list_dirs(
    _: SetupAccess,
    Query(query): Query<DirsQuery>,
) -> ApiResult<Json<DirectoryListing>> {
    let path = query
        .path
        .filter(|path| !path.is_empty())
        .map_or_else(default_directory, PathBuf::from);
    tokio::task::spawn_blocking(move || list_directory(&path, DIRECTORY_LIMIT))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("listing directory: {e}")))?
        .map(Json)
}

/// `$HOME` when it is an absolute directory, otherwise the filesystem root.
fn default_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Lists the directories in `path` (following symlinks as `stat` does), sorted by name, at
/// most `limit`; entries that can not be read are skipped.
fn list_directory(path: &Path, limit: usize) -> ApiResult<DirectoryListing> {
    if !path.is_absolute() {
        return Err(ApiError::BadRequest("path must be absolute".into()));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(ApiError::BadRequest("path may not contain ..".into()));
    }
    // Drops `.` components and trailing separators.
    let path: PathBuf = path.components().collect();
    let display = path
        .to_str()
        .ok_or_else(|| ApiError::BadRequest("path is not valid UTF-8".into()))?
        .to_owned();
    let entries = std::fs::read_dir(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound(format!("{display} does not exist")),
        std::io::ErrorKind::PermissionDenied => {
            ApiError::Forbidden(format!("{display} can not be read"))
        }
        std::io::ErrorKind::NotADirectory => {
            ApiError::BadRequest(format!("{display} is not a directory"))
        }
        _ => ApiError::BadRequest(format!("{display}: {error}")),
    })?;
    let mut dirs = Vec::new();
    let mut truncated = false;
    for (seen, entry) in entries.enumerate() {
        if seen >= DIRECTORY_SCAN_LIMIT {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let child = entry.path();
        if !std::fs::metadata(&child).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let Some(child) = child.to_str() else {
            continue;
        };
        dirs.push(DirectoryEntry {
            name,
            path: child.to_owned(),
        });
    }
    dirs.sort_by_cached_key(|dir| (dir.name.to_lowercase(), dir.name.clone()));
    if dirs.len() > limit {
        dirs.truncate(limit);
        truncated = true;
    }
    Ok(DirectoryListing {
        parent: path.parent().and_then(Path::to_str).map(str::to_owned),
        path: display,
        dirs,
        truncated,
    })
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
