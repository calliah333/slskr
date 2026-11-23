use std::collections::{HashMap, HashSet};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{delete, get, post},
};
use chrono::Utc;
use serde::Deserialize;
use slskr_api::transfers::{
    Batch, BatchOptions, DirectoryResponse, EnqueueDownloadBatchRequest,
    EnqueueDownloadBatchResponse, EnqueueDownloadsResponse, QueueDownloadRequest, Transfer,
    TransferDirection, UserResponse,
};
use uuid::Uuid;

use super::{DownloadContext, destination, store};
use crate::{
    app::AppState,
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult},
};

pub fn routes() -> Router<AppState> {
    direction_routes(TransferDirection::Download)
        .merge(direction_routes(TransferDirection::Upload))
        .route("/transfers/downloads/{username}", post(enqueue_downloads))
        .route(
            "/transfers/downloads/{username}/{id}/position",
            get(download_position),
        )
        .route(
            "/transfers/downloads/batches",
            get(list_batches_username).post(enqueue_batch),
        )
        .route("/transfers/downloads/batches/{id}", get(get_batch))
}

fn direction_routes(direction: TransferDirection) -> Router<AppState> {
    let base = format!("/transfers/{}s", direction.as_route());
    Router::new()
        .route(&base, get(list_all))
        .route(&format!("{base}/{{username}}"), get(list_user))
        .route(
            &format!("{base}/{{username}}/{{id}}"),
            get(get_one).delete(cancel),
        )
        .route(&format!("{base}/all/completed"), delete(clear_completed))
        .layer(Extension(direction))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct IncludeRemoved {
    include_removed: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Remove {
    remove: bool,
}

fn require_non_agent(app: &AppState) -> ApiResult<()> {
    let relay = &app.options.startup().relay;
    if relay.enabled && relay.mode.eq_ignore_ascii_case("agent") {
        return Err(ApiError::Forbidden(String::new()));
    }
    Ok(())
}

fn bad_request(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}
fn not_found() -> ApiError {
    ApiError::NotFound(String::new())
}
fn throttled() -> ApiError {
    ApiError::Status(
        StatusCode::TOO_MANY_REQUESTS,
        "Only one concurrent operation is permitted. Wait until the previous request completes"
            .into(),
    )
}

fn canonical_id(id: &str) -> ApiResult<String> {
    Uuid::parse_str(id)
        .map(|id| id.to_string())
        .map_err(|_| bad_request(""))
}

fn optional_id(id: Option<String>) -> ApiResult<Option<String>> {
    id.filter(|id| !id.trim().is_empty())
        .map(|id| {
            Uuid::parse_str(id.trim())
                .map(|id| id.to_string())
                .map_err(|_| {
                    bad_request("One or more provided identifiers is not a valid GUID/UUID")
                })
        })
        .transpose()
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    body.map(|Json(body)| body)
        .map_err(|error| bad_request(&error.body_text()))
}

fn grouped(transfers: Vec<Transfer>) -> Vec<UserResponse> {
    let mut users = Vec::<UserResponse>::new();
    let mut index = HashMap::<String, (usize, HashMap<String, usize>)>::new();
    for file in transfers {
        let (position, directory_index) = index.entry(file.username.clone()).or_insert_with(|| {
            let position = users.len();
            users.push(UserResponse {
                username: file.username.clone(),
                directories: Vec::new(),
            });
            (position, HashMap::new())
        });
        let directories = &mut users[*position].directories;
        let remote_dir = destination::remote_directory(&file.filename);
        let directory = if let Some(&position) = directory_index.get(remote_dir) {
            &mut directories[position]
        } else {
            directory_index.insert(remote_dir.into(), directories.len());
            directories.push(DirectoryResponse {
                directory: remote_dir.into(),
                file_count: 0,
                files: Vec::new(),
            });
            directories.last_mut().expect("added directory")
        };
        directory.file_count += 1;
        directory.files.push(file);
    }
    users
}

async fn list_all(
    _: Authenticated,
    State(app): State<AppState>,
    Extension(direction): Extension<TransferDirection>,
    Query(query): Query<IncludeRemoved>,
) -> ApiResult<Json<Vec<UserResponse>>> {
    require_non_agent(&app)?;
    Ok(Json(grouped(
        app.services
            .transfers
            .list(direction, None, query.include_removed)
            .await?,
    )))
}

async fn list_user(
    _: Authenticated,
    State(app): State<AppState>,
    Extension(direction): Extension<TransferDirection>,
    Path(username): Path<String>,
) -> ApiResult<Json<UserResponse>> {
    user_response(app, direction, username).await
}

// The static POST /downloads/batches path also matches a user called "batches".
async fn list_batches_username(
    _: Authenticated,
    State(app): State<AppState>,
) -> ApiResult<Json<UserResponse>> {
    user_response(app, TransferDirection::Download, "batches".into()).await
}

async fn user_response(
    app: AppState,
    direction: TransferDirection,
    username: String,
) -> ApiResult<Json<UserResponse>> {
    require_non_agent(&app)?;
    grouped(
        app.services
            .transfers
            .list(direction, Some(username), false)
            .await?,
    )
    .pop()
    .map(Json)
    .ok_or_else(not_found)
}

async fn get_one(
    _: Authenticated,
    State(app): State<AppState>,
    Extension(direction): Extension<TransferDirection>,
    Path((_username, id)): Path<(String, String)>,
) -> ApiResult<Json<Transfer>> {
    require_non_agent(&app)?;
    app.services
        .transfers
        .find(direction, canonical_id(&id)?)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

async fn get_batch(
    _: Authenticated,
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Batch>> {
    require_non_agent(&app)?;
    let id = canonical_id(&id)
        .map_err(|_| bad_request(&format!("The specified id {id} is not a valid GUID/UUID")))?;
    app.services
        .transfers
        .batch(id)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

async fn cancel(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Extension(direction): Extension<TransferDirection>,
    Path((_username, id)): Path<(String, String)>,
    Query(query): Query<Remove>,
) -> ApiResult<StatusCode> {
    require_non_agent(&app)?;
    app.services
        .transfers
        .cancel(direction, canonical_id(&id)?, query.remove)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn clear_completed(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Extension(direction): Extension<TransferDirection>,
) -> ApiResult<StatusCode> {
    require_non_agent(&app)?;
    let transfers = &app.services.transfers;
    transfers.ready().await?;
    transfers.flush().await;
    store::remove(transfers.database(), direction, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn download_position(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path((username, id)): Path<(String, String)>,
) -> ApiResult<Json<u32>> {
    require_non_agent(&app)?;
    if app.services.users.is_blacklisted(&username, None) {
        return Err(not_found());
    }
    let transfers = &app.services.transfers;
    let transfer = transfers
        .find(TransferDirection::Download, canonical_id(&id)?)
        .await?
        .ok_or_else(not_found)?;
    transfers
        .download_position(&transfer)
        .await
        .map(Json)
        .map_err(|error| ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

/// Item of the plain `POST /transfers/downloads/{username}` body. slskd binds
/// `Size` as a signed `long`, so negative values must deserialize.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlainDownloadRequest {
    filename: String,
    #[serde(default)]
    size: i64,
}

/// The plain enqueue route only applies slskd's `[Required]` filename and
/// path-traversal checks; the stricter shape checks are batch-only.
fn validate_plain_files(files: &[PlainDownloadRequest]) -> ApiResult<()> {
    if files.is_empty() {
        return Err(bad_request("At least one file is required"));
    }
    if files.iter().any(|file| file.filename.trim().is_empty()) {
        return Err(bad_request("At least one filename is empty or invalid"));
    }
    if files
        .iter()
        .any(|file| destination::traverses(&file.filename))
    {
        return Err(bad_request(
            "One or more files in the request contain a dangerous path traversal segment",
        ));
    }
    Ok(())
}

fn validate_files<T>(files: &[T], item: impl Fn(&T) -> (&str, Option<u64>)) -> ApiResult<()> {
    if files.is_empty() {
        return Err(bad_request("At least one file is required"));
    }
    let mut distinct = HashSet::with_capacity(files.len());
    for file in files {
        let (filename, size) = item(file);
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
        if size.is_none_or(|size| size > i64::MAX as u64) {
            return Err(bad_request("One or more file sizes are missing or invalid"));
        }
    }
    Ok(())
}

async fn check_peer(app: &AppState, username: &str) -> soulseek::Result<()> {
    let offline = || soulseek::Error::UserOffline(format!("User {username} appears to be offline"));
    if app.services.users.is_blacklisted(username, None) {
        return Err(offline());
    }
    let endpoint = app.client.get_user_endpoint(username).await?;
    if app
        .services
        .users
        .is_blacklisted(username, Some(endpoint.ip()))
    {
        return Err(offline());
    }
    Ok(())
}

async fn connect_peer(app: &AppState, username: &str) -> Result<(), ApiError> {
    app.client
        .connect_to_user(username, false, app.shutdown.child_token())
        .await
        .map_err(|error| ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

async fn enqueue_downloads(
    _: RequireReadWrite,
    State(app): State<AppState>,
    Path(username): Path<String>,
    request: Result<Json<Option<Vec<Option<PlainDownloadRequest>>>>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<EnqueueDownloadsResponse>)> {
    require_non_agent(&app)?;
    let files = body(request)?
        .unwrap_or_default()
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| bad_request("One or more files in the request are null"))?;
    validate_plain_files(&files)?;
    let transfers = &app.services.transfers;
    let _permit = transfers.request_permit().ok_or_else(throttled)?;
    let internal = |message: String| ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, message);
    check_peer(&app, &username)
        .await
        .map_err(|error| internal(error.to_string()))?;
    // Upstream's download service rejects repeats after the endpoint lookup,
    // and the controller turns that into a 500 with the exception message.
    let mut distinct = HashSet::with_capacity(files.len());
    if !files
        .iter()
        .all(|file| distinct.insert(file.filename.as_str()))
    {
        return Err(internal(
            "Two or more files in request are duplicated (Parameter 'files')".into(),
        ));
    }
    connect_peer(&app, &username).await?;
    let files = files
        .into_iter()
        .map(|file| QueueDownloadRequest {
            filename: file.filename,
            // slskd's `long Size` defaults to 0; anything non-positive is unknown.
            size: file.size.max(0) as u64,
        })
        .collect();
    let response = transfers
        .enqueue_downloads(username, files, DownloadContext::default())
        .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn enqueue_batch(
    _: RequireReadWrite,
    State(app): State<AppState>,
    request: Result<Json<EnqueueDownloadBatchRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<EnqueueDownloadBatchResponse>)> {
    require_non_agent(&app)?;
    let request = body(request)?;
    if request.username.trim().is_empty() || request.username.encode_utf16().count() > 500 {
        return Err(bad_request("A valid username is required"));
    }
    validate_files(&request.files, |file| (&file.filename, file.size))?;
    if request
        .options
        .destination
        .as_deref()
        .is_some_and(|dest| dest.trim().is_empty() || !destination::relative(dest))
    {
        return Err(bad_request(
            "The destination must be a relative path without path traversal segments",
        ));
    }
    let batch = Batch {
        id: optional_id(request.id)?.unwrap_or_else(|| Uuid::new_v4().to_string()),
        search_id: optional_id(request.search_id)?,
        username: request.username,
        direction: TransferDirection::Download,
        created_at: Utc::now(),
        transfers: None,
        options: Some(BatchOptions {
            destination: request.options.destination,
            external_id: request.options.external_id,
        }),
    };
    let service = &app.services.transfers;
    let _permit = service.request_permit().ok_or_else(throttled)?;
    check_peer(&app, &batch.username).await.map_err(|error| {
        if matches!(error, soulseek::Error::UserOffline(_)) {
            ApiError::NotFound(error.to_string())
        } else {
            ApiError::Status(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    })?;
    service.ready().await?;
    if !store::create_batch(service.database(), batch.clone()).await? {
        return Err(ApiError::Conflict(format!(
            "A batch with ID {} already exists",
            batch.id
        )));
    }
    connect_peer(&app, &batch.username).await?;
    let id = batch.id.clone();
    let username = batch.username.clone();
    let context = DownloadContext::new(&app, Some(batch)).await;
    let files = request
        .files
        .into_iter()
        .map(|file| QueueDownloadRequest {
            filename: file.filename,
            size: file.size.expect("validated size"),
        })
        .collect();
    let response = service.enqueue_downloads(username, files, context).await?;
    let status = if response.failed.is_empty() {
        StatusCode::CREATED
    } else if response.enqueued.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    let batch = service.batch(id).await?.ok_or_else(not_found)?;
    Ok((
        status,
        Json(EnqueueDownloadBatchResponse {
            batch,
            failures: response.failed,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfers::new_transfer;

    #[test]
    fn transfer_routes_merge_and_keep_directory_group_order() {
        let _ = routes();
        let rows = [
            ("first", r"z\first"),
            ("second", r"z\first"),
            ("first", r"a\another"),
            ("first", r"z\last"),
        ]
        .into_iter()
        .map(|(user, filename)| {
            new_transfer(TransferDirection::Download, user, filename.into(), 10, None)
        })
        .collect();
        let users = grouped(rows);
        assert_eq!(users[0].username, "first");
        assert_eq!(users[0].directories[0].directory, "z");
        assert_eq!(users[0].directories[0].file_count, 2);
        assert_eq!(users[0].directories[1].directory, "a");
        assert_eq!(users[1].username, "second");
    }

    #[test]
    fn plain_enqueue_accepts_slskd_shapes_and_rejects_only_required_and_traversal() {
        let files: Vec<PlainDownloadRequest> = serde_json::from_str(
            r#"[{"filename":"Music\\a.flac","size":-5},{"filename":"Music\\dir\\"},
                {"filename":"Music\\a.flac","size":3}]"#,
        )
        .unwrap();
        assert_eq!(files[0].size, -5);
        assert_eq!(files[1].size, 0);
        // Repeats and trailing separators are not shape errors on this route.
        assert!(validate_plain_files(&files).is_ok());

        let reject = |json: &str| {
            let files: Vec<PlainDownloadRequest> = serde_json::from_str(json).unwrap();
            validate_plain_files(&files).unwrap_err()
        };
        assert!(matches!(reject("[]"), ApiError::BadRequest(_)));
        assert!(matches!(
            reject(r#"[{"filename":"  "}]"#),
            ApiError::BadRequest(_)
        ));
        assert!(matches!(
            reject(r#"[{"filename":"Music\\..\\secret"}]"#),
            ApiError::BadRequest(m) if m.contains("traversal")
        ));
        // The batch route keeps its stricter validator.
        let batch = [("a", Some(1)), ("a", Some(1))];
        assert!(validate_files(&batch, |file| (file.0, file.1)).is_err());
    }
}
