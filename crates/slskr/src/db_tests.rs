use super::Database;
use std::path::Path;

#[tokio::test]
async fn failed_migration_rolls_back_schema_and_version() {
    let path = std::env::temp_dir().join(format!("slskr-db-{}.db", uuid::Uuid::new_v4()));
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
    drop(db);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn migration_versions_apply_once() {
    let db = Database::open(
        Path::new(":memory:"),
        &["CREATE TABLE item (value INTEGER);"],
    )
    .unwrap();
    db.call(|connection| connection.execute("INSERT INTO item VALUES (7)", []))
        .await
        .unwrap();
    let value: i64 = db
        .call(|connection| connection.query_row("SELECT value FROM item", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(value, 7);
}
