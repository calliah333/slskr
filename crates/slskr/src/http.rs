//! API errors and extractors, the API route builder, and safe SPA asset serving.
//!
//! Every API error is `{"error": "<message>"}` with a 4xx/5xx status: [`ApiError`] for
//! handlers, [`Json`], [`Path`] and [`Query`] for request rejections, and the [`json_errors`]
//! middleware for anything else (unknown routes, unsupported methods, body limits).

use std::{
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use crate::{app::AppState, auth::Auth};
use axum::{
    Router,
    body::Body,
    extract::{
        FromRef, FromRequest, FromRequestParts, Path as AxumPath, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    handler::Handler,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{MethodRouter, get},
};
use serde::{Serialize, de::DeserializeOwned};
use slskr_types::error::ErrorBody;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
    #[error("{1}")]
    Status(StatusCode, String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// 404 with the generic message.
    pub fn not_found() -> Self {
        Self::NotFound(String::new())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Status(status, _) => *status,
        }
    }
}

impl From<soulseek::Error> for ApiError {
    fn from(error: soulseek::Error) -> Self {
        use soulseek::Error as E;
        match error {
            E::Argument(message) => Self::BadRequest(message),
            E::UserOffline(message) | E::UserNotFound(message) | E::TransferNotFound(message) => {
                Self::NotFound(message)
            }
            E::Timeout(message) | E::NoResponse(message) => {
                Self::Status(StatusCode::GATEWAY_TIMEOUT, message)
            }
            E::RoomJoinForbidden(message) => Self::Forbidden(message),
            E::DuplicateTransfer(message) | E::DuplicateToken(message) => Self::Conflict(message),
            other => Self::Internal(anyhow::Error::new(other)),
        }
    }
}

/// The JSON error response for `status`; an empty `message` becomes the status's reason.
pub fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let mut error = message.into();
    if error.is_empty() {
        error = status
            .canonical_reason()
            .unwrap_or("Request failed")
            .to_owned();
    }
    (status, axum::Json(ErrorBody { error })).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        match self {
            Self::Internal(error) => {
                tracing::error!(error = %error, "unhandled API error");
                error_response(status, "")
            }
            Self::Unauthorized => error_response(status, ""),
            Self::BadRequest(message)
            | Self::Forbidden(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Status(_, message) => error_response(status, message),
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        // 400 malformed JSON, 415 missing JSON content type, 422 wrong shape, 413 too large.
        Self::Status(rejection.status(), rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        Self::Status(rejection.status(), rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::Status(rejection.status(), rejection.body_text())
    }
}

/// `axum::Json` whose rejections are [`ApiError`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Json<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let axum::Json(value) = axum::Json::<T>::from_request(request, state).await?;
        Ok(Self(value))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Path` whose rejections are [`ApiError`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Path<T> {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let AxumPath(value) = AxumPath::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// `axum::extract::Query` whose rejections are [`ApiError`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let axum::extract::Query(value) =
            axum::extract::Query::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// A JSON body that may be empty: `None` without a body, otherwise like [`Json`].
pub fn optional_json<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: &[u8],
) -> ApiResult<Option<T>> {
    if body.is_empty() {
        return Ok(None);
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim();
    let json = media.eq_ignore_ascii_case("application/json")
        || media.split_once('/').is_some_and(|(kind, subtype)| {
            kind.eq_ignore_ascii_case("application")
                && subtype.len() >= 5
                && subtype.is_char_boundary(subtype.len() - 5)
                && subtype[subtype.len() - 5..].eq_ignore_ascii_case("+json")
        });
    if !json {
        return Err(ApiError::Status(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`".into(),
        ));
    }
    serde_json::from_slice(body).map(Some).map_err(|error| {
        let status = if error.is_data() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError::Status(status, format!("Invalid JSON body: {error}"))
    })
}

/// Rewrites every API error response that is not JSON yet (unknown routes, unsupported
/// methods, body limits, upgrade failures) into `{"error": ...}`, keeping its status and
/// headers. A short plain-text body becomes the message.
pub async fn json_errors(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let message = axum::body::to_bytes(body, 1024)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .map(|text| text.trim().to_owned())
        .unwrap_or_default();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_TYPE);
    let rewritten = error_response(status, message);
    let (rewritten, body) = rewritten.into_parts();
    parts.headers.extend(rewritten.headers);
    Response::from_parts(parts, body)
}

