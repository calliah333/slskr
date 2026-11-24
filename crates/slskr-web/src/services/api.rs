//! HTTP API client.
//!
//! Paths are relative to [`api_base_url`], e.g. `api::get::<State>("/application")`.
//! Requests with a body carry `Content-Type: application/json`. Same-origin requests carry
//! the `HttpOnly` session cookie; the UI never sees the token. A 401 from any route other than
//! `/session`, `/server` and `/application` reloads the page, which shows the sign-in form;
//! so does a `503 {"error": "setup required"}`, which then shows the setup wizard. While
//! the wizard holds a setup code ([`set_setup_code`]), every request carries it in the
//! `X-Setup-Code` header.

use std::{cell::RefCell, fmt};

use gloo_net::http::{Method, RequestBuilder, Response};
use serde::{Serialize, de::DeserializeOwned};
use web_sys::RequestCredentials;

use crate::config::api_base_url;

/// Routes whose 401 responses do not trigger a logout.
const UNAUTHORIZED_EXEMPT: [&str; 3] = ["/session", "/server", "/application"];

thread_local! {
    static SETUP_CODE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Sends `code` as the first-run setup code with every request (`None` stops).
pub fn set_setup_code(code: Option<String>) {
    SETUP_CODE.with(|cell| *cell.borrow_mut() = code.filter(|code| !code.trim().is_empty()));
}

/// A failed request: HTTP status (0 for network/serialization failures) and the error message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// The `error` of the JSON error body, or a generic description.
    pub message: String,
    /// Seconds from the `Retry-After` header (429).
    pub retry_after: Option<u32>,
}

impl ApiError {
    fn local(message: impl fmt::Display) -> Self {
        Self {
            status: 0,
            message: message.to_string(),
            retry_after: None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

fn to_body<B: Serialize + ?Sized>(body: &B) -> Result<String, ApiError> {
    serde_json::to_string(body).map_err(ApiError::local)
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T, ApiError> {
    // Empty bodies (204 etc.) deserialize like JSON `null`, so `()` and `Option<_>` work.
    let text = if text.trim().is_empty() { "null" } else { text };
    serde_json::from_str(text)
        .map_err(|error| ApiError::local(format!("Invalid response: {error}")))
}

fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

/// Sends a request and returns the response body text of a 2xx response.
pub async fn request_text(
    method: Method,
    path: &str,
    body: Option<String>,
) -> Result<String, ApiError> {
    response_text(send(method, path, body, None).await?, path).await
}

async fn send(
    method: Method,
    path: &str,
    body: Option<String>,
    etag: Option<&str>,
) -> Result<Response, ApiError> {
    let url = format!("{}{}", api_base_url(), path);
    let mut builder = RequestBuilder::new(&url)
        .method(method)
        .credentials(RequestCredentials::SameOrigin);
    if let Some(code) = SETUP_CODE.with(|cell| cell.borrow().clone()) {
        builder = builder.header(slskr_types::setup::SETUP_CODE_HEADER, &code);
    }
    if let Some(etag) = etag {
        builder = builder.header("If-None-Match", etag);
    }

    let Some(body) = body else {
        return builder.send().await.map_err(ApiError::local);
    };
    builder = builder.header("Content-Type", "application/json");
    // Bulk downloads repeat path prefixes and can contain thousands of filenames.
    let request = if path == "/transfers/downloads"
        && body.len() >= 16 * 1024
        && let Some(compressed) = gzip_body(&body).await
    {
        builder.header("Content-Encoding", "gzip").body(compressed)
    } else {
        builder.body(body)
    };
    request
        .map_err(ApiError::local)?
        .send()
        .await
        .map_err(ApiError::local)
}

async fn gzip_body(body: &str) -> Option<web_sys::Blob> {
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Blob, ReadableWritablePair};

    let constructor =
        js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("CompressionStream"))
            .ok()?
            .dyn_into::<js_sys::Function>()
            .ok()?;
    let compressor = js_sys::Reflect::construct(
        &constructor,
        &js_sys::Array::of1(&JsValue::from_str("gzip")),
    )
    .ok()?
    .unchecked_into::<ReadableWritablePair>();
    let parts = js_sys::Array::of1(&JsValue::from_str(body));
    let input = Blob::new_with_str_sequence(&parts).ok()?;
    let stream = input.stream().pipe_through(&compressor);
    // Buffer the compressed result so fetch sends an ordinary body with a known length.
    let response = web_sys::Response::new_with_opt_readable_stream(Some(&stream)).ok()?;
    let compressed: Blob = JsFuture::from(response.blob().ok()?)
        .await
        .ok()?
        .dyn_into()
        .ok()?;
    (compressed.size() < input.size()).then_some(compressed)
}

async fn response_text(response: Response, path: &str) -> Result<String, ApiError> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .and_then(|value| value.trim().parse().ok());
    let text = response.text().await.unwrap_or_default();

    if response.ok() {
        return Ok(text);
    }

    if status == 401 && !UNAUTHORIZED_EXEMPT.contains(&path) {
        reload();
    }

    let message = serde_json::from_str::<slskr_types::error::ErrorBody>(&text)
        .map(|body| body.error)
        .ok()
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| format!("Request failed with status code {status}"));
    // The daemon lost its web password (first-run setup): the reloaded page shows the
    // setup notice (`/auth` is never gated).
    if status == 503 && message == slskr_types::setup::SETUP_REQUIRED {
        reload();
    }
    Err(ApiError {
        status,
        message,
        retry_after,
    })
}

