//! Serves the web UI: the copy compiled into the binary (see `build.rs`), or a directory
//! given with `--web-dir` while working on the UI.
//!
//! Hashed asset names are cached for a year; `index.html` and other unhashed files are
//! revalidated with their ETag. Embedded files carry brotli and gzip variants chosen by
//! `Accept-Encoding`. Paths without an extension fall back to `index.html` so client-side
//! routes deep-link, except under `api/`, which never does.

mod meta;

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
};

use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;

/// One file compiled into the binary.
struct Embedded {
    path: &'static str,
    content_type: &'static str,
    etag: &'static str,
    immutable: bool,
    raw: &'static [u8],
    br: Option<&'static [u8]>,
    gzip: Option<&'static [u8]>,
}

mod generated {
    use super::Embedded;
    include!(concat!(env!("OUT_DIR"), "/web_ui.rs"));
}

/// Whether the web UI was compiled into this binary. Without it the binary serves a page
/// explaining how to build the UI, unless `--web-dir` points at one.
pub const EMBEDDED: bool = generated::BUILT;

static FILES: LazyLock<HashMap<&'static str, &'static Embedded>> = LazyLock::new(|| {
    generated::FILES
        .iter()
        .map(|file| (file.path, file))
        .collect()
});

/// A file ready to send.
#[derive(Clone)]
struct Asset {
    content_type: &'static str,
    /// Strong ETag of the uncompressed body, quotes included.
    etag: String,
    immutable: bool,
    body: Bytes,
    br: Option<Bytes>,
    gzip: Option<Bytes>,
}

impl Asset {
    fn embedded(file: &'static Embedded) -> Self {
        Self {
            content_type: file.content_type,
            etag: file.etag.to_owned(),
            immutable: file.immutable,
            body: Bytes::from_static(file.raw),
            br: file.br.map(Bytes::from_static),
            gzip: file.gzip.map(Bytes::from_static),
        }
    }

    fn from_bytes(path: &str, body: Bytes) -> Self {
        Self {
            content_type: meta::content_type(path),
            etag: etag(&body),
            immutable: meta::is_hashed(path),
            body,
            br: None,
            gzip: None,
        }
    }
}

#[derive(Clone)]
struct WebUi(Arc<Inner>);

struct Inner {
    url_base: String,
    source: Source,
}

enum Source {
    /// The embedded UI, with `index.html` rewritten for the URL base once.
    Embedded {
        index: Asset,
    },
    Directory(PathBuf),
}

/// Router for the UI under `url_base`. Mount it beside the API, never in front of it, so
/// a missing route under `api/` can not turn into `index.html`.
pub fn router(web_dir: Option<PathBuf>, url_base: &str) -> Router {
    let base = normalize_base(url_base);
    let source = match web_dir {
        Some(dir) => Source::Directory(dir),
        None => {
            let file = FILES
                .get("index.html")
                .expect("index.html is always embedded");
            let html = std::str::from_utf8(file.raw).expect("index.html is UTF-8");
            let body = Bytes::from(rewrite_index(html, &base));
            Source::Embedded {
                index: Asset::from_bytes("index.html", body),
            }
        }
    };
    let ui = WebUi(Arc::new(Inner {
        url_base: base.clone(),
        source,
    }));
    // Register prefixed routes directly: a nested `/{*path}` would not match `{base}/`.
    let mut router = Router::new().route(&format!("{base}/"), get(serve_index));
    if !base.is_empty() {
        router = router.route(&base, get(serve_index));
    }
    router
        .route(&format!("{base}/{{*path}}"), get(serve_path))
        .with_state(ui)
}

fn normalize_base(base: &str) -> String {
    let base = base.trim_matches('/');
    if base.is_empty() {
        String::new()
    } else {
        format!("/{base}")
    }
}