/// One registered API route: method and path below `/api`.
pub type RouteEntry = (Method, String);

/// Builds the API router and records every route, so the documented route table
/// (`docs/API.md`) can be checked against what is actually served.
#[must_use]
pub struct Routes {
    router: Router<AppState>,
    table: Vec<RouteEntry>,
}

impl Default for Routes {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! route_method {
    ($($name:ident => $method:ident),* $(,)?) => {
        $(
            #[doc = concat!("Routes `", stringify!($method), " path` to `handler`.")]
            pub fn $name<H, T>(self, path: &str, handler: H) -> Self
            where
                H: Handler<T, AppState>,
                T: 'static,
            {
                self.add(Method::$method, path, axum::routing::$name(handler))
            }
        )*
    };
}

impl Routes {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            table: Vec::new(),
        }
    }

    route_method!(get => GET, post => POST, put => PUT, patch => PATCH, delete => DELETE);

    fn add(mut self, method: Method, path: &str, handler: MethodRouter<AppState>) -> Self {
        self.router = self.router.route(path, handler);
        self.table.push((method, path.to_owned()));
        self
    }

    pub fn merge(mut self, other: Routes) -> Self {
        self.router = self.router.merge(other.router);
        self.table.extend(other.table);
        self
    }

    /// Every registered `(method, path)`, paths relative to `/api`.
    pub fn table(&self) -> &[RouteEntry] {
        &self.table
    }

    pub fn into_router(self) -> Router<AppState> {
        self.router
    }
}

impl FromRef<AppState> for Auth {
    fn from_ref(state: &AppState) -> Self {
        state.core.auth.clone()
    }
}

/// Build the static-file/SPA router. Mount separately from `/api` so missing
/// frontend assets can never turn API errors into `index.html` responses.
#[derive(Clone)]
struct StaticFiles {
    root: Arc<PathBuf>,
    url_base: String,
}
pub fn static_router(content_path: impl Into<PathBuf>, url_base: &str) -> Router {
    let base = normalize_base(url_base);
    let files = StaticFiles {
        root: Arc::new(content_path.into()),
        url_base: base.clone(),
    };
    // Register prefixed routes directly: a nested `/{*path}` would not match `{base}/`.
    let router = Router::new().route(&format!("{base}/"), get(serve_index));
    let router = if base.is_empty() {
        router
    } else {
        router.route(&base, get(serve_index))
    };
    router
        .route(&format!("{base}/{{*path}}"), get(serve_path))
        .with_state(files)
}

fn normalize_base(base: &str) -> String {
    let base = base.trim_matches('/');
    if base.is_empty() {
        String::new()
    } else {
        format!("/{base}")
    }
}

async fn serve_index(State(files): State<StaticFiles>) -> Response {
    serve_file(&files, "index.html", true).await
}

async fn serve_path(
    State(files): State<StaticFiles>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    if matches!(path.split('/').next(), Some("api")) {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_file(&files, &path, FsPath::new(&path).extension().is_none()).await
}

async fn serve_file(files: &StaticFiles, relative: &str, spa_fallback: bool) -> Response {
    let candidate = FsPath::new(relative);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let canonical_root = match tokio::fs::canonicalize(files.root.as_path()).await {
        Ok(path) => path,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let requested = canonical_root.join(candidate);
    let resolved = match tokio::fs::canonicalize(&requested).await {
        Ok(path) if path.starts_with(&canonical_root) && path.is_file() => path,
        _ if spa_fallback => match tokio::fs::canonicalize(canonical_root.join("index.html")).await
        {
            Ok(path) if path.starts_with(&canonical_root) && path.is_file() => path,
            _ => return StatusCode::NOT_FOUND.into_response(),
        },
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = match resolved
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    };
    let mut bytes = match tokio::fs::read(resolved).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if content_type.starts_with("text/html") {
        if let Ok(html) = String::from_utf8(bytes) {
            let base = &files.url_base;
            let href = base
                .replace('&', "&amp;")
                .replace('"', "&quot;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let rewritten =
                html.replace("<base href=\"/\" />", &format!("<base href=\"{href}/\" />"));
            let base_json = serde_json::to_string(base)
                .expect("string serialization")
                .replace('<', "\\u003c");
            let rewritten = rewritten.replace(
                "</head>",
                &format!("<script>window.urlBase={base_json};</script></head>"),
            );
            bytes = rewritten.into_bytes();
        } else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