/// Sends a request and deserializes the JSON response.
pub async fn request<T: DeserializeOwned>(
    method: Method,
    path: &str,
    body: Option<String>,
) -> Result<T, ApiError> {
    parse(&request_text(method, path, body).await?)
}

/// `GET path`, JSON response.
pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    request(Method::GET, path, None).await
}

/// Conditional GET. A 304 leaves the caller's current snapshot and validator intact.
pub async fn get_if_changed<T: DeserializeOwned>(
    path: &str,
    etag: Option<&str>,
) -> Result<Option<(T, Option<String>)>, ApiError> {
    let response = send(Method::GET, path, None, etag).await?;
    if response.status() == 304 {
        return Ok(None);
    }
    let etag = response.headers().get("ETag");
    let data = parse(&response_text(response, path).await?)?;
    Ok(Some((data, etag)))
}

/// `GET path`, raw text response.
pub async fn get_text(path: &str) -> Result<String, ApiError> {
    request_text(Method::GET, path, None).await
}

/// `POST path` with a JSON body, JSON response.
pub async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
    path: &str,
    body: &B,
) -> Result<T, ApiError> {
    request(Method::POST, path, Some(to_body(body)?)).await
}

/// `POST path` with a JSON body, raw text response.
pub async fn post_text<B: Serialize + ?Sized>(path: &str, body: &B) -> Result<String, ApiError> {
    request_text(Method::POST, path, Some(to_body(body)?)).await
}

/// `POST path` without a body, JSON response.
pub async fn post_empty<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    request(Method::POST, path, None).await
}

/// `PUT path` with a JSON body, JSON response.
pub async fn put<B: Serialize + ?Sized, T: DeserializeOwned>(
    path: &str,
    body: &B,
) -> Result<T, ApiError> {
    request(Method::PUT, path, Some(to_body(body)?)).await
}

/// `PATCH path` with a JSON body, JSON response.
pub async fn patch<B: Serialize + ?Sized, T: DeserializeOwned>(
    path: &str,
    body: &B,
) -> Result<T, ApiError> {
    request(Method::PATCH, path, Some(to_body(body)?)).await
}

/// `DELETE path`, JSON response.
pub async fn delete<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    request(Method::DELETE, path, None).await
}

/// Enqueues every file of a peer's folder (and its subfolders when `recursive`), saved
/// under `<downloads>/<last path component>/…` (`POST /transfers/downloads/folder`).
pub async fn download_folder(
    username: &str,
    path: &str,
    recursive: bool,
) -> Result<slskr_types::transfers::EnqueuedDownloads, ApiError> {
    post(
        "/transfers/downloads/folder",
        &slskr_types::transfers::EnqueueFolder {
            username: username.to_owned(),
            path: path.to_owned(),
            recursive,
        },
    )
    .await
}
