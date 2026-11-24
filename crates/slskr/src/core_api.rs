//! Application, sign-in, server connection, configuration and settings endpoints.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;
use slskr_types::{
    application::{State as ApplicationState, VersionState},
    config::Config,
    server::ServerState,
    session::{AuthInfo, LoginRequest, Session},
    settings::{ApiKeyInfo, CreateApiKey, Settings},
};

use crate::{
    app::AppState,
    auth::{self, Authenticated, LoginError, RequireAdminSession, RequireReadWrite},
    config::{self, SettingsError},
    http::{ApiError, ApiResult, Json, Path, Routes, optional_json},
};

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/application", application)
        .post("/api/application/shutdown", shutdown)
        .post("/api/application/restart", restart)
        .get("/api/application/version", version)
        .get("/api/auth", auth_info)
        .get("/api/session", check_session)
        .post("/api/session", login)
        .delete("/api/session", logout)
        .get("/api/server", server)
        .post("/api/server/connect", connect)
        .post("/api/server/disconnect", disconnect)
        .get("/api/config", effective_config)
        .get("/api/settings", get_settings)
        .patch("/api/settings", patch_settings)
        .delete("/api/settings/{key}", reset_setting)
        .get("/api/api-keys", list_api_keys)
        .post("/api/api-keys", create_api_key)
        .delete("/api/api-keys/{name}", delete_api_key)
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

async fn version(_: Authenticated, State(app): State<AppState>) -> Json<VersionState> {
    Json(app.state.get().version)
}

async fn auth_info(State(app): State<AppState>) -> Json<AuthInfo> {
    Json(AuthInfo {
        enabled: app.options.startup().web.auth,
        setup_required: crate::setup::required(&app),
    })
}

async fn check_session(caller: Authenticated) -> Json<Session> {
    Json(caller.0.session())
}

/// Signs in and sets the session cookie (201). With authentication disabled there is
/// nothing to sign in to and the passthrough principal is returned (200).
async fn login(
    State(app): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    // Refuse sign-in forced by another site (login CSRF).
    if !auth::same_origin(&headers, &uri) {
        return Err(LoginError::CrossSite.into());
    }
    if !app.auth.enabled() {
        return Ok(Json(auth::Principal::passthrough().session()).into_response());
    }
    let request = optional_json::<LoginRequest>(&headers, &body)?
        .ok_or_else(|| ApiError::BadRequest("No credentials provided".into()))?;
    let (token, principal) = app.auth.login(&request.username, &request.password).await?;
    let cookie = app
        .auth
        .session_cookie(&token, auth::forwarded_https(&headers));
    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(principal.session()),
    )
        .into_response())
}

/// Signs out: deletes the session and clears the cookie.
async fn logout(
    State(app): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !auth::same_origin(&headers, &uri) {
        return Err(LoginError::CrossSite.into());
    }
    app.auth.logout(&headers).await?;
    let cookie = app.auth.clear_cookie(auth::forwarded_https(&headers));
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}

async fn server(_: Authenticated, State(app): State<AppState>) -> Json<ServerState> {
    Json(app.state.get().server)
}

async fn connect(_: RequireReadWrite, State(app): State<AppState>) -> ApiResult<StatusCode> {
    if crate::app::credentials(&app.options.get()).is_none() {
        return Err(ApiError::Conflict(
            "Soulseek credentials are not configured; set them in System → Settings".into(),
        ));
    }
    app.connect().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DisconnectRequest {
    #[serde(default)]
    message: Option<String>,
}

async fn disconnect(
    _: RequireReadWrite,
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let request = optional_json::<DisconnectRequest>(&headers, &body)?;
    let message = request.and_then(|request| request.message);
    app.disconnect(message.as_deref().unwrap_or("Requested by user"))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn effective_config(_: Authenticated, State(app): State<AppState>) -> Json<Config> {
    Json(app.options.get().redact())
}

async fn get_settings(_: RequireAdminSession, State(app): State<AppState>) -> Json<Settings> {
    Json(app.options.settings().await)
}

/// Applies a settings change at once, so a credential change signs sessions out before the
/// response (the options watcher applies it again, as a no-op).
fn applied(app: &AppState, result: Result<Settings, SettingsError>) -> ApiResult<Json<Settings>> {
    let settings = result.map_err(settings_error)?;
    app.auth.update(&config::auth_config(&app.options.get()));
    Ok(Json(settings))
}

async fn patch_settings(
    _: RequireAdminSession,
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Settings>> {
    let patch = optional_json::<Value>(&headers, &body)?
        .ok_or_else(|| ApiError::BadRequest("No settings provided".into()))?;
    let result = app.options.patch(patch).await;
    applied(&app, result)
}

async fn reset_setting(
    _: RequireAdminSession,
    State(app): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Settings>> {
    let result = app.options.reset(&key).await;
    applied(&app, result)
}

async fn list_api_keys(
    _: RequireAdminSession,
    State(app): State<AppState>,
) -> Json<Vec<ApiKeyInfo>> {
    Json(app.options.settings().await.api_keys)
}

async fn create_api_key(
    _: RequireAdminSession,
    State(app): State<AppState>,
    Json(request): Json<CreateApiKey>,
) -> ApiResult<Response> {
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
) -> ApiResult<StatusCode> {
    app.options
        .delete_api_key(&name)
        .await
        .map_err(settings_error)?;
    app.auth.update(&config::auth_config(&app.options.get()));
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn settings_error(error: SettingsError) -> ApiError {
    match error {
        SettingsError::Pinned(message) | SettingsError::Conflict(message) => {
            ApiError::Conflict(message)
        }
        SettingsError::Invalid(message) => ApiError::BadRequest(message),
        SettingsError::NotFound(message) => ApiError::NotFound(message),
        SettingsError::Internal(error) => ApiError::Internal(error),
    }
}
