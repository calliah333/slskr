//! slskr daemon library: HTTP API, live events and services.
//!
//! See `docs/DAEMON.md` for module ownership and contracts.

pub mod app;
pub mod auth;
pub mod bootstrap;
pub mod config;
pub mod core_api;
pub mod db;
pub mod events;
pub mod files;
pub mod http;
pub mod logging;
pub mod messaging;
pub mod realtime;
pub mod resolvers;
pub mod search;
pub mod shares;
pub mod state;
pub mod telemetry;
pub mod transfers;
pub mod users;

pub use bootstrap::{Cli, run};