async fn serve_index(State(ui): State<WebUi>, headers: HeaderMap) -> Response {
    match ui.index().await {
        Some(asset) => respond(&asset, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_path(
    State(ui): State<WebUi>,
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if path.split('/').next() == Some("api") || !is_plain_relative(&path) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let asset = match ui.file(&path).await {
        Some(asset) => Some(asset),
        // Client-side routes have no extension; missing assets do.
        None if Path::new(&path).extension().is_none() => ui.index().await,
        None => None,
    };
    match asset {
        Some(asset) => respond(&asset, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

impl WebUi {
    async fn index(&self) -> Option<Asset> {
        match &self.0.source {
            Source::Embedded { index } => Some(index.clone()),
            Source::Directory(root) => {
                let bytes = read_under(root, "index.html").await?;
                let html = String::from_utf8(bytes).ok()?;
                let body = Bytes::from(rewrite_index(&html, &self.0.url_base));
                Some(Asset::from_bytes("index.html", body))
            }
        }
    }

    async fn file(&self, path: &str) -> Option<Asset> {
        if path == "index.html" {
            return self.index().await;
        }
        match &self.0.source {
            Source::Embedded { .. } => FILES.get(path).map(|file| Asset::embedded(file)),
            Source::Directory(root) => {
                let bytes = read_under(root, path).await?;
                Some(Asset::from_bytes(path, bytes.into()))
            }
        }
    }
}

/// Reads `relative` below `root`, refusing anything that resolves outside it.
async fn read_under(root: &Path, relative: &str) -> Option<Vec<u8>> {
    if !is_plain_relative(relative) {
        return None;
    }
    let root = tokio::fs::canonicalize(root).await.ok()?;
    let resolved = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
    if !resolved.starts_with(&root) || !resolved.is_file() {
        return None;
    }
    tokio::fs::read(resolved).await.ok()
}

/// Whether `path` is relative and made of plain names only (no `..`, `.` or root).
fn is_plain_relative(path: &str) -> bool {
    Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

/// Points `<base href>` at the URL base and tells the UI where the API lives.
fn rewrite_index(html: &str, base: &str) -> String {
    let href = base
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = html.replace("<base href=\"/\" />", &format!("<base href=\"{href}/\" />"));
    let base_json = serde_json::to_string(base)
        .expect("string serialization")
        .replace('<', "\\u003c");
    html.replace(
        "</head>",
        &format!("<script>window.urlBase={base_json};</script></head>"),
    )
}

fn etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

fn respond(asset: &Asset, request: &HeaderMap) -> Response {
    let (encoding, body) = if let Some(br) = asset.br.as_ref().filter(|_| accepts(request, "br")) {
        (Some("br"), br)
    } else if let Some(gzip) = asset.gzip.as_ref().filter(|_| accepts(request, "gzip")) {
        (Some("gzip"), gzip)
    } else {
        (None, &asset.body)
    };
    // Each representation gets its own strong ETag.
    let etag = match encoding {
        Some(encoding) => format!("{}-{encoding}\"", asset.etag.trim_end_matches('"')),
        None => asset.etag.clone(),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(meta::cache_control(asset.immutable)),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    if asset.br.is_some() || asset.gzip.is_some() {
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    if if_none_match(request, &etag) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    if let Some(encoding) = encoding {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    }
    (headers, Body::from(body.clone())).into_response()
}

/// Whether `Accept-Encoding` allows `coding` (listed without `q=0`).
fn accepts(headers: &HeaderMap, coding: &str) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| {
            let mut parts = item.split(';');
            let name = parts.next().unwrap_or_default().trim();
            name.eq_ignore_ascii_case(coding)
                && parts.all(|param| {
                    let param = param.trim();
                    param
                        .strip_prefix("q=")
                        .is_none_or(|q| q.trim().parse::<f32>().is_ok_and(|q| q > 0.0))
                })
        })
}

/// Whether `If-None-Match` matches `etag` (weak comparison, as for GET).
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
}

#[cfg(test)]
mod tests;
