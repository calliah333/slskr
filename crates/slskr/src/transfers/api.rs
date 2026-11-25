//! `/api/transfers/{downloads,uploads}` and `/api/batches`.

use std::collections::HashSet;

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use slskr_types::transfers::{
    Batch, CreateBatch, CreatedBatch, DownloadFile, EnqueueDownloads, EnqueueDownloadsRequest,
    EnqueueFolder, EnqueuedDownloads, QueuePosition, Transfer, TransferDirection,
};
use uuid::Uuid;

use super::{DownloadContext, destination, folder, store};
use crate::{
    app::AppState,
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult, Json, Path, Query, Routes, if_none_match},
    users::convert_directory,
};

const DOWNLOADS: bool = false;
const UPLOADS: bool = true;

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/transfers/downloads", list::<DOWNLOADS>)
        .post("/api/transfers/downloads", enqueue)
        .post("/api/transfers/downloads/folder", enqueue_folder)
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
    recent_completed: Option<u32>,
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
    headers: HeaderMap,
) -> ApiResult<Response> {
    let transfers = &app.services.transfers;
    transfers.ready().await?;
    let direction = direction::<UPLOAD>();
    let username = query.username.filter(|name| !name.is_empty());
    let revision = transfers.idle_list_revision(direction, username.as_deref());
    // A weak tag works across compression variants. Scope it to this list and daemon instance.
    let tag = revision.map(|revision| {
        let user = Sha256::digest(username.as_deref().unwrap_or_default().as_bytes());
        format!(
            "W/\"{}-{revision}-{UPLOAD}-{}-{}-{user:x}\"",
            transfers.0.list_epoch,
            query.include_removed,
            query.recent_completed.map_or(-1, i64::from)
        )
    });
    let (mut response, tag) = if if_none_match(&headers, tag.as_deref()) {
        (StatusCode::NOT_MODIFIED.into_response(), tag)
    } else {
        let snapshot = transfers
            .list(
                direction,
                username.clone(),
                query.include_removed,
                query.recent_completed,
            )
            .await?;
        // The SQL read and live overlay are separate. Only tag stable snapshots.
        let stable = revision.is_some()
            && revision == transfers.idle_list_revision(direction, username.as_deref())
            && snapshot
                .iter()
                .all(|t| t.started_at.is_none() || t.ended_at.is_some());
        (Json(snapshot).into_response(), tag.filter(|_| stable))
    };
    if let Some(tag) = tag {
        response.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&tag).expect("ASCII ETag"),
        );
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
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
    let removed = store::remove(transfers.database(), direction::<UPLOAD>(), None).await;
    transfers.wrote(direction::<UPLOAD>());
    let removed = removed?;
    transfers.publish_removed(removed);
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

/// Why a remote file cannot be downloaded, if it cannot.
pub(super) fn file_problem(filename: &str, size: u64) -> Option<&'static str> {
    if filename.trim().is_empty() || filename.ends_with(['/', '\\']) || filename.contains('\0') {
        Some("At least one filename is empty or invalid")
    } else if destination::traverses(filename) {
        Some("One or more files in the request contain a dangerous path traversal segment")
    } else if size > i64::MAX as u64 {
        Some("One or more file sizes are invalid")
    } else {
        None
    }
}

fn validate_files(files: &[DownloadFile]) -> ApiResult<()> {
    if files.is_empty() {
        return Err(bad_request("At least one file is required"));
    }
    let mut distinct = HashSet::with_capacity(files.len());
    for file in files {
        if let Some(problem) = file_problem(&file.filename, file.size) {
            return Err(bad_request(problem));
        }
        if !distinct.insert(file.filename.as_str()) {
            return Err(bad_request("Two or more files in the request are repeated"));
        }
    }
    Ok(())
}

/// A remote folder without trailing separators; it must have a name and no traversal.
fn validate_folder<'a>(folder: &'a str, field: &str) -> ApiResult<&'a str> {
    let folder = destination::trim_folder(folder);
    if folder.trim().is_empty()
        || folder.contains('\0')
        || destination::traverses(folder)
        || destination::folder_name(folder).trim().is_empty()
    {
        return Err(bad_request(&format!(
            "{field} must name a remote folder without path traversal segments"
        )));
    }
    Ok(folder)
}

