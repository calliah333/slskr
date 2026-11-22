//! slskd-compatible API models shared between the daemon and the web UI.
//!
//! JSON shapes mirror slskd exactly (camelCase fields, slskd enum string formats).

pub mod application;
pub mod conversations;
pub mod events;
pub mod files;
pub mod logs;
pub mod options;
pub mod relay;
pub mod rooms;
pub mod search;
pub mod server;
pub mod session;
pub mod shares;
pub mod telemetry;
pub mod transfers;
pub mod users;
