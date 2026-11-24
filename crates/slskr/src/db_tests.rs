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
