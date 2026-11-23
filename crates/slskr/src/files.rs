//! Browsing and deleting below the downloads and incomplete directories
//! (`/api/files/{root}`).

use std::{
    fs, io,
    path::{Component, Path as FsPath, PathBuf},
};

use axum::{extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use slskr_types::{
    config::Config,
    files::{FilesystemDirectory, FilesystemFile},
};

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, RequireReadWrite},
    http::{ApiError, ApiResult, Json, Path, Query, Routes},
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
    /// Relative to the root, `/`-separated; the root itself when absent.
    #[serde(default)]
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
struct DeleteQuery {
    path: String,
}

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/files/{root}", list)
        .delete("/api/files/{root}", remove)
}

/// `downloads` (`downloads.directory`) or `incomplete` (`downloads.incomplete_directory`).
fn root_path(options: &Config, root: &str) -> ApiResult<PathBuf> {
    match root {
        "downloads" => Ok(PathBuf::from(&options.downloads.directory)),
        "incomplete" => Ok(PathBuf::from(&options.downloads.incomplete_directory)),
        _ => Err(ApiError::NotFound(format!(
            "Unknown root '{root}'; expected downloads or incomplete"
        ))),
    }
}

async fn list(
    _auth: Authenticated,
    State(state): State<AppState>,
    Path(root): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<FilesystemDirectory>> {
    let root = root_path(&state.options.get(), &root)?;
    let relative = relative_path(&query.path)?;
    let path = confined(&root, &relative, true)?;
    Ok(Json(directory(&path, &relative, query.recursive)?))
}

/// Deletes a file, or a directory with everything in it (204).
async fn remove(
    _auth: RequireReadWrite,
    State(state): State<AppState>,
    Path(root): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let root = root_path(&state.options.get(), &root)?;
    let relative = relative_path(&query.path)?;
    if relative.as_os_str().is_empty() {
        return Err(ApiError::Forbidden(
            "Operation on the root directory is not permitted".into(),
        ));
    }
    let path = confined(&root, &relative, false)?;
    let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
    if metadata.is_file() {
        fs::remove_file(path).map_err(map_io)?;
    } else if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(map_io)?;
    } else {
        return Err(ApiError::BadRequest(
            "Path is neither a file nor a directory".into(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn relative_path(path: &str) -> ApiResult<PathBuf> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_matches('/');
    let path = FsPath::new(normalized);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::BadRequest(
            "path must be relative, without . or .. components".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn confined(root: &FsPath, relative: &FsPath, allow_root: bool) -> ApiResult<PathBuf> {
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
        io::ErrorKind::NotFound => ApiError::not_found(),
        io::ErrorKind::PermissionDenied => ApiError::Forbidden("Access denied".into()),
        _ => ApiError::Internal(error.into()),
    }
}

fn display(path: &FsPath) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn directory(path: &FsPath, relative: &FsPath, recursive: bool) -> ApiResult<FilesystemDirectory> {
    let metadata = fs::metadata(path).map_err(map_io)?;
    if !metadata.is_dir() {
        return Err(ApiError::NotFound("Not a directory".into()));
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
        let metadata = entry.metadata().map_err(map_io)?;
        if file_type.is_dir() {
            directories.push(if recursive {
                directory(&entry.path(), &child_relative, true)?
            } else {
                FilesystemDirectory {
                    name,
                    path: display(&child_relative),
                    created_at: date(metadata.created()),
                    modified_at: date(metadata.modified()),
                    files: Vec::new(),
                    directories: Vec::new(),
                }
            });
        } else if file_type.is_file() {
            files.push(FilesystemFile {
                name,
                path: display(&child_relative),
                size: metadata.len(),
                created_at: date(metadata.created()),
                modified_at: date(metadata.modified()),
            });
        }
    }
    Ok(FilesystemDirectory {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: display(relative),
        created_at: date(metadata.created()),
        modified_at: date(metadata.modified()),
        files,
        directories,
    })
}

fn date(value: io::Result<std::time::SystemTime>) -> DateTime<Utc> {
    value
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now)
}
