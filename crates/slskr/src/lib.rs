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

/// Creates `path` (and its parents) and makes it private to the daemon's user (`0700` on
/// Unix), also when it already exists: it holds messages, sessions, peer addresses and
/// the first-run setup code, which a default umask would leave readable to everyone.
pub(crate) fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `N` bytes from the operating system's random number generator, for salts, tokens and
/// secrets.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator");
    bytes
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn private_dirs_are_created_and_tightened() {
        let temp = tempfile::tempdir().unwrap();
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode();
        let created = temp.path().join("a/db");
        super::create_private_dir(&created).unwrap();
        assert_eq!(mode(&created) & 0o777, 0o700);

        let existing = temp.path().join("logs");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        super::create_private_dir(&existing).unwrap();
        assert_eq!(mode(&existing) & 0o777, 0o700);
    }
}
