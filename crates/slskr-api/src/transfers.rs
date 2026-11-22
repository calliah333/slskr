//! Transfers API models (`/api/v0/transfers`), plus the shared helpers used by
//! every slskd DTO module: the [`slskd_flags!`](crate::slskd_flags) newtype
//! generator and the .NET-compatible [`time`] serde adapters.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Helpers backing [`slskd_flags!`](crate::slskd_flags); formats/parses .NET
/// `[Flags]` enum strings (`"Completed, Succeeded"`).
#[doc(hidden)]
pub mod flags {
    use std::fmt;

    /// Error returned when a string is not a valid flags value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FlagsParseError {
        /// Name of the flags type being parsed.
        pub type_name: &'static str,
        /// The offending input.
        pub value: String,
    }

    impl fmt::Display for FlagsParseError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "'{}' is not a valid {} value",
                self.value, self.type_name
            )
        }
    }

    impl std::error::Error for FlagsParseError {}

    /// Formats `bits` the way .NET `Enum.ToString()` does for `[Flags]` enums.
    /// Returns `None` when some bits have no name (.NET then emits the number).
    pub fn format(named: &[(&str, u32)], bits: u32) -> Option<String> {
        if let Some((name, _)) = named.iter().find(|(_, value)| *value == bits) {
            return Some((*name).to_owned());
        }
        if bits == 0 {
            return None;
        }

        let mut candidates: Vec<&(&str, u32)> =
            named.iter().filter(|(_, value)| *value != 0).collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        let mut remaining = bits;
        let mut names = Vec::new();
        for (name, value) in candidates {
            if remaining & value == *value {
                names.push(*name);
                remaining &= !value;
            }
        }

        if remaining != 0 {
            return None;
        }

        names.reverse();
        Some(names.join(", "))
    }

    /// Parses a comma-separated list of names (case-insensitive) and/or integers.
    pub fn parse(named: &[(&str, u32)], s: &str) -> Option<u32> {
        let mut bits = 0u32;
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            if let Some((_, value)) = named
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(part))
            {
                bits |= value;
            } else {
                bits |= part.parse::<u32>().ok()?;
            }
        }
        Some(bits)
    }
}

pub use flags::FlagsParseError;

/// Declares a `u32` newtype mirroring a .NET `[Flags]` enum, serialized as
/// slskd does (`"A, B"` string, or a bare number when bits are unnamed).
#[doc(hidden)]
#[macro_export]
macro_rules! slskd_flags {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$cmeta:meta])* $konst:ident = $value:literal => $label:literal, )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(pub u32);

        impl $name {
            $( $(#[$cmeta])* pub const $konst: Self = Self($value); )*

            /// Every named value with its .NET name, in declaration order.
            pub const NAMED: &'static [(&'static str, u32)] = &[$(($label, $value)),*];

            /// Raw bits.
            pub const fn bits(self) -> u32 {
                self.0
            }

            /// Wraps raw bits.
            pub const fn from_bits(bits: u32) -> Self {
                Self(bits)
            }

            /// True when every bit of `other` is set (.NET `HasFlag`).
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// True when any bit of `other` is set.
            pub const fn intersects(self, other: Self) -> bool {
                self.0 & other.0 != 0
            }

            /// True when no bits are set.
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// Bitwise union.
            pub const fn union(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }

            /// Bits of `self` not in `other`.
            pub const fn difference(self, other: Self) -> Self {
                Self(self.0 & !other.0)
            }

            /// Sets the bits of `other`.
            pub fn insert(&mut self, other: Self) {
                self.0 |= other.0;
            }

            /// Clears the bits of `other`.
            pub fn remove(&mut self, other: Self) {
                self.0 &= !other.0;
            }

            /// .NET `ToString()` form; `None` when some bits are unnamed.
            pub fn to_names(self) -> Option<String> {
                $crate::transfers::flags::format(Self::NAMED, self.0)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                match self.to_names() {
                    Some(names) => f.write_str(&names),
                    None => write!(f, "{}", self.0),
                }
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::transfers::flags::FlagsParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $crate::transfers::flags::parse(Self::NAMED, s).map(Self).ok_or_else(|| {
                    $crate::transfers::flags::FlagsParseError {
                        type_name: stringify!($name),
                        value: s.to_owned(),
                    }
                })
            }
        }

        impl ::std::ops::BitOr for $name {
            type Output = Self;

            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }

        impl ::std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl ::std::ops::BitAnd for $name {
            type Output = Self;

            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }

        impl ::std::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self.to_names() {
                    Some(names) => serializer.serialize_str(&names),
                    None => serializer.serialize_u32(self.0),
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct FlagsVisitor;

                impl<'de> ::serde::de::Visitor<'de> for FlagsVisitor {
                    type Value = $name;

                    fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        f.write_str(concat!("a ", stringify!($name), " flags string or integer"))
                    }

                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<$name, E> {
                        v.parse::<$name>().map_err(E::custom)
                    }

                    fn visit_u64<E: ::serde::de::Error>(self, v: u64) -> Result<$name, E> {
                        u32::try_from(v).map($name).map_err(|_| E::custom("flags value out of range"))
                    }

                    fn visit_i64<E: ::serde::de::Error>(self, v: i64) -> Result<$name, E> {
                        u32::try_from(v).map($name).map_err(|_| E::custom("flags value out of range"))
                    }
                }

                deserializer.deserialize_any(FlagsVisitor)
            }
        }
    };
}

