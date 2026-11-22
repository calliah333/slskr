//! Transfer model: direction, state flags and snapshot.

use std::fmt;
use std::net::SocketAddr;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};
use std::time::{Duration, SystemTime};

/// Direction of a file transfer (wire values 0 and 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferDirection {
    Download = 0,
    Upload = 1,
}

impl TransferDirection {
    /// Wire value.
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Parses a wire value.
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Download),
            1 => Some(Self::Upload),
            _ => None,
        }
    }

    /// Name as rendered by .NET (`"Download"`, `"Upload"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "Download",
            Self::Upload => "Upload",
        }
    }
}

impl fmt::Display for TransferDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Transfer state flags (Soulseek.NET `TransferStates`).
///
/// `COMPLETED` is accompanied by exactly one disposition flag (`SUCCEEDED`,
/// `CANCELLED`, `TIMED_OUT`, `ERRORED`, `REJECTED` or `ABORTED`); `QUEUED` by
/// `LOCALLY` or `REMOTELY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TransferState(pub u32);

impl TransferState {
    pub const NONE: Self = Self(0);
    pub const REQUESTED: Self = Self(1);
    pub const QUEUED: Self = Self(2);
    pub const INITIALIZING: Self = Self(4);
    pub const IN_PROGRESS: Self = Self(8);
    pub const COMPLETED: Self = Self(16);
    pub const SUCCEEDED: Self = Self(32);
    pub const CANCELLED: Self = Self(64);
    pub const TIMED_OUT: Self = Self(128);
    pub const ERRORED: Self = Self(256);
    pub const REJECTED: Self = Self(512);
    pub const ABORTED: Self = Self(1024);
    pub const LOCALLY: Self = Self(2048);
    pub const REMOTELY: Self = Self(4096);

    /// Flag names in ascending value order, as declared in .NET.
    const NAMES: [(u32, &'static str); 13] = [
        (1, "Requested"),
        (2, "Queued"),
        (4, "Initializing"),
        (8, "InProgress"),
        (16, "Completed"),
        (32, "Succeeded"),
        (64, "Cancelled"),
        (128, "TimedOut"),
        (256, "Errored"),
        (512, "Rejected"),
        (1024, "Aborted"),
        (2048, "Locally"),
        (4096, "Remotely"),
    ];

    /// Raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// From raw bits.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether all flags of `other` are set (`HasFlag`); always true for `NONE`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag of `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Sets the flags of `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clears the flags of `other`.
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Whether the transfer reached a terminal state.
    pub const fn is_completed(self) -> bool {
        self.contains(Self::COMPLETED)
    }

    /// .NET `[Flags]` `ToString()`, e.g. `"Completed, Succeeded"`, `"Queued, Remotely"`, `"None"`.
    pub fn to_slskd_string(self) -> String {
        flags_to_string(self.0, &Self::NAMES)
    }

    /// Parses the output of [`Self::to_slskd_string`] (case-insensitive, also accepts a number).
    pub fn from_slskd_str(s: &str) -> Option<Self> {
        flags_from_str(s, &Self::NAMES).map(Self)
    }
}

impl fmt::Display for TransferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_slskd_string())
    }
}

