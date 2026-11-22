use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use serde::{Serialize, de::DeserializeOwned};
use slskr_api::search::{Search, SearchStates, SoulseekSearchResponse};

pub(super) const MIGRATIONS: &[&str] = &[
    "CREATE TABLE searches (id TEXT PRIMARY KEY, record TEXT NOT NULL, responses TEXT NOT NULL DEFAULT '[]');",
    "CREATE TABLE search_responses (
         sequence INTEGER PRIMARY KEY,
         search_id TEXT NOT NULL REFERENCES searches(id) ON DELETE CASCADE,
         response TEXT NOT NULL
     );
     CREATE INDEX search_responses_search_id ON search_responses(search_id);
     INSERT INTO search_responses(search_id, response)
         SELECT searches.id, result.value FROM searches, json_each(searches.responses) AS result ORDER BY searches.rowid, result.key;
     ALTER TABLE searches DROP COLUMN responses;",
];

pub(super) fn load(db: &mut Connection) -> rusqlite::Result<Vec<Search>> {
    let transaction = db.transaction()?;
    let mut searches: Vec<Search> = {
        let mut statement = transaction.prepare("SELECT record FROM searches")?;
        statement
            .query_map([], decode)?
            .collect::<rusqlite::Result<_>>()?
    };
    let now = Utc::now();
    for search in &mut searches {
        if search.ended_at.is_none() || !search.state.contains(SearchStates::COMPLETED) {
            // An unclean exit can interrupt a search, but its committed results are still available.
            search.ended_at = Some(now);
            search.set_state(SearchStates::COMPLETED | SearchStates::TIMED_OUT);
            update(&transaction, search)?;
        }
    }
    transaction.commit()?;
    Ok(searches)
}

pub(super) fn insert(db: &Connection, search: &Search) -> rusqlite::Result<bool> {
    Ok(db.execute(
        "INSERT INTO searches(id, record) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING",
        params![search.id, encode(search)?],
    )? != 0)
}

pub(super) fn append(
    db: &mut Connection,
    id: &str,
    response: &SoulseekSearchResponse,
    [responses, files, locked_files]: [u32; 3],
) -> rusqlite::Result<()> {
    let transaction = db.transaction()?;
    transaction.execute(
        "INSERT INTO search_responses(search_id, response) VALUES (?1, ?2)",
        params![id, encode(response)?],
    )?;
    transaction.execute(
        "UPDATE searches SET record = json_set(record, '$.responseCount', ?2, '$.fileCount', ?3, '$.lockedFileCount', ?4)
         WHERE id = ?1",
        params![id, responses, files, locked_files],
    )?;
    transaction.commit()
}

pub(super) fn update(db: &Connection, search: &Search) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE searches SET record = ?2 WHERE id = ?1",
        params![search.id, encode(search)?],
    )?;
    Ok(())
}

pub(super) fn get(
    db: &Connection,
    id: &str,
    include_responses: bool,
) -> rusqlite::Result<Option<Search>> {
    let Some(mut search) = db
        .query_row(
            "SELECT record FROM searches WHERE id = ?1",
            [id],
            decode::<Search>,
        )
        .optional()?
    else {
        return Ok(None);
    };
    if include_responses {
        let mut statement = db.prepare(
            "SELECT response FROM search_responses WHERE search_id = ?1 ORDER BY sequence",
        )?;
        search.responses = Some(
            statement
                .query_map([id], decode)?
                .collect::<rusqlite::Result<_>>()?,
        );
    }
    Ok(Some(search))
}

pub(super) fn search_text(db: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    db.query_row(
        "SELECT json_extract(record, '$.searchText') FROM searches WHERE id = ?1",
        [id],
        |row| row.get(0),
    )
    .optional()
}

pub(super) fn remove(db: &Connection, id: &str) -> rusqlite::Result<()> {
    db.execute("DELETE FROM searches WHERE id = ?1", [id])?;
    Ok(())
}

fn encode(value: &impl Serialize) -> rusqlite::Result<String> {
    serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn decode<T: DeserializeOwned>(row: &Row<'_>) -> rusqlite::Result<T> {
    let value = row.get_ref(0)?;
    let text = value.as_str().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, value.data_type(), Box::new(error))
    })?;
    serde_json::from_str(text)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error)))
}