/// Serde adapters matching System.Text.Json's `DateTime` and `TimeSpan` formats.
///
/// Deserialization of timestamps is lenient: slskd emits offset-less values for
/// records loaded from its database; those are read as UTC.
pub mod time {
    use std::fmt::Write as _;
    use std::time::Duration;

    use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};

    /// Parses an ISO-8601 timestamp with or without an offset (offset-less = UTC).
    pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
        let s = s.trim();
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|n| n.and_utc())
            })
    }

    /// Formats a timestamp as RFC 3339 with a `Z` suffix.
    pub fn format_datetime(value: &DateTime<Utc>) -> String {
        value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }

    /// Parses a .NET `TimeSpan` constant ("c") string: `[-][d.]hh:mm:ss[.fffffff]`.
    /// Negative spans clamp to zero.
    pub fn parse_timespan(s: &str) -> Option<Duration> {
        let s = s.trim();
        let (negative, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };

        let mut parts = s.split(':');
        let first = parts.next()?;
        let minutes = parts.next()?;
        let seconds = parts.next()?;
        if parts.next().is_some() {
            return None;
        }

        let (days, hours) = match first.split_once('.') {
            Some((d, h)) => (d.parse::<u64>().ok()?, h.parse::<u64>().ok()?),
            None => (0, first.parse::<u64>().ok()?),
        };
        let minutes = minutes.parse::<u64>().ok()?;
        let (secs, nanos) = match seconds.split_once('.') {
            Some((whole, fraction)) => {
                if fraction.is_empty()
                    || fraction.len() > 9
                    || !fraction.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let mut nanos: u32 = fraction.parse().ok()?;
                for _ in fraction.len()..9 {
                    nanos *= 10;
                }
                (whole.parse::<u64>().ok()?, nanos)
            }
            None => (seconds.parse::<u64>().ok()?, 0),
        };

        if hours > 23 || minutes > 59 || secs > 59 {
            return None;
        }
        if negative {
            return Some(Duration::ZERO);
        }

        Some(Duration::new(
            days * 86_400 + hours * 3_600 + minutes * 60 + secs,
            nanos,
        ))
    }

    /// Formats a duration as a .NET `TimeSpan` constant ("c") string.
    pub fn format_timespan(value: Duration) -> String {
        let total = value.as_secs();
        let days = total / 86_400;
        let hours = total % 86_400 / 3_600;
        let minutes = total % 3_600 / 60;
        let seconds = total % 60;
        let ticks = value.subsec_nanos() / 100;

        let mut out = String::with_capacity(26);
        if days > 0 {
            let _ = write!(out, "{days}.");
        }
        let _ = write!(out, "{hours:02}:{minutes:02}:{seconds:02}");
        if ticks > 0 {
            let _ = write!(out, ".{ticks:07}");
        }
        out
    }

    /// `#[serde(with)]` adapter for `DateTime<Utc>`.
    pub mod datetime {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer, de::Error};

        pub fn serialize<S: Serializer>(
            value: &DateTime<Utc>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(&super::format_datetime(value))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<DateTime<Utc>, D::Error> {
            let s = String::deserialize(deserializer)?;
            super::parse_datetime(&s)
                .ok_or_else(|| D::Error::custom(format!("invalid timestamp '{s}'")))
        }
    }

    /// `#[serde(with)]` adapter for `Option<DateTime<Utc>>`; pair with
    /// `default` and `skip_serializing_if = "Option::is_none"`.
    pub mod option_datetime {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer, de::Error};

        pub fn serialize<S: Serializer>(
            value: &Option<DateTime<Utc>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => serializer.serialize_str(&super::format_datetime(value)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            match Option::<String>::deserialize(deserializer)? {
                Some(s) => super::parse_datetime(&s)
                    .map(Some)
                    .ok_or_else(|| D::Error::custom(format!("invalid timestamp '{s}'"))),
                None => Ok(None),
            }
        }
    }

    /// `#[serde(with)]` adapter for `Option<Duration>` as a .NET `TimeSpan`
    /// string; pair with `default` and `skip_serializing_if = "Option::is_none"`.
    pub mod option_timespan {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer, de::Error};

        pub fn serialize<S: Serializer>(
            value: &Option<Duration>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => serializer.serialize_str(&super::format_timespan(*value)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Duration>, D::Error> {
            match Option::<String>::deserialize(deserializer)? {
                Some(s) => super::parse_timespan(&s)
                    .map(Some)
                    .ok_or_else(|| D::Error::custom(format!("invalid timespan '{s}'"))),
                None => Ok(None),
            }
        }
    }
}

