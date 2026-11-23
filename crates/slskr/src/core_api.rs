//! Application, session, server, options, settings, and debug HTTP endpoints.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use slskr_api::{
    application::{State as ApplicationState, VersionState},
    config::Config,
    server::ServerState,
    session::{LoginRequest, Session},
    settings::{CreateApiKey, Settings},
};

use crate::{
    app::AppState,
    auth::{
        self, Authenticated, LoginError, RequireAdmin, RequireAdminSession, RequireReadWrite, Role,
    },
    config::{self, SettingsError},
    http::ApiError,
};

type HttpResult<T> = Result<T, Response>;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/application",
            get(application).delete(shutdown).put(restart),
        )
        .route("/application/version", get(version))
        .route("/application/version/latest", get(latest_version))
        .route("/application/gc", post(collect_garbage))
        .route("/application/loopback", post(loopback))
        .route("/session", get(check_session).post(login).delete(logout))
        .route("/session/enabled", get(session_enabled))
        .route("/server", get(server).put(connect).delete(disconnect))
        .route("/options", get(options))
        .route("/options/startup", get(startup_options))
        .route("/settings", get(get_settings).patch(patch_settings))
        .route("/settings/api-keys", post(create_api_key))
        .route("/settings/api-keys/{name}", delete(delete_api_key))
        .route("/settings/{key}", delete(reset_setting))
        .route("/debug/transfers", get(debug_transfers))
}

async fn application(_: Authenticated, State(app): State<AppState>) -> Json<ApplicationState> {
    Json(app.state.get())
}

async fn shutdown(_: RequireAdminSession, State(app): State<AppState>) -> StatusCode {
    app.request_shutdown();
    StatusCode::NO_CONTENT
}

async fn restart(_: RequireAdminSession, State(app): State<AppState>) -> StatusCode {
    app.request_restart();
    StatusCode::NO_CONTENT
}

