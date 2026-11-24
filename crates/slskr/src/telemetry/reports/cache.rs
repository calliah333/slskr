use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes, HttpBody, to_bytes},
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::Window;
use crate::{app::AppState, auth::Authenticated};

const TTL: Duration = Duration::from_secs(60);
const MAX_ENTRIES: usize = 64;
const MAX_BODY_BYTES: usize = 512 * 1024;

#[derive(Clone)]
struct Cached {
    at: Instant,
    headers: HeaderMap,
    body: Bytes,
}

impl Cached {
    fn response(&self) -> Response {
        let mut response = Response::new(Body::from(self.body.clone()));
        *response.headers_mut() = self.headers.clone();
        response
    }
}

type Entry = Arc<tokio::sync::Mutex<Option<Cached>>>;

#[derive(Clone, Default)]
pub(super) struct ReportCache(Arc<Mutex<HashMap<String, Entry>>>);

impl ReportCache {
    fn entry(&self, key: String) -> Option<Entry> {
        let mut entries = self.0.lock().expect("report cache lock was poisoned");
        if let Some(entry) = entries.get(&key) {
            return Some(entry.clone());
        }
        entries.retain(|_, entry| {
            entry.try_lock().map_or(true, |entry| {
                entry
                    .as_ref()
                    .is_some_and(|cached| cached.at.elapsed() < TTL)
            }) || Arc::strong_count(entry) > 1
        });
        if entries.len() >= MAX_ENTRIES {
            return None;
        }
        Some(entries.entry(key).or_default().clone())
    }
}

pub(crate) async fn cache_response(
    State(app): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let uri = request.uri();
    if request.method() != Method::GET
        || !uri.path().starts_with("/api/reports/transfers/")
        || !matches!(
            axum::extract::Query::<Window>::try_from_uri(uri),
            Ok(axum::extract::Query(Window {
                start: Some(_),
                end: Some(_),
                ..
            }))
        )
    {
        return next.run(request).await;
    }

    let key = uri.to_string();
    let (mut parts, body) = request.into_parts();
    if let Err(error) = Authenticated::from_request_parts(&mut parts, &app).await {
        return error.into_response();
    }
    let request = Request::from_parts(parts, body);
    let Some(entry) = app.services.telemetry.reports.cache.entry(key) else {
        return next.run(request).await;
    };
    // Serialize misses for this window only. Other reports and cache hits stay independent.
    let mut cached = entry.lock().await;
    if let Some(cached) = cached.as_ref().filter(|cached| cached.at.elapsed() < TTL) {
        return cached.response();
    }
    let response = next.run(request).await;
    if response.status() != StatusCode::OK
        || !response
            .body()
            .size_hint()
            .exact()
            .is_some_and(|size| size <= MAX_BODY_BYTES as u64)
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    // Reports return in-memory JSON with an exact size.
    match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => {
            *cached = Some(Cached {
                at: Instant::now(),
                headers: parts.headers.clone(),
                body: body.clone(),
            });
            Response::from_parts(parts, Body::from(body))
        }
        Err(error) => crate::http::ApiError::Internal(error.into()).into_response(),
    }
}
