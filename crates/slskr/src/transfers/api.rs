//! `/api/transfers/{downloads,uploads}` and `/api/batches`.

use std::collections::HashSet;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::Deserialize;
use slskr_types::transfers::{
    Batch, CreateBatch, CreatedBatch, DownloadFile, EnqueueDownloads, EnqueuedDownloads,
    QueuePosition, Transfer, TransferDirection,
};
use uuid::Uuid;

use super::{DownloadContext, destination, store};
use crate::{
    app::AppState,
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult, Json, Path, Query, Routes},
};

const DOWNLOADS: bool = false;
const UPLOADS: bool = true;

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/transfers/downloads", list::<DOWNLOADS>)
        .post("/api/transfers/downloads", enqueue)
        .delete(
            "/api/transfers/downloads/completed",
            clear_completed::<DOWNLOADS>,
        )
        .get("/api/transfers/downloads/{id}", get_one::<DOWNLOADS>)
        .delete("/api/transfers/downloads/{id}", remove::<DOWNLOADS>)
        .post("/api/transfers/downloads/{id}/cancel", cancel::<DOWNLOADS>)
        .get("/api/transfers/downloads/{id}/position", position)
        .get("/api/transfers/uploads", list::<UPLOADS>)
        .delete(
            "/api/transfers/uploads/completed",
            clear_completed::<UPLOADS>,
        )
        .get("/api/transfers/uploads/{id}", get_one::<UPLOADS>)
        .delete("/api/transfers/uploads/{id}", remove::<UPLOADS>)
        .post("/api/transfers/uploads/{id}/cancel", cancel::<UPLOADS>)
        .post("/api/batches", create_batch)
        .get("/api/batches/{id}", get_batch)
}

