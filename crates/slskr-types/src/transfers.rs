//! Transfers (`/api/transfers/...`) and download batches (`/api/batches`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a transfer is a download or an upload.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    #[default]
    Download,
    Upload,
}

impl TransferDirection {
    /// The collection path segment: `downloads` or `uploads`.
    pub const fn collection(self) -> &'static str {
        match self {
            Self::Download => "downloads",
            Self::Upload => "uploads",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Upload => "upload",
        }
    }
}

/// Where a transfer is in its life. The first five states are active; the rest are final.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Waiting to be negotiated with the peer.
    #[default]
    Requested,
    /// Waiting in this client's queue (a retry delay, a free slot, or an upload slot).
    QueuedLocally,
    /// Waiting in the peer's upload queue.
    QueuedRemotely,
    /// Connecting to the peer for the file data.
    Initializing,
    InProgress,
    Succeeded,
    Cancelled,
    TimedOut,
    Errored,
    /// The peer refused the transfer.
    Rejected,
    /// Stopped by this client (for example a size mismatch or a shutdown).
    Aborted,
}

impl TransferState {
    pub const ALL: [Self; 11] = [
        Self::Requested,
        Self::QueuedLocally,
        Self::QueuedRemotely,
        Self::Initializing,
        Self::InProgress,
        Self::Succeeded,
        Self::Cancelled,
        Self::TimedOut,
        Self::Errored,
        Self::Rejected,
        Self::Aborted,
    ];

    /// The transfer ended (successfully or not).
    pub const fn is_complete(self) -> bool {
        !matches!(
            self,
            Self::Requested
                | Self::QueuedLocally
                | Self::QueuedRemotely
                | Self::Initializing
                | Self::InProgress
        )
    }

    pub const fn is_queued(self) -> bool {
        matches!(self, Self::QueuedLocally | Self::QueuedRemotely)
    }

    /// Ended without success.
    pub const fn is_failed(self) -> bool {
        self.is_complete() && !matches!(self, Self::Succeeded)
    }

    /// The JSON name (`"in_progress"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::QueuedLocally => "queued_locally",
            Self::QueuedRemotely => "queued_remotely",
            Self::Initializing => "initializing",
            Self::InProgress => "in_progress",
            Self::Succeeded => "succeeded",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Errored => "errored",
            Self::Rejected => "rejected",
            Self::Aborted => "aborted",
        }
    }

    /// A human readable label (`"In progress"`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Requested => "Requested",
            Self::QueuedLocally => "Queued locally",
            Self::QueuedRemotely => "Queued remotely",
            Self::Initializing => "Initializing",
            Self::InProgress => "In progress",
            Self::Succeeded => "Succeeded",
            Self::Cancelled => "Cancelled",
            Self::TimedOut => "Timed out",
            Self::Errored => "Errored",
            Self::Rejected => "Rejected",
            Self::Aborted => "Aborted",
        }
    }
}

/// A download or upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    pub id: String,
    /// The batch this download belongs to.
    #[serde(default)]
    pub batch_id: Option<String>,
    pub username: String,
    pub direction: TransferDirection,
    /// The peer's (remote) filename.
    pub filename: String,
    /// Folder downloads: the remote folder the file was enqueued under. The file is
    /// saved as `<downloads>/<last component of root>/<path relative to root>`.
    #[serde(default)]
    pub root: Option<String>,
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
    /// Bytes per second.
    #[serde(default)]
    pub average_speed: f64,
    /// Position in the peer's queue (downloads) or ours (uploads), when known.
    #[serde(default)]
    pub place_in_queue: Option<u32>,
    /// Why the transfer failed.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// When a failed download is retried.
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Removed from the transfer list (kept for reports).
    #[serde(default)]
    pub removed: bool,
    /// Derived: `size - bytes_transferred`.
    #[serde(default)]
    pub bytes_remaining: u64,
    /// Derived: `(ended_at or now) - started_at`.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    /// Derived: `bytes_transferred / size * 100`.
    #[serde(default)]
    pub percent_complete: f64,
    /// Derived: `bytes_remaining / average_speed`.
    #[serde(default)]
    pub remaining_ms: Option<u64>,
}

impl Transfer {
    /// Recomputes the derived fields.
    pub fn refresh_derived(&mut self, now: DateTime<Utc>) {
        self.bytes_remaining = self.size.saturating_sub(self.bytes_transferred);
        self.elapsed_ms = self.started_at.map(|started| {
            u64::try_from((self.ended_at.unwrap_or(now) - started).num_milliseconds()).unwrap_or(0)
        });
        self.percent_complete = if self.size == 0 {
            0.0
        } else {
            self.bytes_transferred as f64 / self.size as f64 * 100.0
        };
        self.remaining_ms = (self.average_speed > 0.0)
            .then(|| (self.bytes_remaining as f64 / self.average_speed * 1000.0) as u64);
    }
}

/// A file to download: `POST /api/transfers/downloads` and `POST /api/batches`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFile {
    /// The peer's filename, as in browse or search results.
    pub filename: String,
    pub size: u64,
}

/// `POST /api/transfers/downloads` body.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnqueueDownloads {
    pub username: String,
    pub files: Vec<DownloadFile>,
}