crate::slskd_flags! {
    /// Soulseek.NET `TransferStates`.
    TransferStates {
        NONE = 0 => "None",
        REQUESTED = 1 => "Requested",
        QUEUED = 2 => "Queued",
        INITIALIZING = 4 => "Initializing",
        IN_PROGRESS = 8 => "InProgress",
        COMPLETED = 16 => "Completed",
        SUCCEEDED = 32 => "Succeeded",
        CANCELLED = 64 => "Cancelled",
        TIMED_OUT = 128 => "TimedOut",
        ERRORED = 256 => "Errored",
        REJECTED = 512 => "Rejected",
        ABORTED = 1024 => "Aborted",
        LOCALLY = 2048 => "Locally",
        REMOTELY = 4096 => "Remotely",
    }
}

/// Soulseek.NET `TransferDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TransferDirection {
    #[default]
    Download,
    Upload,
}

impl TransferDirection {
    /// Lowercase route segment prefix (`download`/`upload`), as used in `/transfers/{direction}s`.
    pub const fn as_route(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Upload => "upload",
        }
    }
}

/// slskd `Transfers.Transfer` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub id: String,
    pub username: String,
    pub direction: TransferDirection,
    pub filename: String,
    pub size: u64,
    pub state: TransferStates,
    #[serde(with = "time::datetime")]
    pub requested_at: DateTime<Utc>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_datetime"
    )]
    pub enqueued_at: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_datetime"
    )]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_datetime"
    )]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub average_speed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_in_queue: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_datetime"
    )]
    pub next_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub removed: bool,
    /// Derived: `size - bytesTransferred`.
    #[serde(default)]
    pub bytes_remaining: i64,
    /// Derived: `(endedAt ?? now) - startedAt`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_timespan"
    )]
    pub elapsed_time: Option<Duration>,
    /// Derived: `bytesTransferred / size * 100`.
    #[serde(default)]
    pub percent_complete: f64,
    /// Derived: `bytesRemaining / averageSpeed` seconds.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::option_timespan"
    )]
    pub remaining_time: Option<Duration>,
}

