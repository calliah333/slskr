//! Telemetry DTOs: MetricsController, MetricsHub and ReportsController responses.
//!
//! Map keys that are slskd enums (`TransferDirection`, `TransferStates`) are their string
//! names (`"Upload"`, `"Completed, Succeeded"`), exactly as System.Text.Json writes them.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One Prometheus metric family (`GET /telemetry/metrics` with `Accept: application/json`, `/kpis`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrometheusMetric {
    pub name: Option<String>,
    pub help: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub sum: Option<f64>,
    pub count: Option<f64>,
    pub samples: Option<Vec<PrometheusMetricSample>>,
    pub buckets: Option<BTreeMap<String, PrometheusMetricSample>>,
    pub quantiles: Option<BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrometheusMetricSample {
    pub value: Option<f64>,
    pub labels: Option<BTreeMap<String, String>>,
}

/// Metrics keyed by family name.
pub type PrometheusMetrics = BTreeMap<String, PrometheusMetric>;

/// Metrics hub method names.
pub mod hub_methods {
    pub const UPDATE: &str = "Update";
}

/// Live transfer metrics pushed by the metrics hub (`MetricsService.Current()`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CurrentMetrics {
    pub downloads: DirectionMetrics,
    pub uploads: DirectionMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DirectionMetrics {
    pub in_progress: InProgressMetrics,
    pub queued: QueuedMetrics,
    pub completed: CompletedMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InProgressMetrics {
    pub files: f64,
    pub users: f64,
    pub average_speed: f64,
    pub total_speed: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueuedMetrics {
    pub files: f64,
    pub users: f64,
    pub bytes: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompletedMetrics {
    pub succeeded: f64,
    pub failed: f64,
    pub bytes: f64,
}

/// Aggregate over a set of transfers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferSummary {
    pub username: Option<String>,
    pub total_bytes: i64,
    pub count: i64,
    pub distinct_users: i64,
    pub average_speed: f64,
    pub average_wait: f64,
    pub average_duration: f64,
}

/// `TransferStates` name → summary.
pub type TransferSummaryByState = BTreeMap<String, TransferSummary>;

/// `GET transfers/summary`: `TransferDirection` name → state → summary.
pub type TransferSummaryReport = BTreeMap<String, TransferSummaryByState>;

/// `GET transfers/histogram`: bucket start → direction → state → summary.
pub type TransferHistogram = BTreeMap<DateTime<Utc>, TransferSummaryReport>;

/// `GET transfers/leaderboard`.
pub type TransferLeaderboard = Vec<TransferSummary>;

/// Exception frequency (`GET transfers/exceptions/pareto`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferExceptionSummary {
    pub exception: Option<String>,
    pub count: i64,
    pub distinct_users: i64,
}

/// A failed transfer (`GET transfers/exceptions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferExceptionDetail {
    pub id: Option<String>,
    pub username: Option<String>,
    /// `TransferDirection` name.
    pub direction: String,
    pub filename: Option<String>,
    pub size: i64,
    /// `TransferStates` flags string.
    pub state: String,
    pub requested_at: DateTime<Utc>,
    pub enqueued_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub bytes_transferred: i64,
    pub average_speed: f64,
    pub exception: Option<String>,
}

/// Directory frequency (`GET transfers/directories`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferDirectorySummary {
    pub directory: Option<String>,
    pub count: i64,
    pub distinct_users: i64,
}

/// Per-direction user report (`GET transfers/users/{username}` values).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserDirectionTransferSummary {
    pub summary: TransferSummaryByState,
    pub statistics: UserDirectionTransferStatistics,
    pub exceptions: Vec<TransferExceptionSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserDirectionTransferStatistics {
    pub total: i64,
    pub successful: i64,
    pub errored: i64,
    pub cancelled: i64,
}

/// `GET transfers/users/{username}`: direction name → report.
pub type UserTransferReport = BTreeMap<String, UserDirectionTransferSummary>;

/// Leaderboard sort column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LeaderboardSortBy {
    #[default]
    Count,
    TotalBytes,
    AverageSpeed,
}

impl LeaderboardSortBy {
    /// Case-sensitive parse, like the controller's `Enum.TryParse`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Count" => Some(Self::Count),
            "TotalBytes" => Some(Self::TotalBytes),
            "AverageSpeed" => Some(Self::AverageSpeed),
            _ => None,
        }
    }
}

/// Sort order.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    ASC,
    #[default]
    DESC,
}

impl SortOrder {
    /// Case-sensitive parse, like the controller's `Enum.TryParse`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ASC" => Some(Self::ASC),
            "DESC" => Some(Self::DESC),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_json() {
        let v = serde_json::to_value(CurrentMetrics::default()).unwrap();
        assert!(v["downloads"]["inProgress"].get("averageSpeed").is_some());
        let v = serde_json::to_value(TransferExceptionDetail::default()).unwrap();
        assert!(v.get("requestedAt").is_some());
        assert!(v.get("bytesTransferred").is_some());
        let v = serde_json::to_value(PrometheusMetric::default()).unwrap();
        assert!(v.get("type").is_some());
        let v = serde_json::to_value(UserDirectionTransferSummary::default()).unwrap();
        assert!(v["statistics"].get("successful").is_some());

        let mut h = TransferHistogram::new();
        h.insert(DateTime::<Utc>::default(), TransferSummaryReport::new());
        let s = serde_json::to_string(&h).unwrap();
        let back: TransferHistogram = serde_json::from_str(&s).unwrap();
        assert_eq!(back, h);
    }
}
