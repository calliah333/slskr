//! slskr daemon library: HTTP API, live events and services.
//!
//! See `docs/DAEMON.md` for module ownership and contracts.

pub mod app;
pub mod auth;
pub mod bootstrap;
pub mod cli;
pub mod config;
pub mod core_api;
pub mod db;
pub mod events;
pub mod files;
pub mod http;
pub mod logging;
pub mod messaging;
pub mod profiling;
pub mod realtime;
pub mod resolvers;
pub mod search;
pub mod setup;
pub mod shares;
pub mod state;
pub mod telemetry;
pub mod transfers;
pub mod users;
pub mod web;

pub use bootstrap::run;
pub use cli::Cli;

/// `N` bytes from the operating system's random number generator, for salts, tokens and
/// secrets.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator");
    bytes
}
