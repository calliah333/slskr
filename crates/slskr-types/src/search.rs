//! Searches (`/api/searches`) and their live events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::users::File;

/// Where a search is in its life. `requested`, `queued` and `in_progress` are active; the
/// rest are final and say why the search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchState {
    #[default]
    Requested,
    Queued,
    InProgress,
    /// Ended without a more specific reason.
    Completed,
    /// The search ran for its whole timeout.
    TimedOut,
    Cancelled,
    ResponseLimitReached,
    FileLimitReached,
    Errored,
}

impl SearchState {
    /// The search ended.
    pub const fn is_complete(self) -> bool {
        !matches!(self, Self::Requested | Self::Queued | Self::InProgress)
    }

    /// The JSON name (`"timed_out"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::ResponseLimitReached => "response_limit_reached",
            Self::FileLimitReached => "file_limit_reached",
            Self::Errored => "errored",
        }
    }

    /// A human readable label (`"Timed out"`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Requested => "Requested",
            Self::Queued => "Queued",
            Self::InProgress => "In progress",
            Self::Completed => "Completed",
            Self::TimedOut => "Timed out",
            Self::Cancelled => "Cancelled",
            Self::ResponseLimitReached => "Response limit reached",
            Self::FileLimitReached => "File limit reached",
            Self::Errored => "Errored",
        }
    }
}

/// A search, without its responses (`GET /api/searches/{id}/responses` has them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Search {
    /// UUID.
    pub id: String,
    pub query: String,
    pub state: SearchState,
    /// The Soulseek search token.
    pub token: u32,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub response_count: u32,
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub locked_file_count: u32,
}

impl Search {
    /// Sets `state`, and `ended_at` (to `now`) when the search ends.
    pub fn set_state(&mut self, state: SearchState, now: DateTime<Utc>) {
        self.state = state;
        if state.is_complete() && self.ended_at.is_none() {
            self.ended_at = Some(now);
        }
    }
}

/// One user's response to a search.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResponse {
    pub username: String,
    pub token: u32,
    pub has_free_upload_slot: bool,
    /// Bytes per second.
    pub upload_speed: u32,
    pub queue_length: u32,
    /// Full remote filenames.
    pub files: Vec<File>,
    /// Files the user shares only with some users; they can not be downloaded.
    pub locked_files: Vec<File>,
}

/// `POST /api/searches` body. Absent options take the defaults shown.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    /// A UUID chosen by the caller; generated when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Search terms; `-term` excludes.
    pub query: String,
    /// At least 5; default 15000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// Default 250.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_limit: Option<u32>,
    /// Default 25000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_limit: Option<u32>,
    /// Apply the minimum and maximum filters below. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_responses: Option<bool>,
    /// Default 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_response_file_count: Option<u32>,
    /// Bytes per second; default 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_peer_upload_speed: Option<u32>,
    /// Default 2147483647.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_peer_queue_length: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_shape() {
        let search: Search = serde_json::from_value(serde_json::json!({
            "id": "0b6f1b0e-6c7e-4bd8-8a3e-5f4c2d1a0b9c",
            "query": "foo bar",
            "state": "timed_out",
            "token": 42,
            "started_at": "2024-05-01T12:00:00Z",
            "ended_at": "2024-05-01T12:00:15Z",
            "response_count": 2,
            "file_count": 10,
            "locked_file_count": 1
        }))
        .unwrap();
        assert!(search.state.is_complete());
        let value = serde_json::to_value(&search).unwrap();
        assert_eq!(value["state"], "timed_out");
        assert_eq!(value["ended_at"], "2024-05-01T12:00:15Z");
    }

    #[test]
    fn request_omits_defaults() {
        let request = SearchRequest {
            id: Some("x".into()),
            query: "foo".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"id":"x","query":"foo"}"#
        );
    }
}
