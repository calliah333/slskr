//! SQLite database wrapper with ordered, transactional migrations.

use std::{
    panic::Location,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};

use crate::profiling;

/// How long a statement waits for a lock held by another connection before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Page cache per connection, in KiB (a negative `cache_size`).
const CACHE_KIB: i64 = 8 * 1024;
/// Size the WAL file is truncated to after a checkpoint, so one large write does not leave
/// a large file behind. Checkpoints themselves use SQLite's default (every 1000 pages).
const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;
/// Rows `PRAGMA optimize` samples per index, so refreshing statistics stays cheap on large
/// tables (SQLite's recommendation for `analysis_limit`).
const ANALYSIS_LIMIT: i64 = 400;
/// How often a writer connection refreshes planner statistics and returns free pages.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// `auto_vacuum = INCREMENTAL`.
const INCREMENTAL: i64 = 2;

#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Slot>,
    /// The database file; `None` in memory, where a second connection would be another
    /// database.
    path: Option<PathBuf>,
    /// Names the database in [`profiling`]: the file stem, `(read)` for a reader.
    label: Arc<str>,
}

struct Slot {
    connection: Connection,
    /// When the connection was last maintained; `None` for read-only connections, which
    /// cannot be.
    maintained_at: Option<Instant>,
}

impl Database {
    /// Open a SQLite database and apply migrations in order.
    ///
    /// File databases use write-ahead logging with `synchronous = NORMAL`: every commit is
    /// atomic and consistent, but the last commits before a power loss may roll back. They
    /// also use incremental auto-vacuum, so pages that retention frees are returned to the
    /// file system by `maintain` rather than kept forever.
    pub fn open(path: &Path, migrations: &[&str]) -> anyhow::Result<Self> {
        let in_memory = path == Path::new(":memory:");
        let connection = if in_memory {
            Connection::open_in_memory()?
        } else {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            Connection::open(path)?
        };
        connection.pragma_update(None, "foreign_keys", "ON")?;
        tune(&connection)?;
        functions(&connection)?;
        if !in_memory {
            // Before the journal mode: on a new file this must precede the first write.
            enable_incremental_vacuum(&connection, path)?;
            // Outside any transaction: the journal mode cannot change inside one.
            let mode: String =
                connection
                    .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
            anyhow::ensure!(
                mode.eq_ignore_ascii_case("wal"),
                "could not enable write-ahead logging for {} (journal mode {mode})",
                path.display()
            );
            connection.pragma_update(None, "synchronous", "NORMAL")?;
            connection.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT)?;
        }
        let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(
            current >= 0 && current as usize <= migrations.len(),
            "database schema version {current} is newer than available migrations {}",
            migrations.len()
        );
        for (index, migration) in migrations.iter().enumerate().skip(current as usize) {
            let version = (index + 1) as i64;
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(migration)?;
            transaction.pragma_update(None, "user_version", version)?;
            transaction.commit()?;
        }
        // SQLite's advice for long-lived connections: analyze what needs it now (0x10000
        // also checks tables that have not been queried yet), then again periodically.
        connection.execute_batch("PRAGMA optimize = 0x10002;")?;
        // Retention may have freed pages since the last run; restarts must not keep
        // postponing their return past the first maintenance interval.
        release_free_pages(&connection)?;
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(Slot {
                    connection,
                    maintained_at: Some(Instant::now()),
                }),
                path: (!in_memory).then(|| path.to_owned()),
                label: label(path).into(),
            }),
        })
    }

    /// A separate read-only connection to the same file, for long reads (reports) that
    /// should not hold up writers: with write-ahead logging, readers and the writer do not
    /// block each other. An in-memory database returns itself.
    pub fn reader(&self) -> anyhow::Result<Self> {
        let Some(path) = &self.inner.path else {
            return Ok(self.clone());
        };
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        tune(&connection)?;
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(Slot {
                    connection,
                    maintained_at: None,
                }),
                path: Some(path.clone()),
                label: format!("{} (read)", self.inner.label).into(),
            }),
        })
    }

    /// Run a synchronous SQLite operation away from the async executor. A writer
    /// connection also runs `maintain` after it, at most every `MAINTENANCE_INTERVAL`.
    ///
    /// Each call is timed for [`crate::profiling`] under its caller's line.
    #[track_caller]
    pub fn call<T, F>(&self, f: F) -> impl Future<Output = anyhow::Result<T>> + Send + use<T, F>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let caller = Location::caller();
        let site = profiling::db_site(&self.inner.label, caller);
        let inner = Arc::clone(&self.inner);
        let called = Instant::now();
        async move {
            tokio::task::spawn_blocking(move || {
                let mut slot = inner
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let started = Instant::now();
                let result = f(&mut slot.connection).map_err(anyhow::Error::from);
                let (wait, run) = (started - called, started.elapsed());
                site.wait.record(wait);
                site.run.record(run);
                profiling::db_call_ended(&inner.label, caller, wait, run);
                if slot
                    .maintained_at
                    .is_some_and(|at| at.elapsed() >= MAINTENANCE_INTERVAL)
                {
                    slot.maintained_at = Some(Instant::now());
                    let started = Instant::now();
                    match maintain(&slot.connection) {
                        Ok(()) => tracing::info!(database = %inner.label,
                            ms = started.elapsed().as_millis() as u64, "maintained database"),
                        Err(error) => tracing::warn!(database = %inner.label, %error,
                            "could not maintain database"),
                    }
                }
                result
            })
            .await
            .map_err(anyhow::Error::from)?
        }
    }

    /// Gives free pages back to the file system now if they are a tenth of the file, after
    /// a large delete.
    pub async fn release_free_pages(&self) -> anyhow::Result<()> {
        self.call(|connection| release_free_pages(connection)).await
    }

    /// Runs [`maintain`] now.
    #[cfg(test)]
    pub(crate) async fn maintain_now(&self) -> anyhow::Result<()> {
        self.call(|connection| maintain(connection)).await
    }
}

