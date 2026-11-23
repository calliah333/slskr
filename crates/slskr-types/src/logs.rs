//! Log records (`GET /api/logs`, `log` events) and log files (`/api/logs/files`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A buffered log record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    /// The emitting module.
    pub context: Option<String>,
    pub sub_context: Option<String>,
    /// `trace`, `debug`, `info`, `warn` or `error`.
    pub level: Option<String>,
    pub message: Option<String>,
}

/// A log file (`GET /api/logs/files`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFile {
    pub name: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}
