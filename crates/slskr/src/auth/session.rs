//! Server-side sign-in sessions behind the `slskr_session` cookie.
//!
//! A token is 32 random bytes, base64url encoded, handed to the browser once. Only its
//! SHA-256 hash is kept: in memory for synchronous lookups and in `db/sessions.db` so
//! sessions survive restarts. Expiry slides: a session used at least
//! [`REFRESH_INTERVAL`] after its last refresh is extended by the configured TTL, which
//! keeps writes to roughly one per session per hour. Database writes go through a single
//! task, in order, so a refresh can never resurrect a deleted session.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::Role;
use crate::db::Database;

/// Name of the session cookie.
pub const COOKIE_NAME: &str = "slskr_session";
/// Minimum time between two expiry refreshes of one session.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3_600);
/// How often expired sessions are removed.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(3_600);
/// Default session lifetime (`web.session_ttl`).
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 86_400);

const TOKEN_BYTES: usize = 32;

const MIGRATIONS: &[&str] = &["
CREATE TABLE sessions (
    token_hash BLOB PRIMARY KEY NOT NULL,
    username TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);
CREATE INDEX sessions_expires_at ON sessions (expires_at);
CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value BLOB NOT NULL);
"];

type TokenHash = [u8; 32];

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub username: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

enum Write {
    Insert(TokenHash, SessionRecord),
    Touch {
        hash: TokenHash,
        expires_at: DateTime<Utc>,
        last_seen_at: DateTime<Utc>,
    },
    Delete(TokenHash),
    /// Credentials changed: drop every session and remember the new fingerprint.
    Reset(Vec<u8>),
    Prune(DateTime<Utc>),
    Flush,
}

type Command = (Write, Option<oneshot::Sender<anyhow::Result<()>>>);

struct Inner {
    sessions: RwLock<HashMap<TokenHash, SessionRecord>>,
    ttl_ms: AtomicU64,
    /// Salt of the credential fingerprint stored next to the sessions.
    salt: [u8; 16],
    /// `None` for memory-only stores.
    writer: Option<mpsc::UnboundedSender<Command>>,
}

/// Session registry; cheap to clone.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Inner>,
}

fn timestamp(time: DateTime<Utc>) -> String {
    // Fixed width, so SQL text comparison orders like time.
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap_or(TimeDelta::MAX)
}

/// SHA-256 of a presented token; `None` unless it is a well-formed token.
fn hash_token(token: &str) -> Option<TokenHash> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.trim())
        .ok()
        .filter(|bytes| bytes.len() == TOKEN_BYTES)?;
    Some(Sha256::digest(bytes).into())
}

/// Salted digest identifying the web credentials the sessions were issued for.
fn fingerprint(salt: &[u8], username: &str, password: &str) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(salt);
    digest.update((username.len() as u64).to_le_bytes());
    digest.update(username.as_bytes());
    digest.update(password.as_bytes());
    digest.finalize().to_vec()
}

fn apply(connection: &mut rusqlite::Connection, write: &Write) -> rusqlite::Result<()> {
    match write {
        Write::Insert(hash, record) => {
            connection.execute(
                "INSERT OR REPLACE INTO sessions
                 (token_hash, username, role, created_at, expires_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &hash[..],
                    record.username,
                    record.role.as_str(),
                    timestamp(record.created_at),
                    timestamp(record.expires_at),
                    timestamp(record.last_seen_at),
                ],
            )?;
        }
        Write::Touch {
            hash,
            expires_at,
            last_seen_at,
        } => {
            connection.execute(
                "UPDATE sessions SET expires_at = ?2, last_seen_at = ?3 WHERE token_hash = ?1",
                params![&hash[..], timestamp(*expires_at), timestamp(*last_seen_at)],
            )?;
        }
        Write::Delete(hash) => {
            connection.execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                params![&hash[..]],
            )?;
        }
        Write::Reset(fingerprint) => {
            let transaction = connection.transaction()?;
            transaction.execute("DELETE FROM sessions", [])?;
            transaction.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('credentials', ?1)",
                params![fingerprint],
            )?;
            transaction.commit()?;
        }
        Write::Prune(now) => {
            connection.execute(
                "DELETE FROM sessions WHERE expires_at <= ?1",
                params![timestamp(*now)],
            )?;
        }
        Write::Flush => {}
    }
    Ok(())
}

async fn run_writer(database: Database, mut commands: mpsc::UnboundedReceiver<Command>) {
    while let Some((write, done)) = commands.recv().await {
        let result = database
            .call(move |connection| apply(connection, &write))
            .await;
        match done {
            Some(done) => {
                let _ = done.send(result);
            }
            None => {
                if let Err(error) = result {
                    warn!(%error, "could not update sessions database");
                }
            }
        }
    }
}

