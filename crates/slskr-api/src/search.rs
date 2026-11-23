//! Search API models (`/api/v0/searches`) and search event payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transfers::time;

crate::slskd_flags! {
    /// Soulseek.NET `SearchStates`.
    SearchStates {
        NONE = 0 => "None",
        REQUESTED = 1 => "Requested",
        IN_PROGRESS = 2 => "InProgress",
        COMPLETED = 4 => "Completed",
        CANCELLED = 8 => "Cancelled",
        TIMED_OUT = 16 => "TimedOut",
        RESPONSE_LIMIT_REACHED = 32 => "ResponseLimitReached",
        FILE_LIMIT_REACHED = 64 => "FileLimitReached",
        ERRORED = 128 => "Errored",
        QUEUED = 256 => "Queued",
    }
}

/// slskd `Search.Search` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Search {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_datetime"
    )]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub file_count: u32,
    pub id: String,
    /// Derived: `state` contains `Completed`.
    #[serde(default)]
    pub is_complete: bool,
    #[serde(default)]
    pub locked_file_count: u32,
    #[serde(default)]
    pub response_count: u32,
    /// HTTP emits `[]` unless responses were requested (`includeResponses=true`);
    /// `search_*` events emit `null`.
    #[serde(default)]
    pub responses: Option<Vec<SearchResponse>>,
    pub search_text: String,
    #[serde(with = "time::datetime")]
    pub started_at: DateTime<Utc>,
    pub state: SearchStates,
    #[serde(default)]
    pub token: u32,
}

impl Search {
    /// Sets `state` and keeps `isComplete` consistent with it.
    pub fn set_state(&mut self, state: SearchStates) {
        self.state = state;
        self.is_complete = state.contains(SearchStates::COMPLETED);
    }
}

/// slskd `Search.File` (search response file).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub extension: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_variable_bit_rate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub is_locked: bool,
}

impl From<crate::users::File> for File {
    fn from(file: crate::users::File) -> Self {
        Self {
            bit_depth: file.bit_depth,
            bit_rate: file.bit_rate,
            code: file.code,
            extension: file.extension,
            filename: file.filename,
            is_variable_bit_rate: file.is_variable_bit_rate,
            length: file.length,
            sample_rate: file.sample_rate,
            size: file.size,
            is_locked: false,
        }
    }
}

/// slskd `Search.Response`, as returned by `GET /searches/{id}/responses`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub locked_file_count: u32,
    #[serde(default)]
    pub locked_files: Vec<File>,
    #[serde(default)]
    pub queue_length: u64,
    #[serde(default)]
    pub token: u32,
    #[serde(default)]
    pub upload_speed: u32,
    pub username: String,
}

impl From<SoulseekSearchResponse> for SearchResponse {
    fn from(response: SoulseekSearchResponse) -> Self {
        Self {
            file_count: response.file_count,
            files: response.files.into_iter().map(File::from).collect(),
            has_free_upload_slot: response.has_free_upload_slot,
            locked_file_count: response.locked_file_count,
            locked_files: response.locked_files.into_iter().map(File::from).collect(),
            queue_length: u64::from(response.queue_length),
            token: response.token,
            upload_speed: response.upload_speed,
            username: response.username,
        }
    }
}

/// Soulseek.NET `SearchResponse`, as sent in `search_responses` events.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoulseekSearchResponse {
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub files: Vec<crate::users::File>,
    #[serde(default)]
    pub has_free_upload_slot: bool,
    #[serde(default)]
    pub locked_file_count: u32,
    #[serde(default)]
    pub locked_files: Vec<crate::users::File>,
    #[serde(default)]
    pub queue_length: u32,
    #[serde(default)]
    pub token: u32,
    #[serde(default)]
    pub upload_speed: u32,
    pub username: String,
}

/// Body of `POST /searches`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_responses: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_peer_queue_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_peer_upload_speed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_response_file_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_limit: Option<u32>,
    #[serde(default)]
    pub search_text: String,
    /// Milliseconds; slskd requires at least 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_timeout: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_states_round_trip() {
        let state = SearchStates::COMPLETED | SearchStates::TIMED_OUT;
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#""Completed, TimedOut""#
        );
        assert_eq!(
            serde_json::from_str::<SearchStates>(r#""Completed, TimedOut""#).unwrap(),
            state
        );
        assert_eq!(
            serde_json::to_string(
                &(SearchStates::COMPLETED | SearchStates::RESPONSE_LIMIT_REACHED)
            )
            .unwrap(),
            r#""Completed, ResponseLimitReached""#
        );
        assert_eq!(
            serde_json::from_str::<SearchStates>(r#""InProgress""#).unwrap(),
            SearchStates::IN_PROGRESS
        );
        assert!(state.contains(SearchStates::COMPLETED));
    }

    #[test]
    fn search_json_shape() {
        let json = r#"{
            "endedAt": "2024-05-01T12:00:15.000Z",
            "fileCount": 10,
            "id": "0b6f1b0e-6c7e-4bd8-8a3e-5f4c2d1a0b9c",
            "isComplete": true,
            "lockedFileCount": 1,
            "responseCount": 2,
            "searchText": "foo bar",
            "startedAt": "2024-05-01T14:00:00.1234567+02:00",
            "state": "Completed, TimedOut",
            "token": 42
        }"#;
        let search: Search = serde_json::from_str(json).unwrap();
        assert!(search.responses.is_none());
        assert_eq!(
            search.state,
            SearchStates::COMPLETED | SearchStates::TIMED_OUT
        );

        let value = serde_json::to_value(&search).unwrap();
        for key in [
            "endedAt",
            "fileCount",
            "id",
            "isComplete",
            "lockedFileCount",
            "responseCount",
            "searchText",
            "startedAt",
            "state",
            "token",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert!(value["responses"].is_null());
    }

    #[test]
    fn response_and_request_shapes() {
        let response: SearchResponse = serde_json::from_str(
            r#"{"fileCount":1,"files":[{"code":1,"extension":"flac","filename":"a.flac","size":5,"bitDepth":16,"sampleRate":44100,"length":200,"isLocked":false}],
                "hasFreeUploadSlot":true,"lockedFileCount":0,"lockedFiles":[],"queueLength":0,"token":7,"uploadSpeed":1000,"username":"u"}"#,
        )
        .unwrap();
        assert_eq!(response.files[0].sample_rate, Some(44100));
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["hasFreeUploadSlot"], true);
        assert_eq!(value["files"][0]["isLocked"], false);
        assert!(value["files"][0].get("bitRate").is_none());

        let request = SearchRequest {
            id: Some("x".into()),
            search_text: "foo".into(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"id":"x","searchText":"foo"}"#
        );
    }
}
