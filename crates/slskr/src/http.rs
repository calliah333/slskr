//! HTTP errors, shared router helpers and safe SPA asset serving.

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use crate::{app::AppState, auth::Auth};
use axum::{
    Json, Router,
    body::Body,
    extract::{FromRef, Path as AxumPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, Json(message)).into_response(),
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::Forbidden(message) => (StatusCode::FORBIDDEN, Json(message)).into_response(),
            Self::NotFound(message) if message.is_empty() => StatusCode::NOT_FOUND.into_response(),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, Json(message)).into_response(),
            Self::Conflict(message) => (StatusCode::CONFLICT, Json(message)).into_response(),
            Self::Internal(error) => {
                tracing::error!(error = %error, "unhandled API error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json("Internal Server Error"),
                )
                    .into_response()
            }
            Self::Status(status, message) => (status, Json(message)).into_response(),
        }
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
    if matches!(path.split('/').next(), Some("api" | "hub")) {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_file(&files, &path, Path::new(&path).extension().is_none()).await
}

async fn serve_file(files: &StaticFiles, relative: &str, spa_fallback: bool) -> Response {
    let candidate = Path::new(relative);
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
