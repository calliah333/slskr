//! HTTP API client (port of slskd `lib/api.js`, an axios instance).
//!
//! Paths are relative to [`api_base_url`], e.g. `api::get::<State>("/application")`.
//! Every request carries `Content-Type: application/json`, credentials, and the bearer
//! token unless passthrough auth is active. A 401 from any route other than
//! `/session`, `/server` and `/application` logs out (clears the token and reloads).

use std::fmt;

use gloo_net::http::{Method, RequestBuilder};
use serde::{Serialize, de::DeserializeOwned};
use web_sys::RequestCredentials;

use super::token::{clear_token, get_token, is_passthrough_enabled};
use crate::config::api_base_url;

/// Routes whose 401 responses do not trigger a logout.
const UNAUTHORIZED_EXEMPT: [&str; 3] = ["/session", "/server", "/application"];

/// A failed request: HTTP status (0 for network/serialization failures) and the response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// Response body (like axios `error.response.data`), or a generic description when empty.
    pub message: String,
}

impl ApiError {
    fn local(message: impl fmt::Display) -> Self {
        Self {
            status: 0,
            message: message.to_string(),
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

fn logout_and_reload() {
    clear_token();
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
    let url = format!("{}{}", api_base_url(), path);
    let mut builder = RequestBuilder::new(&url)
        .method(method)
        .credentials(RequestCredentials::Include)
        .header("Content-Type", "application/json");

    if !is_passthrough_enabled()
        && let Some(token) = get_token()
    {
        builder = builder.header("Authorization", &format!("Bearer {token}"));
    }

    let response = match body {
        Some(body) => builder.body(body).map_err(ApiError::local)?.send().await,
        None => builder.send().await,
    }
    .map_err(ApiError::local)?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if response.ok() {
        return Ok(text);
    }

    if status == 401 && !UNAUTHORIZED_EXEMPT.contains(&path) {
        logout_and_reload();
    }

    let message = if text.is_empty() {
        format!("Request failed with status code {status}")
    } else {
        text
    };
    Err(ApiError { status, message })
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

/// `PUT path` with a JSON body, raw text response.
pub async fn put_text<B: Serialize + ?Sized>(path: &str, body: &B) -> Result<String, ApiError> {
    request_text(Method::PUT, path, Some(to_body(body)?)).await
}

/// `PUT path` without a body, JSON response.
pub async fn put_empty<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    request(Method::PUT, path, None).await
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

/// `DELETE path` with a JSON body (axios `{ data }`), JSON response.
pub async fn delete_with<B: Serialize + ?Sized, T: DeserializeOwned>(
    path: &str,
    body: &B,
) -> Result<T, ApiError> {
    request(Method::DELETE, path, Some(to_body(body)?)).await
}
