//! API models shared between the daemon and the web UI.
//!
//! JSON shapes mirror slskd (camelCase fields, slskd enum string formats), except
//! [`config`], which mirrors `slskr.toml` (snake_case).

pub mod application;
pub mod config;
pub mod conversations;
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
