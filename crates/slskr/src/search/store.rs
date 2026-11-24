use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use serde::{Serialize, de::DeserializeOwned};
use slskr_types::search::{Search, SearchResponse, SearchState};

/// `searches.record` and `search_responses.response` hold the API JSON of [`Search`] and
/// [`SearchResponse`].
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
    // From the slskd-shaped JSON (camelCase, flags strings) to the slskr API shape.
    r#"UPDATE searches SET record = json_object(
         'id', record ->> '$.id',
         'query', record ->> '$.searchText',
         'state', CASE
             WHEN record ->> '$.state' LIKE '%Cancelled%' THEN 'cancelled'
             WHEN record ->> '$.state' LIKE '%Errored%' THEN 'errored'
             WHEN record ->> '$.state' LIKE '%ResponseLimitReached%' THEN 'response_limit_reached'
             WHEN record ->> '$.state' LIKE '%FileLimitReached%' THEN 'file_limit_reached'
             WHEN record ->> '$.state' LIKE '%TimedOut%' THEN 'timed_out'
             WHEN record ->> '$.state' LIKE '%Completed%' THEN 'completed'
             WHEN record ->> '$.state' LIKE '%InProgress%' THEN 'in_progress'
             WHEN record ->> '$.state' LIKE '%Queued%' THEN 'queued'
             ELSE 'requested' END,
         'token', coalesce(record ->> '$.token', 0),
         'started_at', record ->> '$.startedAt',
         'ended_at', record ->> '$.endedAt',
         'response_count', coalesce(record ->> '$.responseCount', 0),
         'file_count', coalesce(record ->> '$.fileCount', 0),
         'locked_file_count', coalesce(record ->> '$.lockedFileCount', 0));
     UPDATE search_responses SET response = json_object(
         'username', response ->> '$.username',
         'token', coalesce(response ->> '$.token', 0),
         'has_free_upload_slot', json(CASE WHEN response ->> '$.hasFreeUploadSlot' THEN 'true' ELSE 'false' END),
         'upload_speed', coalesce(response ->> '$.uploadSpeed', 0),
         'queue_length', coalesce(response ->> '$.queueLength', 0),
         'files', (SELECT json_group_array(json_object(
                 'filename', file.value ->> '$.filename',
                 'size', coalesce(file.value ->> '$.size', 0),
                 'extension', coalesce(file.value ->> '$.extension', ''),
                 'bit_rate', file.value ->> '$.bitRate',
                 'bit_depth', file.value ->> '$.bitDepth',
                 'sample_rate', file.value ->> '$.sampleRate',
                 'duration_ms', (file.value ->> '$.length') * 1000,
                 'is_variable_bit_rate', json(CASE file.value ->> '$.isVariableBitRate' WHEN 1 THEN 'true' WHEN 0 THEN 'false' ELSE 'null' END)))
             FROM json_each(response, '$.files') AS file),
         'locked_files', (SELECT json_group_array(json_object(
                 'filename', file.value ->> '$.filename',
                 'size', coalesce(file.value ->> '$.size', 0),
                 'extension', coalesce(file.value ->> '$.extension', ''),
                 'bit_rate', file.value ->> '$.bitRate',
                 'bit_depth', file.value ->> '$.bitDepth',
                 'sample_rate', file.value ->> '$.sampleRate',
                 'duration_ms', (file.value ->> '$.length') * 1000,
                 'is_variable_bit_rate', json(CASE file.value ->> '$.isVariableBitRate' WHEN 1 THEN 'true' WHEN 0 THEN 'false' ELSE 'null' END)))
             FROM json_each(response, '$.lockedFiles') AS file));"#,
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
        if search.ended_at.is_none() || !search.state.is_complete() {
            // An unclean exit can interrupt a search, but its committed results are still available.
            search.state = SearchState::TimedOut;
            search.ended_at = Some(now);
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

/// Stores `responses` in arrival order and the search's resulting `counts`, in one
/// transaction.
pub(super) fn append_many(
    db: &mut Connection,
    id: &str,
    responses: &[SearchResponse],
    [response_count, files, locked_files]: [u32; 3],
) -> rusqlite::Result<()> {
    let transaction = db.transaction()?;
    {
        let mut insert = transaction
            .prepare_cached("INSERT INTO search_responses(search_id, response) VALUES (?1, ?2)")?;
        for response in responses {
            insert.execute(params![id, encode(response)?])?;
        }
        transaction
            .prepare_cached(
                "UPDATE searches SET record = json_set(record, '$.response_count', ?2, '$.file_count', ?3, '$.locked_file_count', ?4)
                 WHERE id = ?1",
            )?
            .execute(params![id, response_count, files, locked_files])?;
    }
    transaction.commit()
}

pub(super) fn update(db: &Connection, search: &Search) -> rusqlite::Result<()> {
    db.prepare_cached("UPDATE searches SET record = ?2 WHERE id = ?1")?
        .execute(params![search.id, encode(search)?])?;
    Ok(())
}

pub(super) fn get(db: &Connection, id: &str) -> rusqlite::Result<Option<Search>> {
    db.prepare_cached("SELECT record FROM searches WHERE id = ?1")?
        .query_row([id], decode::<Search>)
        .optional()
}

/// The API JSON array of a search's responses in arrival order, assembled from the stored
/// JSON without decoding it; `None` when there is no such search.
pub(super) fn responses_json(db: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    let exists = db
        .prepare_cached("SELECT 1 FROM searches WHERE id = ?1")?
        .exists([id])?;
    if !exists {
        return Ok(None);
    }
    let mut statement = db.prepare_cached(
        "SELECT response FROM search_responses WHERE search_id = ?1 ORDER BY sequence",
    )?;
    let mut rows = statement.query([id])?;
    let mut json = String::from("[");
    while let Some(row) = rows.next()? {
        let value = row.get_ref(0)?;
        let text = value.as_str().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, value.data_type(), Box::new(error))
        })?;
        if json.len() > 1 {
            json.push(',');
        }
        json.push_str(text);
    }
    json.push(']');
    Ok(Some(json))
}

pub(super) fn search_text(db: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    db.prepare_cached("SELECT json_extract(record, '$.query') FROM searches WHERE id = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()
}

pub(super) fn remove(db: &Connection, id: &str) -> rusqlite::Result<()> {
    db.prepare_cached("DELETE FROM searches WHERE id = ?1")?
        .execute([id])?;
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