async fn version(_: Authenticated, State(app): State<AppState>) -> Json<String> {
    Json(app.state.get().version.current)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionQuery {
    #[serde(default)]
    force_check: bool,
}

async fn latest_version(
    caller: Authenticated,
    State(app): State<AppState>,
    Query(query): Query<VersionQuery>,
) -> HttpResult<Json<VersionState>> {
    if query.force_check {
        if !caller.0.role.satisfies(Role::ReadWrite) {
            return Err(StatusCode::FORBIDDEN.into_response());
        }
        let version = app.state.get().version;
        if !version.is_development && !version.is_canary {
            let (latest, available) = check_release(&version)
                .await
                .map_err(|error| ApiError::Internal(error).into_response())?;
            app.state.update(move |state| {
                state.version.latest = Some(latest);
                state.version.is_update_available = Some(available);
            });
        }
    }
    Ok(Json(app.state.get().version))
}

async fn check_release(version: &VersionState) -> anyhow::Result<(String, bool)> {
    use anyhow::{Context, ensure};

    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }

    // A build must identify its own upstream; a slskd release is not a slskr update.
    let repository = env!("CARGO_PKG_REPOSITORY")
        .strip_prefix("https://github.com/")
        .context("No GitHub release repository declared for slskr")?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    ensure!(
        repository.split('/').count() == 2,
        "Invalid GitHub release repository"
    );
    let release = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("slskr/{}", version.full))
        .build()?
        .get(format!(
            "https://api.github.com/repos/{repository}/releases/latest"
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<Release>()
        .await?;
    let latest = release.tag_name.trim_start_matches(['v', 'V']);
    let available = release_number(latest)? > release_number(&version.current)?;
    Ok((
        if available {
            latest.to_owned()
        } else {
            version.current.clone()
        },
        available,
    ))
}

fn release_number(version: &str) -> anyhow::Result<[u32; 3]> {
    use anyhow::{Context, ensure};
    let mut parts = version.split('.');
    let mut number = [0; 3];
    for slot in &mut number {
        *slot = parts
            .next()
            .context("Incomplete release version")?
            .parse()?;
    }
    ensure!(
        parts.next().is_none(),
        "Unexpected release version component"
    );
    Ok(number)
}

async fn collect_garbage(_: RequireAdmin) -> StatusCode {
    // slskd forced a compacting generation-2 .NET collection. Rust drops owned
    // values deterministically and has no managed heap to collect.
    StatusCode::OK
}

async fn loopback(_: RequireReadWrite, headers: HeaderMap, body: Bytes) -> HttpResult<StatusCode> {
    let body = optional_json::<Value>(&headers, &body)?.unwrap_or(Value::Null);
    tracing::info!(body = %body, "Loopback POST");
    Ok(StatusCode::OK)
}

async fn check_session(caller: Authenticated) -> Json<Session> {
    Json(caller.0.session())
}

async fn session_enabled(State(app): State<AppState>) -> Json<bool> {
    Json(app.options.startup().web.auth)
}

/// Signs in and sets the session cookie. With authentication disabled there is nothing
/// to sign in to and the passthrough principal is returned.
async fn login(
    State(app): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResult<Response> {
    // Refuse sign-in forced by another site (login CSRF).
    if !auth::same_origin(&headers, &uri) {
        return Err(LoginError::CrossSite.into_response());
    }
    if !app.auth.enabled() {
        return Ok(Json(auth::Principal::passthrough().session()).into_response());
    }
    let request = optional_json::<LoginRequest>(&headers, &body)?
        .ok_or_else(|| bad_request("No credentials provided"))?;
    let (token, principal) = app
        .auth
        .login(
            request.username.as_deref().unwrap_or_default(),
            request.password.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(IntoResponse::into_response)?;
    let cookie = app
        .auth
        .session_cookie(&token, auth::forwarded_https(&headers));
    Ok(([(header::SET_COOKIE, cookie)], Json(principal.session())).into_response())
}

/// Signs out: deletes the session and clears the cookie.
async fn logout(State(app): State<AppState>, uri: axum::http::Uri, headers: HeaderMap) -> Response {
    if !auth::same_origin(&headers, &uri) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(error) = app.auth.logout(&headers).await {
        return ApiError::Internal(error).into_response();
    }
    let cookie = app.auth.clear_cookie(auth::forwarded_https(&headers));
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}

async fn server(_: Authenticated, State(app): State<AppState>) -> Json<ServerState> {
    Json(app.state.get().server)
}

async fn connect(_: RequireReadWrite, State(app): State<AppState>) -> HttpResult<StatusCode> {
    let restarted = app
        .connect()
        .await
        .map_err(|error| ApiError::Internal(error).into_response())?;
    Ok(if restarted {
        StatusCode::RESET_CONTENT
    } else {
        StatusCode::OK
    })
}

async fn disconnect(
    _: RequireReadWrite,
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResult<StatusCode> {
    let message = optional_json::<String>(&headers, &body)?;
    app.disconnect(message.as_deref().unwrap_or("Requested by user"))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn options(_: Authenticated, State(app): State<AppState>) -> Json<Config> {
    Json(app.options.get().redact())
}

async fn startup_options(_: RequireAdmin, State(app): State<AppState>) -> Json<Config> {
    Json(app.options.startup().redact())
}

async fn get_settings(_: RequireAdminSession, State(app): State<AppState>) -> Json<Settings> {
    Json(app.options.settings().await)
}

/// Applies a settings change at once, so a credential change signs sessions out before the
/// response (the options watcher applies it again, as a no-op).
fn applied(app: &AppState, result: Result<Settings, SettingsError>) -> HttpResult<Json<Settings>> {
    let settings = result.map_err(settings_error)?;
    app.auth.update(&config::auth_config(&app.options.get()));
    Ok(Json(settings))
}

async fn patch_settings(
    _: RequireAdminSession,
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResult<Json<Settings>> {
    let patch = optional_json::<Value>(&headers, &body)?
        .ok_or_else(|| bad_request("No settings provided"))?;
    let result = app.options.patch(patch).await;
    applied(&app, result)
}

async fn reset_setting(
    _: RequireAdminSession,
    State(app): State<AppState>,
    Path(key): Path<String>,
) -> HttpResult<Json<Settings>> {
    let result = app.options.reset(&key).await;
    applied(&app, result)
}

async fn create_api_key(
    _: RequireAdminSession,
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResult<Response> {
    let request = optional_json::<CreateApiKey>(&headers, &body)?
        .ok_or_else(|| bad_request("No API key provided"))?;
    let created = app
        .options
        .create_api_key(request)
        .await
        .map_err(settings_error)?;
    app.auth.update(&config::auth_config(&app.options.get()));
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

async fn delete_api_key(
    _: RequireAdminSession,
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> HttpResult<StatusCode> {
    app.options
        .delete_api_key(&name)
        .await
        .map_err(settings_error)?;
    app.auth.update(&config::auth_config(&app.options.get()));
    Ok(StatusCode::NO_CONTENT)
}

fn settings_error(error: SettingsError) -> Response {
    match error {
        SettingsError::Pinned(message) | SettingsError::Conflict(message) => {
            ApiError::Conflict(message)
        }
        SettingsError::Invalid(message) => ApiError::BadRequest(message),
        SettingsError::NotFound(message) => ApiError::NotFound(message),
        SettingsError::Internal(error) => ApiError::Internal(error),
    }
    .into_response()
}

fn media_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
}

fn is_json(media: &str) -> bool {
    media.eq_ignore_ascii_case("application/json")
        || media.split_once('/').is_some_and(|(kind, subtype)| {
            kind.eq_ignore_ascii_case("application")
                && subtype
                    .get(subtype.len().saturating_sub(5)..)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
        })
}

fn optional_json<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> HttpResult<Option<T>> {
    if body.is_empty() {
        return Ok(None);
    }
    if !is_json(media_type(headers)) {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
    }
    serde_json::from_slice(body).map_err(|error| bad_request(error.to_string()))
}

fn bad_request(message: impl Into<String>) -> Response {
    ApiError::BadRequest(message.into()).into_response()
}

async fn debug_transfers(_: RequireAdmin, State(app): State<AppState>) -> Json<Value> {
    let uploads = app
        .client
        .uploads()
        .into_iter()
        .map(transfer_json)
        .collect::<Vec<_>>();
    let downloads = app
        .client
        .downloads()
        .into_iter()
        .map(transfer_json)
        .collect::<Vec<_>>();
    Json(json!({"soulseek": {"uploads": uploads, "downloads": downloads}}))
}

fn transfer_json(transfer: soulseek::Transfer) -> Value {
    let mut fields = json!({
        "averageSpeed": transfer.average_speed,
        "bytesRemaining": transfer.bytes_remaining(),
        "bytesTransferred": transfer.bytes_transferred,
        "direction": transfer.direction.as_str(),
        "filename": transfer.filename,
        "percentComplete": transfer.percent_complete(),
        "size": transfer.size,
        "startOffset": transfer.start_offset,
        "state": transfer.state.to_slskd_string(),
        "token": transfer.token as i32,
        "username": transfer.username,
    });
    let map = fields.as_object_mut().expect("transfer JSON is an object");
    if let Some(time) = transfer.start_time {
        map.insert("startTime".into(), json!(DateTime::<Utc>::from(time)));
    }
    if let Some(time) = transfer.end_time {
        map.insert("endTime".into(), json!(DateTime::<Utc>::from(time)));
    }
    if let Some(elapsed) = transfer.elapsed() {
        map.insert("elapsedTime".into(), json!(time_span(elapsed)));
    }
    if let Some(remaining) = transfer.remaining() {
        map.insert("remainingTime".into(), json!(time_span(remaining)));
    }
    if let Some(token) = transfer.remote_token {
        map.insert("remoteToken".into(), json!(token as i32));
    }
    if let Some(endpoint) = transfer.ip_endpoint {
        map.insert(
            "ipEndPoint".into(),
            json!({"address": endpoint.ip(), "port": endpoint.port()}),
        );
    }
    if let Some(exception) = transfer.exception {
        map.insert("exception".into(), json!({"message": exception}));
    }
    fields
}

fn time_span(duration: Duration) -> String {
    use std::fmt::Write as _;
    let seconds = duration.as_secs();
    let days = seconds / 86400;
    let mut result = if days > 0 {
        format!("{days}.")
    } else {
        String::new()
    };
    let _ = write!(
        result,
        "{:02}:{:02}:{:02}",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    );
    let ticks = duration.subsec_nanos() / 100;
    if ticks != 0 {
        let _ = write!(result, ".{ticks:07}");
    }
    result
}