/// Per-connection settings shared by the writer and readers.
fn tune(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -CACHE_KIB)?;
    connection.pragma_update(None, "analysis_limit", ANALYSIS_LIMIT)
}

/// SQL functions that migrations use: `mask_addresses(text)` (see
/// [`crate::transfers::mask_addresses`]).
fn functions(connection: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    connection.create_scalar_function(
        "mask_addresses",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .map(|text| crate::transfers::mask_addresses(&text).into_owned()))
        },
    )
}

/// Switches a file to `auto_vacuum = INCREMENTAL`. A new file only needs the setting; an
/// existing one is rebuilt once with `VACUUM`, which also returns its free pages.
fn enable_incremental_vacuum(connection: &Connection, path: &Path) -> rusqlite::Result<()> {
    let mode: i64 = connection.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    if mode == INCREMENTAL {
        return Ok(());
    }
    connection.pragma_update(None, "auto_vacuum", INCREMENTAL)?;
    let pages: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    if pages > 0 {
        tracing::info!(path = %path.display(), "rebuilding database for incremental vacuum (once)");
        connection.execute_batch("VACUUM;")?;
    }
    Ok(())
}

/// Refreshes planner statistics where the data changed enough to matter, and gives free
/// pages back to the file system.
fn maintain(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA optimize;")?;
    release_free_pages(connection)
}

/// Gives free pages back to the file system once they are a tenth of the file.
fn release_free_pages(connection: &Connection) -> rusqlite::Result<()> {
    let free: i64 = connection.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let pages: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    if free > 0 && free * 10 >= pages {
        // Each step frees one page, so the statement runs until it has no more rows.
        let mut statement = connection.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
    }
    Ok(())
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;

/// `transfers` for `…/transfers.db`.
fn label(path: &Path) -> String {
    path.file_stem().map_or_else(
        || "database".into(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}