impl BitOr for TransferState {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TransferState {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for TransferState {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for TransferState {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

/// .NET flags formatting: zero → `"None"`; set flags ascending, joined by `", "`;
/// unknown bits → the decimal value.
pub(crate) fn flags_to_string(bits: u32, names: &[(u32, &str)]) -> String {
    if bits == 0 {
        return "None".to_string();
    }
    let mut remaining = bits;
    let mut parts = Vec::new();
    for &(value, name) in names {
        if bits & value == value {
            parts.push(name);
            remaining &= !value;
        }
    }
    if remaining != 0 {
        return bits.to_string();
    }
    parts.join(", ")
}

/// Inverse of [`flags_to_string`].
pub(crate) fn flags_from_str(s: &str, names: &[(u32, &str)]) -> Option<u32> {
    let s = s.trim();
    if let Ok(v) = s.parse::<u32>() {
        return Some(v);
    }
    let mut bits = 0;
    for part in s.split(',') {
        let part = part.trim();
        if part.eq_ignore_ascii_case("None") {
            continue;
        }
        let (value, _) = names.iter().find(|(_, n)| n.eq_ignore_ascii_case(part))?;
        bits |= value;
    }
    Some(bits)
}

/// Immutable snapshot of a transfer (Soulseek.NET `Transfer`).
#[derive(Debug, Clone, PartialEq)]
pub struct Transfer {
    pub direction: TransferDirection,
    pub username: String,
    /// Remote filename (as known by the uploading peer).
    pub filename: String,
    /// Local, unique token.
    pub token: u32,
    pub state: TransferState,
    /// Total size in bytes.
    pub size: u64,
    /// Offset at which the transfer started (resume).
    pub start_offset: u64,
    /// Bytes transferred, including `start_offset`.
    pub bytes_transferred: u64,
    /// Average speed in bytes per second.
    pub average_speed: f64,
    pub start_time: Option<SystemTime>,
    pub end_time: Option<SystemTime>,
    /// Token assigned by the remote peer, if any.
    pub remote_token: Option<u32>,
    /// Endpoint of the remote peer, once known.
    pub ip_endpoint: Option<SocketAddr>,
    /// Message of the error that ended the transfer, if any.
    pub exception: Option<String>,
}

impl Transfer {
    /// New transfer with no progress.
    pub fn new(
        direction: TransferDirection,
        username: impl Into<String>,
        filename: impl Into<String>,
        token: u32,
        state: TransferState,
        size: u64,
        start_offset: u64,
    ) -> Self {
        Self {
            direction,
            username: username.into(),
            filename: filename.into(),
            token,
            state,
            size,
            start_offset,
            bytes_transferred: 0,
            average_speed: 0.0,
            start_time: None,
            end_time: None,
            remote_token: None,
            ip_endpoint: None,
            exception: None,
        }
    }

    /// Bytes not yet transferred.
    pub fn bytes_remaining(&self) -> u64 {
        self.size.saturating_sub(self.bytes_transferred)
    }

    /// Progress in percent (0 when the size is 0).
    pub fn percent_complete(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            self.bytes_transferred as f64 / self.size as f64 * 100.0
        }
    }

    /// Time since start, until the end time or now.
    pub fn elapsed(&self) -> Option<Duration> {
        let start = self.start_time?;
        let end = self.end_time.unwrap_or_else(SystemTime::now);
        Some(end.duration_since(start).unwrap_or_default())
    }

    /// Estimated remaining time from the average speed.
    pub fn remaining(&self) -> Option<Duration> {
        if self.average_speed > 0.0 && self.average_speed.is_finite() {
            Some(Duration::from_secs_f64(
                self.bytes_remaining() as f64 / self.average_speed,
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_strings() {
        assert_eq!(TransferState::NONE.to_slskd_string(), "None");
        assert_eq!(
            (TransferState::COMPLETED | TransferState::SUCCEEDED).to_slskd_string(),
            "Completed, Succeeded"
        );
        assert_eq!(
            (TransferState::REMOTELY | TransferState::QUEUED).to_slskd_string(),
            "Queued, Remotely"
        );
        assert_eq!(TransferState(1 << 20).to_slskd_string(), "1048576");
        assert_eq!(
            TransferState::from_slskd_str("Completed, Errored"),
            Some(TransferState::COMPLETED | TransferState::ERRORED)
        );
        assert_eq!(TransferState::from_slskd_str("bogus"), None);
    }

    #[test]
    fn flags() {
        let s = TransferState::COMPLETED | TransferState::CANCELLED;
        assert!(s.contains(TransferState::COMPLETED));
        assert!(!s.contains(TransferState::SUCCEEDED));
        assert!(s.is_completed());
    }

    #[test]
    fn derived_values() {
        let mut t = Transfer::new(
            TransferDirection::Download,
            "u",
            "f",
            1,
            TransferState::IN_PROGRESS,
            200,
            0,
        );
        t.bytes_transferred = 50;
        t.average_speed = 50.0;
        assert_eq!(t.bytes_remaining(), 150);
        assert_eq!(t.percent_complete(), 25.0);
        assert_eq!(t.remaining(), Some(Duration::from_secs(3)));
        assert_eq!(t.elapsed(), None);
    }
}