impl SessionStore {
    fn with(
        sessions: HashMap<TokenHash, SessionRecord>,
        ttl: Duration,
        salt: [u8; 16],
        writer: Option<mpsc::UnboundedSender<Command>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: RwLock::new(sessions),
                ttl_ms: AtomicU64::new(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)),
                salt,
                writer,
            }),
        }
    }

    /// A store that keeps sessions in memory only (tests, tools).
    pub fn memory(ttl: Duration) -> Self {
        Self::with(HashMap::new(), ttl, rand::random(), None)
    }

    /// Opens (or creates) the sessions database and loads unexpired sessions. Sessions
    /// issued for other web credentials than `username`/`password` are discarded.
    pub async fn open(
        path: &Path,
        ttl: Duration,
        username: &str,
        password: &str,
    ) -> anyhow::Result<Self> {
        let database = Database::open(path, MIGRATIONS)?;
        let (username, password) = (username.to_owned(), password.to_owned());
        let now = Utc::now();
        let (salt, sessions) = database
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let stored_salt: Option<Vec<u8>> = transaction
                    .query_row("SELECT value FROM meta WHERE key = 'salt'", [], |row| {
                        row.get(0)
                    })
                    .optional()?;
                let salt: [u8; 16] = match stored_salt.and_then(|salt| salt.try_into().ok()) {
                    Some(salt) => salt,
                    None => {
                        let salt: [u8; 16] = rand::random();
                        transaction.execute(
                            "INSERT OR REPLACE INTO meta (key, value) VALUES ('salt', ?1)",
                            params![&salt[..]],
                        )?;
                        salt
                    }
                };
                let expected = fingerprint(&salt, &username, &password);
                let stored: Option<Vec<u8>> = transaction
                    .query_row(
                        "SELECT value FROM meta WHERE key = 'credentials'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                if stored.as_deref() != Some(expected.as_slice()) {
                    transaction.execute("DELETE FROM sessions", [])?;
                    transaction.execute(
                        "INSERT OR REPLACE INTO meta (key, value) VALUES ('credentials', ?1)",
                        params![expected],
                    )?;
                }
                transaction.execute(
                    "DELETE FROM sessions WHERE expires_at <= ?1",
                    params![timestamp(now)],
                )?;
                let mut sessions = HashMap::new();
                {
                    let mut statement = transaction.prepare(
                        "SELECT token_hash, username, role, created_at, expires_at, last_seen_at
                         FROM sessions",
                    )?;
                    let rows = statement.query_map([], |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })?;
                    for row in rows {
                        let (hash, username, role, created, expires, seen) = row?;
                        let record = (|| {
                            Some(SessionRecord {
                                username,
                                role: role.parse().ok()?,
                                created_at: parse_timestamp(&created)?,
                                expires_at: parse_timestamp(&expires)?,
                                last_seen_at: parse_timestamp(&seen)?,
                            })
                        })();
                        if let (Ok(hash), Some(record)) = (TokenHash::try_from(hash), record) {
                            sessions.insert(hash, record);
                        }
                    }
                }
                transaction.commit()?;
                Ok((salt, sessions))
            })
            .await?;
        let (writer, commands) = mpsc::unbounded_channel();
        tokio::spawn(run_writer(database, commands));
        Ok(Self::with(sessions, ttl, salt, Some(writer)))
    }

    /// Lifetime given to new and refreshed sessions.
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.inner.ttl_ms.load(Ordering::Relaxed))
    }

    pub fn set_ttl(&self, ttl: Duration) {
        self.inner.ttl_ms.store(
            u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn sessions(&self) -> std::sync::RwLockReadGuard<'_, HashMap<TokenHash, SessionRecord>> {
        self.inner
            .sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<TokenHash, SessionRecord>> {
        self.inner
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, write: Write) {
        if let Some(writer) = &self.inner.writer {
            let _ = writer.send((write, None));
        }
    }

    async fn send_and_wait(&self, write: Write) -> anyhow::Result<()> {
        let Some(writer) = &self.inner.writer else {
            return Ok(());
        };
        let (done, wait) = oneshot::channel();
        writer
            .send((write, Some(done)))
            .map_err(|_| anyhow::anyhow!("sessions database writer stopped"))?;
        wait.await
            .map_err(|_| anyhow::anyhow!("sessions database writer stopped"))?
    }

    /// Creates a session and returns its token.
    pub async fn create(&self, username: &str, role: Role) -> anyhow::Result<String> {
        self.create_at(username, role, Utc::now()).await
    }

    async fn create_at(
        &self,
        username: &str,
        role: Role,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        let raw: [u8; TOKEN_BYTES] = rand::random();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let hash: TokenHash = Sha256::digest(raw).into();
        let record = SessionRecord {
            username: username.to_owned(),
            role,
            created_at: now,
            expires_at: now + delta(self.ttl()),
            last_seen_at: now,
        };
        self.send_and_wait(Write::Insert(hash, record.clone()))
            .await?;
        self.sessions_mut().insert(hash, record);
        Ok(token)
    }

    /// The unexpired session for `token`, without refreshing it.
    pub fn lookup(&self, token: &str) -> Option<SessionRecord> {
        self.lookup_at(token, Utc::now())
    }

    fn lookup_at(&self, token: &str, now: DateTime<Utc>) -> Option<SessionRecord> {
        let hash = hash_token(token)?;
        let record = self.sessions().get(&hash).cloned()?;
        if record.expires_at > now {
            return Some(record);
        }
        if self.sessions_mut().remove(&hash).is_some() {
            self.send(Write::Delete(hash));
        }
        None
    }

    /// Extends an unexpired session when its last refresh is at least
    /// [`REFRESH_INTERVAL`] old. Returns true when it was extended (re-send the cookie).
    pub fn touch(&self, token: &str) -> bool {
        self.touch_at(token, Utc::now())
    }

    fn touch_at(&self, token: &str, now: DateTime<Utc>) -> bool {
        let Some(hash) = hash_token(token) else {
            return false;
        };
        let due = |record: &SessionRecord| {
            record.expires_at > now && now - record.last_seen_at >= delta(REFRESH_INTERVAL)
        };
        if !self.sessions().get(&hash).is_some_and(due) {
            return false;
        }
        let expires_at = now + delta(self.ttl());
        {
            let mut sessions = self.sessions_mut();
            let Some(record) = sessions.get_mut(&hash).filter(|record| due(record)) else {
                return false;
            };
            record.expires_at = expires_at;
            record.last_seen_at = now;
        }
        self.send(Write::Touch {
            hash,
            expires_at,
            last_seen_at: now,
        });
        true
    }

    /// Deletes the session for `token`, if any.
    pub async fn remove(&self, token: &str) -> anyhow::Result<()> {
        let Some(hash) = hash_token(token) else {
            return Ok(());
        };
        if self.sessions_mut().remove(&hash).is_some() {
            self.send_and_wait(Write::Delete(hash)).await?;
        }
        Ok(())
    }

    /// Invalidates every session because the web credentials changed.
    pub fn reset(&self, username: &str, password: &str) {
        let count = {
            let mut sessions = self.sessions_mut();
            let count = sessions.len();
            sessions.clear();
            count
        };
        if count > 0 {
            tracing::info!(count, "web credentials changed; signed out every session");
        }
        self.send(Write::Reset(fingerprint(
            &self.inner.salt,
            username,
            password,
        )));
    }

    /// Removes expired sessions; returns how many were removed from memory.
    pub fn prune(&self) -> usize {
        self.prune_at(Utc::now())
    }

    fn prune_at(&self, now: DateTime<Utc>) -> usize {
        let removed = {
            let mut sessions = self.sessions_mut();
            let before = sessions.len();
            sessions.retain(|_, record| record.expires_at > now);
            before - sessions.len()
        };
        self.send(Write::Prune(now));
        removed
    }

    /// Number of live sessions in memory (including not yet pruned expired ones).
    pub fn len(&self) -> usize {
        self.sessions().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Moves a session's timestamps `by` into the past.
    #[cfg(test)]
    pub(crate) fn backdate(&self, token: &str, by: TimeDelta) {
        let hash = hash_token(token).expect("token");
        if let Some(record) = self.sessions_mut().get_mut(&hash) {
            record.created_at -= by;
            record.last_seen_at -= by;
            record.expires_at -= by;
        }
    }

    /// Waits until every write queued so far reached the database.
    pub async fn flush(&self) -> anyhow::Result<()> {
        self.send_and_wait(Write::Flush).await
    }

    /// Prunes expired sessions every [`PRUNE_INTERVAL`] until `shutdown`.
    pub fn spawn_pruner(&self, shutdown: CancellationToken) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(PRUNE_INTERVAL);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = timer.tick() => {
                        let removed = store.prune();
                        if removed > 0 {
                            tracing::debug!(removed, "pruned expired sessions");
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(30 * 86_400);

    fn hours(n: i64) -> TimeDelta {
        TimeDelta::hours(n)
    }

    #[tokio::test]
    async fn create_lookup_and_remove() {
        let store = SessionStore::memory(TTL);
        let token = store.create("admin", Role::Administrator).await.unwrap();
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&token)
                .unwrap()
                .len(),
            32
        );
        let record = store.lookup(&token).unwrap();
        assert_eq!(record.username, "admin");
        assert_eq!(record.role, Role::Administrator);
        assert_eq!(record.expires_at - record.created_at, delta(TTL));

        let other = store.create("admin", Role::Administrator).await.unwrap();
        assert_ne!(token, other);
        assert!(store.lookup("garbage").is_none());
        assert!(store.lookup("").is_none());
        let mut tampered = token.clone().into_bytes();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        assert!(
            store
                .lookup(std::str::from_utf8(&tampered).unwrap())
                .is_none()
        );

        store.remove(&token).await.unwrap();
        assert!(store.lookup(&token).is_none());
        assert!(store.lookup(&other).is_some());
    }

    #[tokio::test]
    async fn expiry_and_sliding_refresh() {
        let store = SessionStore::memory(Duration::from_secs(10 * 3_600));
        let start = Utc::now();
        let token = store
            .create_at("admin", Role::Administrator, start)
            .await
            .unwrap();

        // Not due within the refresh interval.
        assert!(!store.touch_at(&token, start + TimeDelta::minutes(59)));
        let record = store.lookup_at(&token, start).unwrap();
        assert_eq!(record.expires_at, start + hours(10));

        // Due after an hour: expiry slides from the time of use.
        assert!(store.touch_at(&token, start + hours(2)));
        let record = store.lookup_at(&token, start).unwrap();
        assert_eq!(record.last_seen_at, start + hours(2));
        assert_eq!(record.expires_at, start + hours(12));
        // Only once per interval.
        assert!(!store.touch_at(&token, start + hours(2) + TimeDelta::minutes(30)));

        // Valid just before expiry, gone at it, and never refreshed once expired.
        assert!(store.lookup_at(&token, start + hours(11)).is_some());
        assert!(!store.touch_at(&token, start + hours(12)));
        assert!(store.lookup_at(&token, start + hours(12)).is_none());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn prune_removes_only_expired() {
        let store = SessionStore::memory(Duration::from_secs(3_600));
        let start = Utc::now();
        let old = store
            .create_at("a", Role::Administrator, start)
            .await
            .unwrap();
        let new = store
            .create_at("a", Role::Administrator, start + hours(1))
            .await
            .unwrap();
        assert_eq!(store.prune_at(start + hours(1)), 1);
        assert!(store.lookup_at(&old, start).is_none());
        assert!(store.lookup_at(&new, start + hours(1)).is_some());
    }

    #[tokio::test]
    async fn persists_across_reopen_and_resets_on_credential_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");

        let store = SessionStore::open(&path, TTL, "admin", "one")
            .await
            .unwrap();
        let token = store.create("admin", Role::Administrator).await.unwrap();
        let removed = store.create("admin", Role::Administrator).await.unwrap();
        store.remove(&removed).await.unwrap();
        let start = Utc::now() + hours(2);
        assert!(store.touch_at(&token, start));
        store.flush().await.unwrap();
        drop(store);

        // Same credentials: the session survives, with its refreshed expiry.
        let store = SessionStore::open(&path, TTL, "admin", "one")
            .await
            .unwrap();
        let record = store.lookup(&token).unwrap();
        assert_eq!(timestamp(record.last_seen_at), timestamp(start));
        assert!(store.lookup(&removed).is_none());

        // Hot reload of the credentials signs everyone out, persistently.
        store.reset("admin", "two");
        assert!(store.lookup(&token).is_none());
        let fresh = store.create("admin", Role::Administrator).await.unwrap();
        store.flush().await.unwrap();
        drop(store);
        let store = SessionStore::open(&path, TTL, "admin", "two")
            .await
            .unwrap();
        assert!(store.lookup(&token).is_none());
        assert!(store.lookup(&fresh).is_some());
        drop(store);

        // Credentials changed while the daemon was stopped.
        let store = SessionStore::open(&path, TTL, "admin", "three")
            .await
            .unwrap();
        assert!(store.lookup(&fresh).is_none());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn expired_sessions_are_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = SessionStore::open(&path, Duration::from_secs(300), "a", "b")
            .await
            .unwrap();
        let token = store
            .create_at("a", Role::Administrator, Utc::now() - hours(1))
            .await
            .unwrap();
        store.flush().await.unwrap();
        drop(store);
        let store = SessionStore::open(&path, Duration::from_secs(300), "a", "b")
            .await
            .unwrap();
        assert!(store.lookup(&token).is_none());
        assert!(store.is_empty());
    }
}
