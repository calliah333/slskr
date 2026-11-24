-- Transfer times as INTEGER nanoseconds since the Unix epoch instead of RFC 3339 TEXT:
-- reports subtract and bucket them directly instead of parsing every row. A column's
-- type cannot change in place, so the table is rebuilt (keeping rowids, which order the
-- lists), with its indexes and the chart triggers.
CREATE TABLE transfers_new (
    id TEXT PRIMARY KEY, batch_id TEXT, username TEXT NOT NULL, direction TEXT NOT NULL,
    filename TEXT NOT NULL, size INTEGER NOT NULL, start_offset INTEGER NOT NULL DEFAULT 0,
    state INTEGER NOT NULL, state_description TEXT, requested_at INTEGER NOT NULL,
    enqueued_at INTEGER, started_at INTEGER, ended_at INTEGER,
    bytes_transferred INTEGER NOT NULL DEFAULT 0, average_speed REAL NOT NULL DEFAULT 0,
    exception TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER,
    removed INTEGER NOT NULL DEFAULT 0, place_in_queue INTEGER, root TEXT
);

-- Whole seconds from unixepoch(), which also applies a UTC offset; the fraction from the
-- text, padded to nine digits. Unparseable times become NULL (0 when required).
INSERT INTO transfers_new (rowid, id, batch_id, username, direction, filename, size, start_offset,
    state, state_description, requested_at, enqueued_at, started_at, ended_at, bytes_transferred,
    average_speed, exception, attempts, next_attempt_at, removed, place_in_queue, root)
SELECT rowid, id, batch_id, username, direction, filename, size, start_offset,
    state, state_description,
    coalesce(CASE WHEN requested_at IS NULL THEN NULL ELSE unixepoch(requested_at) * 1000000000 + CASE WHEN substr(requested_at, 20, 1) = '.' THEN CAST(substr(substr(requested_at, 21, length(requested_at) - 20 - CASE WHEN requested_at LIKE '%Z' THEN 1 ELSE 6 END) || '000000000', 1, 9) AS INTEGER) ELSE 0 END END, 0),
    CASE WHEN enqueued_at IS NULL THEN NULL ELSE unixepoch(enqueued_at) * 1000000000 + CASE WHEN substr(enqueued_at, 20, 1) = '.' THEN CAST(substr(substr(enqueued_at, 21, length(enqueued_at) - 20 - CASE WHEN enqueued_at LIKE '%Z' THEN 1 ELSE 6 END) || '000000000', 1, 9) AS INTEGER) ELSE 0 END END,
    CASE WHEN started_at IS NULL THEN NULL ELSE unixepoch(started_at) * 1000000000 + CASE WHEN substr(started_at, 20, 1) = '.' THEN CAST(substr(substr(started_at, 21, length(started_at) - 20 - CASE WHEN started_at LIKE '%Z' THEN 1 ELSE 6 END) || '000000000', 1, 9) AS INTEGER) ELSE 0 END END,
    CASE WHEN ended_at IS NULL THEN NULL ELSE unixepoch(ended_at) * 1000000000 + CASE WHEN substr(ended_at, 20, 1) = '.' THEN CAST(substr(substr(ended_at, 21, length(ended_at) - 20 - CASE WHEN ended_at LIKE '%Z' THEN 1 ELSE 6 END) || '000000000', 1, 9) AS INTEGER) ELSE 0 END END,
    bytes_transferred, average_speed, exception, attempts,
    CASE WHEN next_attempt_at IS NULL THEN NULL ELSE unixepoch(next_attempt_at) * 1000000000 + CASE WHEN substr(next_attempt_at, 20, 1) = '.' THEN CAST(substr(substr(next_attempt_at, 21, length(next_attempt_at) - 20 - CASE WHEN next_attempt_at LIKE '%Z' THEN 1 ELSE 6 END) || '000000000', 1, 9) AS INTEGER) ELSE 0 END END,
    removed, place_in_queue, root
FROM transfers;

DROP TABLE transfers;
ALTER TABLE transfers_new RENAME TO transfers;

CREATE INDEX ix_transfers_username ON transfers(username);
CREATE INDEX ix_transfers_batch_id ON transfers(batch_id);
CREATE INDEX ix_transfers_file ON transfers(direction, username, filename);
CREATE INDEX ix_transfers_recent ON transfers(direction, ended_at DESC) WHERE (state & 16) != 0;
CREATE INDEX ix_transfers_active ON transfers(direction) WHERE (state & 16) = 0;
CREATE INDEX ix_transfers_report_state_end ON transfers(state, ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX ix_transfers_report_errors_end ON transfers(direction, ended_at)
    WHERE (state & ~48) != 0 AND ended_at IS NOT NULL;
CREATE INDEX ix_transfers_requested_at ON transfers(requested_at);

-- transfer_chart_hours keeps its rows: an hour is still ended_at's whole hour in seconds.
CREATE TRIGGER transfer_chart_insert AFTER INSERT ON transfers
WHEN NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND NEW.ended_at IS NOT NULL
BEGIN
    INSERT INTO transfer_chart_hours VALUES (NEW.ended_at / 3600000000000 * 3600, NEW.direction, NEW.state, NEW.size, 1)
    ON CONFLICT (hour, direction, state) DO UPDATE SET bytes = bytes + excluded.bytes, files = files + 1;
END;

CREATE TRIGGER transfer_chart_delete AFTER DELETE ON transfers
WHEN OLD.state IN (16, 48, 80, 144, 272, 528, 1040) AND OLD.ended_at IS NOT NULL
BEGIN
    UPDATE transfer_chart_hours SET bytes = bytes - OLD.size, files = files - 1
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction AND state = OLD.state;
    DELETE FROM transfer_chart_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction AND state = OLD.state AND files = 0;
END;

CREATE TRIGGER transfer_chart_update AFTER UPDATE OF ended_at, direction, state, size ON transfers
WHEN OLD.ended_at IS NOT NEW.ended_at OR OLD.direction != NEW.direction OR OLD.state != NEW.state OR OLD.size != NEW.size
BEGIN
    UPDATE transfer_chart_hours SET bytes = bytes - OLD.size, files = files - 1
    WHERE OLD.state IN (16, 48, 80, 144, 272, 528, 1040)
      AND hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction AND state = OLD.state;
    DELETE FROM transfer_chart_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction AND state = OLD.state AND files = 0;

    INSERT INTO transfer_chart_hours
    SELECT NEW.ended_at / 3600000000000 * 3600, NEW.direction, NEW.state, NEW.size, 1
    WHERE NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND NEW.ended_at IS NOT NULL
    ON CONFLICT (hour, direction, state) DO UPDATE SET bytes = bytes + excluded.bytes, files = files + 1;
END;