const fn direction<const UPLOAD: bool>() -> TransferDirection {
    if UPLOAD {
        TransferDirection::Upload
    } else {
        TransferDirection::Download
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ListQuery {
    username: Option<String>,
    include_removed: bool,
}

fn bad_request(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn throttled() -> ApiError {
    ApiError::Status(
        StatusCode::TOO_MANY_REQUESTS,
        "Only one enqueue request may run at a time; retry when the previous one completes".into(),
    )
}

/// Ids are UUIDs; anything else names no transfer or batch.
fn canonical_id(id: &str) -> ApiResult<String> {
    Uuid::parse_str(id)
        .map(|id| id.to_string())
        .map_err(|_| ApiError::not_found())
}

fn optional_id(id: Option<String>, name: &str) -> ApiResult<Option<String>> {
    id.filter(|id| !id.trim().is_empty())
        .map(|id| {
            Uuid::parse_str(id.trim())
                .map(|id| id.to_string())
                .map_err(|_| bad_request(&format!("{name} must be a UUID")))
        })
        .transpose()
}

async fn list<const UPLOAD: bool>(
    _: Authenticated,
    State(app): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<Transfer>>> {
    let username = query.username.filter(|name| !name.is_empty());
    Ok(Json(
        app.services
            .transfers
            .list(direction::<UPLOAD>(), username, query.include_removed)
            .await?,
    ))
}

async fn get_one<const UPLOAD: bool>(
    _: Authenticated,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Transfer>> {
    app.services
        .transfers
        .find(direction::<UPLOAD>(), canonical_id(&id)?)
        .await?
        .map(Json)
        .ok_or_else(ApiError::not_found)
}

/// Cancels a transfer if it is active and keeps it in the list (204).
async fn cancel<const UPLOAD: bool>(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    stop(&app, direction::<UPLOAD>(), &id, false).await
}

/// Cancels a transfer if it is active and removes it from the list (204).
async fn remove<const UPLOAD: bool>(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    stop(&app, direction::<UPLOAD>(), &id, true).await
}

async fn stop(
    app: &AppState,
    direction: TransferDirection,
    id: &str,
    remove: bool,
) -> ApiResult<StatusCode> {
    let transfers = &app.services.transfers;
    let id = canonical_id(id)?;
    transfers
        .find(direction, id.clone())
        .await?
        .ok_or_else(ApiError::not_found)?;
    transfers.cancel(direction, id, remove).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes every completed transfer from the list (204).
async fn clear_completed<const UPLOAD: bool>(
    _: RequireReadWrite,
    State(app): State<AppState>,
) -> ApiResult<StatusCode> {
    let transfers = &app.services.transfers;
    transfers.ready().await?;
    transfers.flush().await;
    store::remove(transfers.database(), direction::<UPLOAD>(), None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Asks the peer for a queued download's place in its upload queue.
async fn position(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<QueuePosition>> {
    let transfers = &app.services.transfers;
    let transfer = transfers
        .find(TransferDirection::Download, canonical_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if app.services.users.is_blacklisted(&transfer.username, None) {
        return Err(ApiError::not_found());
    }
    let place_in_queue = transfers
        .download_position(&transfer)
        .await
        .map_err(|error| match error.downcast::<soulseek::Error>() {
            Ok(error) => ApiError::from(error),
            Err(error) => ApiError::Internal(error),
        })?;
    Ok(Json(QueuePosition { place_in_queue }))
}

fn validate_username(username: &str) -> ApiResult<()> {
    if username.trim().is_empty() || username.encode_utf16().count() > 500 {
        return Err(bad_request("A valid username is required"));
    }
    Ok(())
}

fn validate_files(files: &[DownloadFile]) -> ApiResult<()> {
    if files.is_empty() {
        return Err(bad_request("At least one file is required"));
    }
    let mut distinct = HashSet::with_capacity(files.len());
    for file in files {
        let filename = file.filename.as_str();
        if filename.trim().is_empty() || filename.ends_with(['/', '\\']) || filename.contains('\0')
        {
            return Err(bad_request("At least one filename is empty or invalid"));
        }
        if destination::traverses(filename) {
            return Err(bad_request(
                "One or more files in the request contain a dangerous path traversal segment",
            ));
        }
        if !distinct.insert(filename) {
            return Err(bad_request("Two or more files in the request are repeated"));
        }
        if file.size > i64::MAX as u64 {
            return Err(bad_request("One or more file sizes are invalid"));
        }
    }
    Ok(())
}

/// Checks that the peer is reachable and not blacklisted, then connects to it.
async fn prepare_peer(app: &AppState, username: &str) -> ApiResult<()> {
    let offline = || soulseek::Error::UserOffline(format!("User {username} appears to be offline"));
    if app.services.users.is_blacklisted(username, None) {
        return Err(offline().into());
    }
    let endpoint = app.client.get_user_endpoint(username).await?;
    if app
        .services
        .users
        .is_blacklisted(username, Some(endpoint.ip()))
    {
        return Err(offline().into());
    }
    app.client
        .connect_to_user(username, false, app.shutdown.child_token())
        .await?;
    Ok(())
}

/// 201 when every file was enqueued, 207 when some failed, 200 when none was enqueued.
fn enqueue_status(enqueued: usize, failed: usize) -> StatusCode {
    if failed == 0 {
        StatusCode::CREATED
    } else if enqueued == 0 {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    }
}

async fn enqueue(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Json(request): Json<EnqueueDownloads>,
) -> ApiResult<Response> {
    validate_username(&request.username)?;
    validate_files(&request.files)?;
    let transfers = &app.services.transfers;
    let _permit = transfers.request_permit().ok_or_else(throttled)?;
    prepare_peer(&app, &request.username).await?;
    let response: EnqueuedDownloads = transfers
        .enqueue_downloads(request.username, request.files, DownloadContext::default())
        .await?;
    let status = enqueue_status(response.enqueued.len(), response.failed.len());
    Ok((status, Json(response)).into_response())
}

async fn create_batch(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Json(request): Json<CreateBatch>,
) -> ApiResult<Response> {
    validate_username(&request.username)?;
    validate_files(&request.files)?;
    if request
        .destination
        .as_deref()
        .is_some_and(|dest| dest.trim().is_empty() || !destination::relative(dest))
    {
        return Err(bad_request(
            "destination must be a relative path without path traversal segments",
        ));
    }
    let batch = Batch {
        id: optional_id(request.id, "id")?.unwrap_or_else(|| Uuid::new_v4().to_string()),
        search_id: optional_id(request.search_id, "search_id")?,
        username: request.username,
        created_at: Utc::now(),
        destination: request.destination,
        external_id: request.external_id,
        transfers: Vec::new(),
    };
    let service = &app.services.transfers;
    let _permit = service.request_permit().ok_or_else(throttled)?;
    service.ready().await?;
    if !store::create_batch(service.database(), batch.clone()).await? {
        return Err(ApiError::Conflict(format!(
            "A batch with id {} already exists",
            batch.id
        )));
    }
    prepare_peer(&app, &batch.username).await?;
    let id = batch.id.clone();
    let username = batch.username.clone();
    let context = DownloadContext::new(&app, Some(batch)).await;
    let response = service
        .enqueue_downloads(username, request.files, context)
        .await?;
    let status = enqueue_status(response.enqueued.len(), response.failed.len());
    let batch = service.batch(id).await?.ok_or_else(ApiError::not_found)?;
    Ok((
        status,
        Json(CreatedBatch {
            batch,
            failed: response.failed,
        }),
    )
        .into_response())
}

async fn get_batch(
    _: Authenticated,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Batch>> {
    app.services
        .transfers
        .batch(canonical_id(&id)?)
        .await?
        .map(Json)
        .ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_validation_and_status() {
        let _ = routes();
        let files = |json: &str| serde_json::from_str::<Vec<DownloadFile>>(json).unwrap();
        assert!(validate_files(&files(r#"[{"filename":"Music\\a.flac","size":3}]"#)).is_ok());
        for invalid in [
            "[]",
            r#"[{"filename":"  ","size":1}]"#,
            r#"[{"filename":"Music\\dir\\","size":1}]"#,
            r#"[{"filename":"Music\\..\\secret","size":1}]"#,
            r#"[{"filename":"a","size":1},{"filename":"a","size":1}]"#,
        ] {
            assert!(
                matches!(
                    validate_files(&files(invalid)),
                    Err(ApiError::BadRequest(_))
                ),
                "{invalid}"
            );
        }
        assert_eq!(enqueue_status(2, 0), StatusCode::CREATED);
        assert_eq!(enqueue_status(1, 1), StatusCode::MULTI_STATUS);
        assert_eq!(enqueue_status(0, 2), StatusCode::OK);
        assert!(matches!(canonical_id("x"), Err(ApiError::NotFound(_))));
    }
}
