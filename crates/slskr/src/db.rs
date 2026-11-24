//! SQLite database wrapper with ordered, transactional migrations.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};

/// How long a statement waits for a lock held by another connection before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Page cache per connection, in KiB (a negative `cache_size`).
const CACHE_KIB: i64 = 8 * 1024;
/// Size the WAL file is truncated to after a checkpoint, so one large write does not leave
/// a large file behind. Checkpoints themselves use SQLite's default (every 1000 pages).
const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    changes: AtomicU64,
    /// The database file; `None` in memory, where a second connection would be another
    /// database.
    path: Option<PathBuf>,
}

impl Database {
    /// Open a SQLite database and apply migrations in order.
    ///
    /// File databases use write-ahead logging with `synchronous = NORMAL`: every commit is
    /// atomic and consistent, but the last commits before a power loss may roll back.
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
        if !in_memory {
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
        Ok(Self {
            inner: Arc::new(Inner {
                changes: AtomicU64::new(connection.total_changes()),
                connection: Mutex::new(connection),
                path: (!in_memory).then(|| path.to_owned()),
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
                changes: AtomicU64::new(0),
                connection: Mutex::new(connection),
                path: Some(path.clone()),
            }),
        })
    }

    /// Rows changed through this connection, without acquiring the SQLite lock.
    pub fn changes(&self) -> u64 {
        self.inner.changes.load(Ordering::Acquire)
    }

    /// Run a synchronous SQLite operation away from the async executor.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut connection = inner
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = f(&mut connection);
            // Publish under the lock, even when a call writes and then returns an error.
            inner
                .changes
                .store(connection.total_changes(), Ordering::Release);
            result.map_err(anyhow::Error::from)
        })
        .await
        .map_err(anyhow::Error::from)?
    }
}

/// Per-connection settings shared by the writer and readers.
fn tune(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -CACHE_KIB)
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
