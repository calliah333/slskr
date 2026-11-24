use super::Database;
use std::path::Path;

#[tokio::test]
async fn failed_migration_rolls_back_schema_and_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    assert!(Database::open(&path, &["CREATE TABLE retained (id INTEGER); INVALID SQL;"]).is_err());

    let db = Database::open(&path, &[]).unwrap();
    let table_count: i64 = db
        .call(|connection| {
            connection.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'retained'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(table_count, 0);
    let version: i64 = db
        .call(|connection| connection.pragma_query_value(None, "user_version", |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(version, 0);
}

#[tokio::test]
async fn file_databases_use_write_ahead_logging() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wal.db");
    // An existing rollback-journal database at schema version 1.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE item (value INTEGER); PRAGMA user_version = 1;")
        .unwrap();
    let migrations = [
        "CREATE TABLE item (value INTEGER);",
        "INSERT INTO item VALUES (3);",
    ];

    for _ in 0..2 {
        let db = Database::open(&path, &migrations).unwrap();
        let (mode, synchronous, busy_timeout, foreign_keys, value) = db
            .call(|connection| {
                let pragma = |name: &str| {
                    connection.pragma_query_value(None, name, |row| row.get::<_, i64>(0))
                };
                Ok((
                    connection
                        .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?,
                    pragma("synchronous")?,
                    pragma("busy_timeout")?,
                    pragma("foreign_keys")?,
                    connection.query_row("SELECT sum(value) FROM item", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 1, "synchronous = NORMAL");
        assert_eq!(busy_timeout, 5_000);
        assert_eq!(foreign_keys, 1);
        // The pending migration ran once, after switching to WAL.
        assert_eq!(value, 3);
    }
}

#[tokio::test]
async fn migration_versions_apply_once() {
    let db = Database::open(
        Path::new(":memory:"),
        &["CREATE TABLE item (value INTEGER);"],
    )
    .unwrap();
    let mode: String = db
        .call(|connection| connection.pragma_query_value(None, "journal_mode", |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(mode, "memory");
    db.call(|connection| connection.execute("INSERT INTO item VALUES (7)", []))
        .await
        .unwrap();
    let value: i64 = db
        .call(|connection| connection.query_row("SELECT value FROM item", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(value, 7);
}

#[tokio::test]
async fn reader_sees_commits_without_waiting_for_the_writer() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(
        &directory.path().join("reader.db"),
        &["CREATE TABLE item (value INTEGER); INSERT INTO item VALUES (1);"],
    )
    .unwrap();
    let reader = db.reader().unwrap();
    async fn count(db: &Database) -> anyhow::Result<i64> {
        db.call(|connection| {
            connection.query_row("SELECT count(*) FROM item", [], |row| row.get(0))
        })
        .await
    }

    // The writer holds its connection inside an open transaction until released.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (started, writing) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn({
        let db = db.clone();
        async move {
            db.call(move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute("INSERT INTO item VALUES (2)", [])?;
                started.send(()).unwrap();
                held.recv().unwrap();
                transaction.commit()
            })
            .await
        }
    });
    writing.await.unwrap();
    let during = tokio::time::timeout(std::time::Duration::from_secs(5), count(&reader))
        .await
        .expect("the reader waited for the writer")
        .unwrap();
    assert_eq!(during, 1, "uncommitted rows are not visible");
    release.send(()).unwrap();
    writer.await.unwrap().unwrap();
    assert_eq!(count(&reader).await.unwrap(), 2);

    let write = reader
        .call(|connection| connection.execute("INSERT INTO item VALUES (3)", []))
        .await;
    assert!(write.is_err(), "the reader is read-only");
}

#[tokio::test]
async fn in_memory_reader_is_the_same_database() {
    let db = Database::open(
        Path::new(":memory:"),
        &["CREATE TABLE item (value INTEGER);"],
    )
    .unwrap();
    db.reader()
        .unwrap()
        .call(|connection| connection.execute("INSERT INTO item VALUES (1)", []))
        .await
        .unwrap();
    let count: i64 = db
        .call(|connection| connection.query_row("SELECT count(*) FROM item", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn existing_files_switch_to_incremental_vacuum_and_return_free_pages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vacuum.db");
    // A database written before incremental vacuum, with 4 MB of rows.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE item (value BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
             INSERT INTO item SELECT zeroblob(4096) FROM n;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    let db = Database::open(&path, &["CREATE TABLE item (value BLOB);"]).unwrap();
    let pragma = |name: &'static str| {
        let db = db.clone();
        async move {
            db.call(move |connection| {
                connection.pragma_query_value(None, name, |row| row.get::<_, i64>(0))
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(pragma("auto_vacuum").await, 2);
    assert_eq!(
        db.call(|connection| connection
            .query_row("SELECT count(*) FROM item", [], |row| row.get::<_, i64>(0)))
            .await
            .unwrap(),
        1000,
        "VACUUM keeps the rows"
    );

    db.call(|connection| connection.execute("DELETE FROM item", []))
        .await
        .unwrap();
    assert!(pragma("freelist_count").await > 900);
    db.maintain_now().await.unwrap();
    assert_eq!(pragma("freelist_count").await, 0);
    assert!(pragma("page_count").await < 50);
}
