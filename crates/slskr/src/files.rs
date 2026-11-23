//! Download and incomplete-directory browsing and deletion routes.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use axum::{
    Json, Router,
    extract::{Path as RoutePath, Query, State},
    http::StatusCode,
    routing::{delete, get},
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use slskr_api::{
    config::Config,
    files::{FilesystemDirectory, FilesystemFile},
};

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult},
};

#[derive(Clone)]
pub struct FileService {
    core: Core,
}

impl FileService {
    pub fn new(core: &Core) -> anyhow::Result<Self> {
        Ok(Self { core: core.clone() })
    }

    pub fn start(&self, _app: AppState) {}
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    recursive: bool,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/files/downloads/directories", get(download_root))
        .route(
            "/files/downloads/directories/{path}",
            get(download_subdir).delete(delete_download_dir),
        )
        .route(
            "/files/downloads/files/{path}",
            delete(delete_download_file),
        )
        .route("/files/incomplete/directories", get(incomplete_root))
        .route(
            "/files/incomplete/directories/{path}",
            get(incomplete_subdir).delete(delete_incomplete_dir),
        )
        .route(
            "/files/incomplete/files/{path}",
            delete(delete_incomplete_file),
        )
}

async fn download_root(
    _auth: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FilesystemDirectory>> {
    list(
        state.options.get().downloads.directory.as_str(),
        None,
        q.recursive,
    )
}
async fn incomplete_root(
    _auth: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FilesystemDirectory>> {
    list(
        state.options.get().downloads.incomplete_directory.as_str(),
        None,
        q.recursive,
    )
}
async fn download_subdir(
    _auth: Authenticated,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FilesystemDirectory>> {
    list(
        state.options.get().downloads.directory.as_str(),
        Some(&path),
        q.recursive,
    )
}
async fn incomplete_subdir(
    _auth: Authenticated,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FilesystemDirectory>> {
    list(
        state.options.get().downloads.incomplete_directory.as_str(),
        Some(&path),
        q.recursive,
    )
}
async fn delete_download_dir(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
) -> ApiResult<StatusCode> {
    remove(&state.options.get(), true, false, &path)
}
async fn delete_download_file(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
) -> ApiResult<StatusCode> {
    remove(&state.options.get(), true, true, &path)
}
async fn delete_incomplete_dir(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
) -> ApiResult<StatusCode> {
    remove(&state.options.get(), false, false, &path)
}
async fn delete_incomplete_file(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    RoutePath(path): RoutePath<String>,
) -> ApiResult<StatusCode> {
    remove(&state.options.get(), false, true, &path)
}

fn root_path(options: &Config, downloads: bool) -> PathBuf {
    PathBuf::from(if downloads {
        options.downloads.directory.as_str()
    } else {
        options.downloads.incomplete_directory.as_str()
    })
}

fn decoded_path(encoded: Option<&str>) -> ApiResult<PathBuf> {
    let Some(encoded) = encoded else {
        return Ok(PathBuf::new());
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded))
        .map_err(|_| ApiError::BadRequest("Path must be valid base64".into()))?;
    let text =
        String::from_utf8(raw).map_err(|_| ApiError::BadRequest("Path must be UTF-8".into()))?;
    let normalized = text.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::BadRequest("Path must be relative".into()));
    }
    Ok(path.to_path_buf())
}

fn confined(root: &Path, relative: &Path, allow_root: bool) -> ApiResult<PathBuf> {
    let canonical_root = root.canonicalize().map_err(map_io)?;
    let joined = canonical_root.join(relative);
    if !allow_root && joined == canonical_root {
        return Err(ApiError::Forbidden(
            "Operation on the root directory is not permitted".into(),
        ));
    }
    let canonical = joined.canonicalize().map_err(map_io)?;
    if !canonical.starts_with(&canonical_root) || (!allow_root && canonical == canonical_root) {
        return Err(ApiError::Forbidden(
            "Path escapes the configured directory".into(),
        ));
    }
    Ok(canonical)
}

fn map_io(error: io::Error) -> ApiError {
    match error.kind() {
        io::ErrorKind::NotFound => ApiError::NotFound(String::new()),
        io::ErrorKind::PermissionDenied => ApiError::Forbidden("Access denied".into()),
        _ => ApiError::Internal(error.into()),
    }
}

fn list(
    root: &str,
    encoded: Option<&str>,
    recursive: bool,
) -> ApiResult<Json<FilesystemDirectory>> {
    let relative = decoded_path(encoded)?;
    let root = PathBuf::from(root);
    let path = confined(&root, &relative, true)?;
    let result = directory(
        &path,
        &root.canonicalize().map_err(map_io)?,
        &relative,
        recursive,
    )?;
    Ok(Json(result))
}

fn directory(
    path: &Path,
    root: &Path,
    relative: &Path,
    recursive: bool,
) -> ApiResult<FilesystemDirectory> {
    let metadata = fs::metadata(path).map_err(map_io)?;
    if !metadata.is_dir() {
        return Err(ApiError::NotFound(String::new()));
    }
    let mut files = Vec::new();
    let mut directories = Vec::new();
    for entry in fs::read_dir(path).map_err(map_io)? {
        let entry = entry.map_err(map_io)?;
        let file_type = entry.file_type().map_err(map_io)?;
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_relative = relative.join(&name);
        let child_path = entry.path();
        let metadata = entry.metadata().map_err(map_io)?;
        if file_type.is_dir() {
            let child = if recursive {
                Some(directory(&child_path, root, &child_relative, true)?)
            } else {
                None
            };
            directories.push(FilesystemDirectory {
                name: name.clone(),
                full_name: child_relative.to_string_lossy().replace('\\', "/"),
                attributes: Default::default(),
                created_at: date(metadata.created()),
                modified_at: date(metadata.modified()),
                files: child.as_ref().map(|d| d.files.clone().unwrap_or_default()),
                directories: child.map(|d| d.directories.unwrap_or_default()),
            });
        } else if file_type.is_file() {
            files.push(FilesystemFile {
                name: name.clone(),
                full_name: name,
                length: metadata.len(),
                attributes: Default::default(),
                created_at: date(metadata.created()),
                modified_at: date(metadata.modified()),
            });
        }
    }
    let _ = root;
    Ok(FilesystemDirectory {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        full_name: relative.to_string_lossy().replace('\\', "/"),
        attributes: Default::default(),
        created_at: date(metadata.created()),
        modified_at: date(metadata.modified()),
        files: Some(files),
        directories: Some(directories),
    })
}

fn date(value: io::Result<std::time::SystemTime>) -> DateTime<Utc> {
    value
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now)
}

fn remove(
    options: &Config,
    downloads: bool,
    is_file: bool,
    encoded: &str,
) -> ApiResult<StatusCode> {
    let relative = decoded_path(Some(encoded))?;
    if relative.as_os_str().is_empty() {
        return Err(ApiError::Forbidden(
            "Operation on the root directory is not permitted".into(),
        ));
    }
    let root = root_path(options, downloads);
    let path = confined(&root, &relative, false)?;
    let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
    if metadata.file_type().is_symlink()
        || (is_file && !metadata.is_file())
        || (!is_file && !metadata.is_dir())
    {
        return Err(ApiError::BadRequest(
            "Path has the wrong filesystem type".into(),
        ));
    }
    if is_file {
        fs::remove_file(path).map_err(map_io)?;
    } else {
        fs::remove_dir_all(path).map_err(map_io)?;
    }
    Ok(StatusCode::NO_CONTENT)
}