/// The `root` of an enqueue request (empty means none); every file must be below it.
fn validate_root(root: Option<String>, files: &[DownloadFile]) -> ApiResult<Option<String>> {
    let Some(root) = root.filter(|root| !root.trim().is_empty()) else {
        return Ok(None);
    };
    let root = validate_folder(&root, "root")?;
    if files
        .iter()
        .any(|file| destination::relative_to(root, &file.filename).is_none())
    {
        return Err(bad_request("Every file must be below root"));
    }
    Ok(Some(root.to_owned()))
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
    Json(request): Json<EnqueueDownloadsRequest>,
) -> ApiResult<Response> {
    let EnqueueDownloadsRequest {
        downloads: EnqueueDownloads { username, files },
        root,
    } = request;
    validate_username(&username)?;
    validate_files(&files)?;
    let root = validate_root(root, &files)?;
    let transfers = &app.services.transfers;
    let _permit = transfers.request_permit().ok_or_else(throttled)?;
    prepare_peer(&app, &username).await?;
    let response: EnqueuedDownloads = transfers
        .enqueue_downloads(username, files, root, DownloadContext::default())
        .await?;
    let status = enqueue_status(response.enqueued.len(), response.failed.len());
    Ok((status, Json(response)).into_response())
}

/// Enqueues every file of a peer's folder (and, when recursive, of the folders below
/// it), saved as `<downloads>/<folder name>/<path below the folder>`.
async fn enqueue_folder(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Json(request): Json<EnqueueFolder>,
) -> ApiResult<Response> {
    validate_username(&request.username)?;
    let root = validate_folder(&request.path, "path")?.to_owned();
    let username = request.username;
    prepare_peer(&app, &username).await?;
    let mut directories: Vec<_> = app
        .client
        .get_directory_contents(&username, &root, None, app.shutdown.child_token())
        .await?
        .into_iter()
        .map(convert_directory)
        .collect();
    if request.recursive && !folder::has_subfolders(&root, &directories) {
        // Most clients list only the requested folder; the browse cache (or a new
        // browse) has the folders below it.
        let shares = app.services.users.browse_peer(&username).await?;
        directories.extend(
            shares
                .directories
                .iter()
                .filter(|directory| destination::relative_to(&root, &directory.name).is_some())
                .cloned(),
        );
    }
    let plan = folder::plan(&root, request.recursive, directories)
        .filter(|plan| !plan.files.is_empty() || !plan.rejected.is_empty())
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "Folder {root} of user {username} does not exist or has no files"
            ))
        })?;
    let transfers = &app.services.transfers;
    let _permit = transfers.request_permit().ok_or_else(throttled)?;
    let mut response = if plan.files.is_empty() {
        EnqueuedDownloads::default()
    } else {
        transfers
            .enqueue_downloads(username, plan.files, Some(root), DownloadContext::default())
            .await?
    };
    response.files += plan.rejected.len();
    response.failed.extend(plan.rejected);
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
    // Reach the peer first: a batch stored for an offline peer would have no transfers,
    // and retrying with the same id would then conflict.
    prepare_peer(&app, &batch.username).await?;
    if !store::create_batch(service.database(), batch.clone()).await? {
        return Err(ApiError::Conflict(format!(
            "A batch with id {} already exists",
            batch.id
        )));
    }
    let id = batch.id.clone();
    let username = batch.username.clone();
    let context = DownloadContext::new(&app, Some(batch)).await;
    let response = service
        .enqueue_downloads(username, request.files, None, context)
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

    #[test]
    fn root_and_folder_validation() {
        let files = |names: &[&str]| -> Vec<DownloadFile> {
            names
                .iter()
                .map(|filename| DownloadFile {
                    filename: (*filename).into(),
                    size: 1,
                })
                .collect()
        };
        let album = files(&[r"@@m\Album\01.flac", r"@@m\Album\CD2\01.flac"]);
        assert_eq!(validate_root(None, &album).unwrap(), None);
        assert_eq!(validate_root(Some("  ".into()), &album).unwrap(), None);
        assert_eq!(
            validate_root(Some(r"@@m\Album\".into()), &album).unwrap(),
            Some(r"@@m\Album".into())
        );
        for root in [
            r"@@m\Album\CD2",
            r"@@m\Alb",
            r"@@m\Album\01.flac",
            r"@@m\..",
            "\\",
        ] {
            assert!(
                matches!(
                    validate_root(Some(root.into()), &album),
                    Err(ApiError::BadRequest(_))
                ),
                "{root}"
            );
        }
        assert_eq!(validate_folder(r"a\b\\", "path").unwrap(), r"a\b");
        for path in ["", "  ", r"\", r"a\..\b", r".", "a\0b", r"a\ "] {
            assert!(
                matches!(validate_folder(path, "path"), Err(ApiError::BadRequest(_))),
                "{path:?}"
            );
        }
        assert_eq!(file_problem(r"a\b", 1), None);
        assert!(file_problem(r"a\..\b", 1).is_some());
    }
}
