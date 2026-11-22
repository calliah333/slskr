//! Log records (`GET /api/v0/logs`, logs hub).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Logs hub method names.
pub mod hub_methods {
    pub const BUFFER: &str = "BUFFER";
    pub const LOG: &str = "LOG";
}

/// A buffered log line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub context: Option<String>,
    pub sub_context: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_fields() {
        let v = serde_json::to_value(LogRecord::default()).unwrap();
        assert!(v.get("subContext").is_some());
        assert!(v.get("timestamp").is_some());
    }
}