impl Transfer {
    /// Recomputes the derived fields (`bytesRemaining`, `elapsedTime`,
    /// `percentComplete`, `remainingTime`) exactly as slskd's getters do.
    pub fn refresh_derived(&mut self, now: DateTime<Utc>) {
        self.bytes_remaining = self.size as i64 - self.bytes_transferred as i64;
        self.elapsed_time = self.started_at.map(|started| {
            (self.ended_at.unwrap_or(now) - started)
                .to_std()
                .unwrap_or(Duration::ZERO)
        });
        self.percent_complete = if self.size == 0 {
            0.0
        } else {
            self.bytes_transferred as f64 / self.size as f64 * 100.0
        };
        self.remaining_time = if self.average_speed == 0.0 {
            None
        } else {
            Some(
                Duration::try_from_secs_f64(self.bytes_remaining as f64 / self.average_speed)
                    .unwrap_or(Duration::ZERO),
            )
        };
    }
}

/// Transfers grouped by directory, as in `GET /transfers/{direction}s`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryResponse {
    pub directory: String,
    #[serde(default)]
    pub file_count: u32,
    #[serde(default)]
    pub files: Vec<Transfer>,
}

/// Transfers grouped by user, as in `GET /transfers/{direction}s`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserResponse {
    pub username: String,
    #[serde(default)]
    pub directories: Vec<DirectoryResponse>,
}

/// Item of the `POST /transfers/downloads/{username}` body (a JSON array).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueDownloadRequest {
    pub filename: String,
    #[serde(default)]
    pub size: u64,
}

/// `201` body of `POST /transfers/downloads/{username}`.
///
/// slskd serializes `failed` as value tuples (which System.Text.Json renders as
/// `{}`); slskr emits `{filename, message}` and accepts either.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadsResponse {
    #[serde(default)]
    pub enqueued: Vec<Transfer>,
    #[serde(default)]
    pub failed: Vec<EnqueueDownloadBatchResponseFailure>,
}

/// slskd `Batch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_id: Option<String>,
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub direction: TransferDirection,
    #[serde(with = "time::datetime")]
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfers: Option<Vec<Transfer>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<BatchOptions>,
}

/// slskd `BatchOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

/// Body of `POST /transfers/downloads/batches`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadBatchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_id: Option<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub files: Vec<EnqueueDownloadBatchItem>,
    #[serde(default)]
    pub options: EnqueueDownloadBatchOptions,
}

/// File entry of [`EnqueueDownloadBatchRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadBatchItem {
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Options of [`EnqueueDownloadBatchRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadBatchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

/// Response of `POST /transfers/downloads/batches` (200/201/207).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadBatchResponse {
    pub batch: Batch,
    #[serde(default)]
    pub failures: Vec<EnqueueDownloadBatchResponseFailure>,
}

/// A file that could not be enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueDownloadBatchResponseFailure {
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn transfer_states_round_trip() {
        let state = TransferStates::COMPLETED | TransferStates::SUCCEEDED;
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#""Completed, Succeeded""#
        );
        assert_eq!(
            serde_json::from_str::<TransferStates>(r#""Completed, Succeeded""#).unwrap(),
            state
        );
        assert_eq!(
            serde_json::from_str::<TransferStates>(r#""Queued, Remotely""#).unwrap(),
            TransferStates::QUEUED | TransferStates::REMOTELY
        );
        assert_eq!(
            serde_json::to_string(&TransferStates::NONE).unwrap(),
            r#""None""#
        );
        assert_eq!(serde_json::from_str::<TransferStates>("48").unwrap(), state);
        assert_eq!(
            serde_json::from_str::<TransferStates>(r#""completed,succeeded""#).unwrap(),
            state
        );
        assert_eq!(
            serde_json::to_string(&TransferStates(1 << 20)).unwrap(),
            "1048576"
        );
        assert!(serde_json::from_str::<TransferStates>(r#""Bogus""#).is_err());
        assert!(state.contains(TransferStates::COMPLETED));
        assert!(!state.contains(TransferStates::ERRORED));
        assert_eq!(state.to_string(), "Completed, Succeeded");
        assert_eq!(
            "InProgress".parse::<TransferStates>().unwrap(),
            TransferStates::IN_PROGRESS
        );
    }

    #[test]
    fn timespan_format() {
        use time::{format_timespan, parse_timespan};
        assert_eq!(format_timespan(Duration::from_secs(83)), "00:01:23");
        assert_eq!(
            format_timespan(Duration::from_millis(83_500)),
            "00:01:23.5000000"
        );
        assert_eq!(format_timespan(Duration::from_secs(90_061)), "1.01:01:01");
        assert_eq!(
            parse_timespan("00:01:23.5000000"),
            Some(Duration::from_millis(83_500))
        );
        assert_eq!(
            parse_timespan("1.01:01:01"),
            Some(Duration::from_secs(90_061))
        );
        assert_eq!(parse_timespan("-00:00:01"), Some(Duration::ZERO));
        assert_eq!(parse_timespan("bad"), None);
    }

    #[test]
    fn lenient_datetime() {
        let expected = Utc.with_ymd_and_hms(2024, 5, 1, 12, 0, 0).unwrap();
        assert_eq!(time::parse_datetime("2024-05-01T12:00:00Z"), Some(expected));
        assert_eq!(time::parse_datetime("2024-05-01T12:00:00"), Some(expected));
        assert_eq!(
            time::parse_datetime("2024-05-01T14:00:00+02:00"),
            Some(expected)
        );
        assert!(time::parse_datetime("2024-05-01T12:00:00.1234567").is_some());
    }

    #[test]
    fn transfer_json_shape() {
        let json = r#"{
            "id": "6a1b4f2e-0000-0000-0000-000000000001",
            "username": "alice",
            "direction": "Download",
            "filename": "@@music\\a\\b.flac",
            "size": 1000,
            "state": "InProgress",
            "requestedAt": "2024-05-01T12:00:00.1234567",
            "startedAt": "2024-05-01T12:00:01Z",
            "bytesTransferred": 250,
            "averageSpeed": 125.0,
            "attempts": 0,
            "removed": false,
            "bytesRemaining": 750,
            "elapsedTime": "00:00:02",
            "percentComplete": 25.0,
            "remainingTime": "00:00:06"
        }"#;
        let mut transfer: Transfer = serde_json::from_str(json).unwrap();
        assert_eq!(transfer.state, TransferStates::IN_PROGRESS);
        assert_eq!(transfer.remaining_time, Some(Duration::from_secs(6)));

        transfer.refresh_derived(Utc.with_ymd_and_hms(2024, 5, 1, 12, 0, 3).unwrap());
        assert_eq!(transfer.bytes_remaining, 750);
        assert_eq!(transfer.percent_complete, 25.0);
        assert_eq!(transfer.elapsed_time, Some(Duration::from_secs(2)));

        let value = serde_json::to_value(&transfer).unwrap();
        for key in [
            "id",
            "username",
            "direction",
            "filename",
            "size",
            "state",
            "requestedAt",
            "startedAt",
            "bytesTransferred",
            "averageSpeed",
            "attempts",
            "removed",
            "bytesRemaining",
            "elapsedTime",
            "percentComplete",
            "remainingTime",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert!(value.get("endedAt").is_none());
        assert!(value.get("batchId").is_none());
        assert_eq!(value["state"], "InProgress");

        let grouped = vec![UserResponse {
            username: "alice".into(),
            directories: vec![DirectoryResponse {
                directory: "@@music\\a".into(),
                file_count: 1,
                files: vec![transfer],
            }],
        }];
        let value = serde_json::to_value(&grouped).unwrap();
        assert_eq!(value[0]["directories"][0]["fileCount"], 1);
    }

    #[test]
    fn batch_request_shape() {
        let request: EnqueueDownloadBatchRequest = serde_json::from_str(
            r#"{"username":"bob","files":[{"filename":"x","size":3}],"options":{"externalId":"e"},"searchId":"s"}"#,
        )
        .unwrap();
        assert_eq!(request.options.external_id.as_deref(), Some("e"));
        assert_eq!(request.files[0].size, Some(3));
        assert_eq!(request.search_id.as_deref(), Some("s"));
    }
}
