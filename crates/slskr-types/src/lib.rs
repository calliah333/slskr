//! Types of the slskr HTTP API (`{url_base}/api/...`) and the live events socket, shared by
//! the daemon and the web UI. No I/O; compiles to wasm.
//!
//! JSON conventions (see `docs/API.md`): field names and enum values are `snake_case`,
//! timestamps are RFC 3339 UTC strings, sizes are bytes (`u64`), durations are integer
//! milliseconds in fields named `*_ms`, and absent optional values are `null`. [`config`]
//! mirrors `slskr.toml`.

pub mod application;
pub mod config;
pub mod conversations;
pub mod error;
pub mod events;
pub mod files;
pub mod logs;
pub mod realtime;
pub mod rooms;
pub mod search;
pub mod server;
pub mod session;
pub mod settings;
pub mod shares;
pub mod telemetry;
pub mod transfers;
pub mod users;

/// Milliseconds of a duration, saturating.
pub fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