/// `POST /api/transfers/downloads` body as the daemon reads it: the fields of
/// [`EnqueueDownloads`] (flattened, so the JSON is `{username, files, root?}`) plus an
/// optional `root`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnqueueDownloadsRequest {
    #[serde(flatten)]
    pub downloads: EnqueueDownloads,
    /// A remote folder that every file is under. Each file is then saved as
    /// `<downloads>/<last component of root>/<path relative to root>`, keeping the
    /// folder's structure; without it, `downloads.subdirectory` applies.
    #[serde(default)]
    pub root: Option<String>,
}

/// `POST /api/transfers/downloads/folder` body: every file in a peer's folder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnqueueFolder {
    pub username: String,
    /// The remote folder, as in browse or search results (`@@share\Artist\Album`).
    pub path: String,
    /// Also download every folder below `path`.
    #[serde(default)]
    pub recursive: bool,
}

/// `POST /api/transfers/downloads` and `POST /api/transfers/downloads/folder` response.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnqueuedDownloads {
    #[serde(default)]
    pub enqueued: Vec<Transfer>,
    #[serde(default)]
    pub failed: Vec<EnqueueFailure>,
    /// The number of files asked for: `enqueued` plus `failed`.
    #[serde(default)]
    pub files: usize,
}

/// A file that could not be enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnqueueFailure {
    pub filename: String,
    pub error: String,
}

/// `POST /api/batches` body: downloads from one user, tracked together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CreateBatch {
    /// A UUID chosen by the caller; generated when absent.
    #[serde(default)]
    pub id: Option<String>,
    /// The search the files were found by (a UUID).
    #[serde(default)]
    pub search_id: Option<String>,
    pub username: String,
    pub files: Vec<DownloadFile>,
    /// Directory below `downloads.directory` for the completed files, relative.
    #[serde(default)]
    pub destination: Option<String>,
    /// An identifier of the caller's choosing.
    #[serde(default)]
    pub external_id: Option<String>,
}

/// A batch of downloads (`GET /api/batches/{id}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    #[serde(default)]
    pub search_id: Option<String>,
    pub username: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub transfers: Vec<Transfer>,
}

/// `POST /api/batches` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatedBatch {
    pub batch: Batch,
    #[serde(default)]
    pub failed: Vec<EnqueueFailure>,
}

/// `GET /api/transfers/downloads/{id}/position` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuePosition {
    pub place_in_queue: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn states_and_derived_fields() {
        for state in TransferState::ALL {
            assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
        }
        assert!(TransferState::Aborted.is_failed());
        assert!(!TransferState::Succeeded.is_failed());
        assert!(!TransferState::QueuedRemotely.is_complete());

        let mut transfer: Transfer = serde_json::from_value(serde_json::json!({
            "id": "6a1b4f2e-0000-0000-0000-000000000001",
            "username": "alice",
            "direction": "download",
            "filename": "@@music\\a\\b.flac",
            "size": 1000,
            "state": "in_progress",
            "requested_at": "2024-05-01T12:00:00Z",
            "started_at": "2024-05-01T12:00:01Z",
            "bytes_transferred": 250,
            "average_speed": 125.0
        }))
        .unwrap();
        transfer.refresh_derived(Utc.with_ymd_and_hms(2024, 5, 1, 12, 0, 3).unwrap());
        assert_eq!(transfer.bytes_remaining, 750);
        assert_eq!(transfer.percent_complete, 25.0);
        assert_eq!(transfer.elapsed_ms, Some(2000));
        assert_eq!(transfer.remaining_ms, Some(6000));
        let value = serde_json::to_value(&transfer).unwrap();
        assert_eq!(value["state"], "in_progress");
        assert_eq!(value["direction"], "download");
        assert_eq!(value["requested_at"], "2024-05-01T12:00:00Z");
        assert!(value["ended_at"].is_null());
        assert!(value["batch_id"].is_null());
    }

    #[test]
    fn batch_request_shape() {
        let request: CreateBatch = serde_json::from_str(
            r#"{"username":"bob","files":[{"filename":"x","size":3}],"external_id":"e","search_id":"s"}"#,
        )
        .unwrap();
        assert_eq!(request.external_id.as_deref(), Some("e"));
        assert_eq!(request.files[0].size, 3);
        assert!(
            serde_json::from_str::<CreateBatch>(r#"{"username":"bob","files":[{"filename":"x"}]}"#)
                .is_err()
        );
    }

    #[test]
    fn enqueue_request_shapes() {
        let plain: EnqueueDownloadsRequest =
            serde_json::from_str(r#"{"username":"bob","files":[{"filename":"a\\b","size":1}]}"#)
                .unwrap();
        assert_eq!(plain.root, None);
        assert_eq!(plain.downloads.files[0].filename, r"a\b");
        let rooted: EnqueueDownloadsRequest =
            serde_json::from_str(r#"{"username":"bob","files":[],"root":"a"}"#).unwrap();
        assert_eq!(rooted.root.as_deref(), Some("a"));
        assert_eq!(rooted.downloads.username, "bob");
        let folder: EnqueueFolder =
            serde_json::from_str(r#"{"username":"bob","path":"a\\b"}"#).unwrap();
        assert!(!folder.recursive);
        assert_eq!(folder.path, r"a\b");
        let value = serde_json::to_value(EnqueuedDownloads::default()).unwrap();
        assert_eq!(value["files"], 0);
    }
}
