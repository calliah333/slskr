//! Async Soulseek network client library.
//!
//! See `docs/ARCHITECTURE.md` for the module layout and contracts.

pub mod client;
pub mod error;
pub mod events;
pub mod model;
pub(crate) mod net;
pub mod options;
pub mod protocol;
pub mod resolvers;

pub use client::SoulseekClient;
pub use error::{Error, Result};
pub use events::Event;
pub use model::*;
pub use options::*;
pub use resolvers::Resolvers;
