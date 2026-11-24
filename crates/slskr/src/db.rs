//! SQLite database wrapper with ordered, transactional migrations.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use rusqlite::Connection;

#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    changes: AtomicU64,
}

impl Database {
    /// Open a SQLite database and apply migrations in order.
    pub fn open(path: &Path, migrations: &[&str]) -> anyhow::Result<Self> {
        let connection = if path == Path::new(":memory:") {
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

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
