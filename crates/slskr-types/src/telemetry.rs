//! Live transfer metrics (`GET /api/metrics`, `metrics` event) and transfer reports
//! (`/api/reports/transfers/...`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transfers::{TransferDirection, TransferState};

/// Live transfer metrics, sent about once a second as the `metrics` event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CurrentMetrics {
    pub downloads: DirectionMetrics,
    pub uploads: DirectionMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectionMetrics {
    pub in_progress: InProgressMetrics,
    pub queued: QueuedMetrics,
    pub completed: CompletedMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InProgressMetrics {
    pub files: u64,
    pub users: u64,
    /// Bytes per second, per transfer.
    pub average_speed: f64,
    /// Bytes per second, all transfers.
    pub total_speed: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueuedMetrics {
    pub files: u64,
    pub users: u64,
    pub bytes: u64,
}

/// Totals since the daemon started.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompletedMetrics {
    pub succeeded: u64,
    pub failed: u64,
    pub bytes: u64,
}

/// Aggregate over a set of completed transfers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferSummary {
    /// Set in leaderboards.
    pub username: Option<String>,
    pub total_bytes: u64,
    pub count: u64,
    pub distinct_users: u64,
    /// Bytes per second.
    pub average_speed: f64,
    /// From enqueued to started.
    pub average_wait_ms: u64,
    /// From started to ended.
    pub average_duration_ms: u64,
}

/// Final state → summary.
pub type TransferSummaryByState = BTreeMap<TransferState, TransferSummary>;

/// `GET /api/reports/transfers/summary`: direction → final state → summary.
pub type TransferSummaryReport = BTreeMap<TransferDirection, TransferSummaryByState>;

/// `GET /api/reports/transfers/histogram`: bucket start → direction → state → summary.
pub type TransferHistogram = BTreeMap<DateTime<Utc>, TransferSummaryReport>;

/// `GET /api/reports/transfers/leaderboard`.
pub type TransferLeaderboard = Vec<TransferSummary>;

/// How often an error occurred (`GET /api/reports/transfers/errors/pareto`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferErrorSummary {
    pub error: Option<String>,
    pub count: u64,
    pub distinct_users: u64,
}

/// A failed transfer (`GET /api/reports/transfers/errors`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferErrorDetail {
    pub id: String,
    pub username: String,
    pub direction: TransferDirection,
    pub filename: String,
    pub size: u64,
    pub state: TransferState,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub enqueued_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub average_speed: f64,
    #[serde(default)]
    pub error: Option<String>,
}

/// How often files from a directory were uploaded (`GET /api/reports/transfers/directories`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferDirectorySummary {
    pub directory: Option<String>,
    pub count: u64,
    pub distinct_users: u64,
}

/// One direction of a user report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserDirectionTransferSummary {
    pub summary: TransferSummaryByState,
    pub statistics: UserDirectionTransferStatistics,
    pub errors: Vec<TransferErrorSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserDirectionTransferStatistics {
    pub total: u64,
    pub successful: u64,
    /// Errored, timed out, rejected or aborted.
    pub errored: u64,
    pub cancelled: u64,
}

/// `GET /api/reports/transfers/users/{username}`: direction → report.
pub type UserTransferReport = BTreeMap<TransferDirection, UserDirectionTransferSummary>;

/// Leaderboard sort column (`sort_by`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderboardSortBy {
    #[default]
    Count,
    TotalBytes,
    AverageSpeed,
}

/// Sort order (`sort_order`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_keys_are_enum_names() {
        let mut report = TransferSummaryReport::new();
        report
            .entry(TransferDirection::Upload)
            .or_default()
            .insert(TransferState::Succeeded, TransferSummary::default());
        let value = serde_json::to_value(&report).unwrap();
        assert!(
            value["upload"]["succeeded"]
                .get("average_wait_ms")
                .is_some()
        );
        assert_eq!(
            serde_json::from_value::<TransferSummaryReport>(value).unwrap(),
            report
        );

        let mut histogram = TransferHistogram::new();
        histogram.insert(DateTime::<Utc>::default(), report);
        let text = serde_json::to_string(&histogram).unwrap();
        assert!(text.starts_with(r#"{"1970-01-01T00:00:00Z":"#));
        assert_eq!(
            serde_json::from_str::<TransferHistogram>(&text).unwrap(),
            histogram
        );
    }
}
