//! Process logging, bounded live buffer, and log-file HTTP endpoints.

use std::{
    cell::Cell,
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use chrono::{DateTime, Utc};
use serde_json::json;
use slskr_api::{
    files::FilesystemFile,
    logs::{LogRecord, hub_methods},
    options::Options,
};
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{EnvFilter, Layer, fmt::MakeWriter, layer::Context, prelude::*};

use crate::{
    app::{AppState, Paths},
    auth::RequireAdmin,
    http::{ApiError, ApiResult},
    signalr::Hub,
};

const LOG_BUFFER_CAPACITY: usize = 1000;

thread_local! {
    // The hub can log while dropping a slow subscriber, with its client lock held.
    static BROADCASTING_LOG: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone)]
pub struct LogBuffer(Arc<Inner>);

struct Inner {
    records: Mutex<VecDeque<LogRecord>>,
    hub: Mutex<Option<Hub>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self(Arc::new(Inner {
            records: Mutex::new(VecDeque::with_capacity(LOG_BUFFER_CAPACITY)),
            hub: Mutex::new(None),
        }))
    }
}

impl LogBuffer {
    pub fn attach_hub(&self, hub: Hub) {
        *lock(&self.0.hub) = Some(hub.clone());
        let buffer = Arc::downgrade(&self.0);
        hub.on_connected(move || {
            let snapshot = buffer
                .upgrade()
                .map_or_else(|| json!([]), |inner| json!(*lock(&inner.records)));
            vec![(hub_methods::BUFFER.to_owned(), vec![snapshot])]
        });
    }

    pub fn snapshot(&self) -> Vec<LogRecord> {
        lock(&self.0.records).iter().cloned().collect()
    }

    fn push_line(&self, record: LogRecord) {
        BROADCASTING_LOG.with(|broadcasting| {
            let hub = if broadcasting.get() {
                None
            } else {
                lock(&self.0.hub)
                    .clone()
                    .filter(|hub| hub.connection_count() != 0)
            };
            let payload = hub.as_ref().map(|_| json!(record));
            {
                let mut records = lock(&self.0.records);
                if records.len() == LOG_BUFFER_CAPACITY {
                    records.pop_front();
                }
                records.push_back(record);
            }
            if let (Some(hub), Some(payload)) = (hub, payload) {
                struct Reset<'a>(&'a Cell<bool>);
                impl Drop for Reset<'_> {
                    fn drop(&mut self) {
                        self.0.set(false);
                    }
                }

                broadcasting.set(true);
                let _reset = Reset(broadcasting);
                hub.broadcast(hub_methods::LOG, vec![payload]);
            }
        });
    }
}

struct BufferLayer(LogBuffer);

#[derive(Default)]
struct EventMessage {
    message: String,
    fields: String,
}

impl Visit for EventMessage {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = match *metadata.level() {
            Level::TRACE => "Verbose",
            Level::DEBUG => "Debug",
            Level::INFO => "Information",
            Level::WARN => "Warning",
            Level::ERROR => "Error",
        };
        let mut visitor = EventMessage::default();
        event.record(&mut visitor);
        let message = if visitor.message.is_empty() {
            visitor.fields
        } else {
            if !visitor.fields.is_empty() {
                visitor.message.push(' ');
                visitor.message.push_str(&visitor.fields);
            }
            visitor.message
        };
        self.0.push_line(LogRecord {
            timestamp: Utc::now(),
            context: Some(metadata.target().to_owned()),
            sub_context: None,
            level: Some(level.to_owned()),
            message: Some(message),
        });
    }
}

struct FileWriter(Mutex<File>);
struct LockedFile<'a>(MutexGuard<'a, File>);

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = LockedFile<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LockedFile(lock(&self.0))
    }
}

impl Write for LockedFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Initializes tracing and returns the shared live log buffer.
pub fn init(options: &Options, paths: &Paths) -> anyhow::Result<LogBuffer> {
    let buffer = LogBuffer::default();
    let file = if options.logger.no_disk {
        None
    } else {
        std::fs::create_dir_all(&paths.logs_dir)?;
        let path = paths
            .logs_dir
            .join(format!("slskr-{}.log", Utc::now().format("%Y%m%d")));
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(FileWriter(Mutex::new(
                    OpenOptions::new().create(true).append(true).open(path)?,
                )))
                .with_ansi(!options.logger.no_color),
        )
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if options.debug { "debug" } else { "info" }));
    tracing_subscriber::registry()
        .with(filter)
        .with(BufferLayer(buffer.clone()))
        .with(file)
        .try_init()?;
    Ok(buffer)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/logs/live", get(live))
        .route("/logs/files", get(list_files))
        .route("/logs/files/{filename}", get(get_file))
}

async fn live(_admin: RequireAdmin, State(state): State<AppState>) -> Json<Vec<LogRecord>> {
    Json(state.core.logs.snapshot())
}

async fn list_files(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<FilesystemFile>>> {
    let dir = &state.core.paths.logs_dir;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ApiError::NotFound(String::new()));
        }
        Err(e) => return Err(ApiError::Internal(e.into())),
    };
    let mut files = Vec::new();
    for entry in entries {
        // Like upstream's IgnoreInaccessible, skip entries that cannot be inspected.
        let Ok(entry) = entry else { continue };
        let Ok(ty) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        // Upstream skips hidden and reparse-point (symlink) entries.
        if !ty.is_file()
            || ty.is_symlink()
            || name.starts_with('.')
            || !name.to_ascii_lowercase().ends_with(".log")
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        files.push(FilesystemFile {
            full_name: name.clone(),
            name,
            length: metadata.len(),
            attributes: Default::default(),
            created_at: file_date(metadata.created()),
            modified_at: file_date(metadata.modified()),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(files))
}

fn file_date(value: io::Result<std::time::SystemTime>) -> DateTime<Utc> {
    value
        .ok()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now)
}

async fn get_file(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> ApiResult<axum::response::Response> {
    let file = safe_log_path(&state.core.paths.logs_dir, &filename)?;
    let data = std::fs::read(file).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            ApiError::NotFound(String::new())
        } else {
            ApiError::Internal(e.into())
        }
    })?;
    let body = String::from_utf8_lossy(&data).into_owned();
    // Return plain text using an explicit response to preserve the log file's media type.
    let response = axum::http::Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(body)
        .map_err(anyhow::Error::from)?;
    Ok(response.into_response())
}

fn safe_log_path(root: &PathBuf, filename: &str) -> ApiResult<PathBuf> {
    if filename.trim().is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename == "."
        || filename == ".."
    {
        return Err(ApiError::BadRequest(
            "Filename must be a single filename".into(),
        ));
    }
    if !filename.to_ascii_lowercase().ends_with(".log") {
        return Err(ApiError::Forbidden(
            "Only .log files may be retrieved".into(),
        ));
    }
    let root = root.canonicalize().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            ApiError::NotFound(String::new())
        } else {
            ApiError::Internal(e.into())
        }
    })?;
    let path = root.join(filename);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            ApiError::NotFound(String::new())
        } else {
            ApiError::Internal(e.into())
        }
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(ApiError::Forbidden("Log path is not a regular file".into()));
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| ApiError::Internal(e.into()))?;
    if canonical.parent() != Some(root.as_path()) {
        return Err(ApiError::Forbidden("Log path escapes log directory".into()));
    }
    Ok(canonical)
}
