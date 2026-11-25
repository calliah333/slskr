-- Compacts the transfers table, which a long-running instance grows to a million rows.
--
-- - Paths and errors are stored once, in `transfer_directories`, `transfer_files` and
--   `transfer_errors`, and referenced by id. A share has tens of thousands of files and a
--   few thousand folders, and failures repeat a few hundred texts once the peer addresses
--   in them are masked (`mask_addresses`, registered by `Database::open`).
-- - Ids are 16-byte UUIDs instead of 36-character text; an id that is not a canonical
--   UUID stays text.
-- - `state_description` (never read) and `start_offset` (never read) are gone.
-- - Negative speeds (from imported history) become 0.
-- - The rollups are rebuilt from the converted rows (the error keys changed), then rows
--   that were removed from the list more than 30 days ago are deleted: the rollups keep
--   their totals, and nothing else reads them.
--
-- `transfer_details` reads a transfer with its texts. Rows of interned tables are never
-- deleted after this: they are bounded by the files shared and requested.

CREATE TABLE transfer_directories (
    directory_id INTEGER PRIMARY KEY,
    -- Up to and including the last separator; '' for none.
    path TEXT NOT NULL UNIQUE
);
CREATE TABLE transfer_files (
    file_id INTEGER PRIMARY KEY,
    directory_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (directory_id, name)
);
CREATE TABLE transfer_errors (
    error_id INTEGER PRIMARY KEY,
    exception TEXT NOT NULL UNIQUE
);

INSERT INTO transfer_directories (path)
SELECT DISTINCT rtrim(filename, replace(replace(filename, '/', ''), '\', '')) FROM transfers;

INSERT INTO transfer_files (directory_id, name)
SELECT DISTINCT d.directory_id, substr(t.filename, length(d.path) + 1)
FROM transfers t
JOIN transfer_directories d ON d.path = rtrim(t.filename, replace(replace(t.filename, '/', ''), '\', ''));

INSERT INTO transfer_errors (exception)
SELECT DISTINCT mask_addresses(exception) FROM transfers WHERE exception IS NOT NULL;

CREATE TABLE transfers_compact (
    -- 16 bytes for a canonical (lowercase, hyphenated) UUID, else the text.
    id BLOB NOT NULL,
    batch_id TEXT,
    username TEXT NOT NULL,
    direction TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    size INTEGER NOT NULL,
    state INTEGER NOT NULL,
    requested_at INTEGER NOT NULL,
    enqueued_at INTEGER,
    started_at INTEGER,
    ended_at INTEGER,
    bytes_transferred INTEGER NOT NULL DEFAULT 0,
    average_speed REAL NOT NULL DEFAULT 0,
    error_id INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    removed INTEGER NOT NULL DEFAULT 0,
    place_in_queue INTEGER,
    root TEXT
);

INSERT INTO transfers_compact (rowid, id, batch_id, username, direction, file_id, size, state,
    requested_at, enqueued_at, started_at, ended_at, bytes_transferred, average_speed, error_id,
    attempts, next_attempt_at, removed, place_in_queue, root)
SELECT t.rowid,
    CASE WHEN length(t.id) = 36 AND t.id = lower(t.id) AND substr(t.id, 9, 1) = '-'
            AND substr(t.id, 14, 1) = '-' AND substr(t.id, 19, 1) = '-' AND substr(t.id, 24, 1) = '-'
            AND length(unhex(replace(t.id, '-', ''))) = 16
        THEN unhex(replace(t.id, '-', '')) ELSE t.id END,
    t.batch_id, t.username, t.direction, f.file_id, t.size, t.state, t.requested_at,
    t.enqueued_at, t.started_at, t.ended_at, t.bytes_transferred, max(t.average_speed, 0),
    e.error_id, t.attempts, t.next_attempt_at, t.removed, t.place_in_queue, t.root
FROM transfers t
JOIN transfer_directories d ON d.path = rtrim(t.filename, replace(replace(t.filename, '/', ''), '\', ''))
JOIN transfer_files f ON f.directory_id = d.directory_id AND f.name = substr(t.filename, length(d.path) + 1)
LEFT JOIN transfer_errors e ON e.exception = mask_addresses(t.exception);

DROP TABLE transfers;
ALTER TABLE transfers_compact RENAME TO transfers;

-- After the bulk insert, so the indexes are built packed.
CREATE UNIQUE INDEX ix_transfers_id ON transfers(id);
CREATE INDEX ix_transfers_username ON transfers(username);
CREATE INDEX ix_transfers_batch_id ON transfers(batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX ix_transfers_recent ON transfers(direction, ended_at DESC) WHERE (state & 16) != 0;
CREATE INDEX ix_transfers_active ON transfers(direction) WHERE (state & 16) = 0;
CREATE INDEX ix_transfers_report_state_end ON transfers(state, ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX ix_transfers_report_errors_end ON transfers(direction, ended_at)
    WHERE (state & ~48) != 0 AND ended_at IS NOT NULL;
CREATE INDEX ix_transfers_listed_file ON transfers(direction, username, file_id)
    WHERE removed = 0 AND (state & 16) != 0;
-- The purge: rows removed from the list, by age.
CREATE INDEX ix_transfers_removed ON transfers(ended_at) WHERE removed;

CREATE VIEW transfer_details AS
SELECT t.rowid AS seq, t.id, t.batch_id, t.username, t.direction, d.path || f.name AS filename,
    rtrim(d.path, '/\') AS directory, t.size, t.state, t.requested_at, t.enqueued_at,
    t.started_at, t.ended_at, t.bytes_transferred, t.average_speed, e.exception, t.attempts,
    t.next_attempt_at, t.removed, t.place_in_queue, t.root
FROM transfers t
JOIN transfer_files f ON f.file_id = t.file_id
JOIN transfer_directories d ON d.directory_id = f.directory_id
LEFT JOIN transfer_errors e ON e.error_id = t.error_id;

DELETE FROM transfer_chart_hours;
DELETE FROM transfer_user_hours;
DELETE FROM transfer_error_hours;
DELETE FROM transfer_directory_hours;

INSERT INTO transfer_chart_hours
SELECT ended_at / 3600000000000 * 3600, direction, state, SUM(size), COUNT(*)
FROM transfers
WHERE state IN (16, 48, 80, 144, 272, 528, 1040) AND ended_at IS NOT NULL
GROUP BY 1, 2, 3;

INSERT INTO transfer_user_hours
SELECT ended_at / 3600000000000 * 3600, direction, state, username, COUNT(*), SUM(size),
    TOTAL(average_speed), TOTAL(started_at - enqueued_at), COUNT(started_at - enqueued_at),
    TOTAL(ended_at - started_at), COUNT(ended_at - started_at)
FROM transfers
WHERE state IN (16, 48, 80, 144, 272, 528, 1040) AND ended_at IS NOT NULL
GROUP BY 1, 2, 3, 4;

INSERT INTO transfer_error_hours
SELECT ended_at / 3600000000000 * 3600, direction,
    coalesce(CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END, ''),
    username, COUNT(*)
FROM transfer_details
WHERE (state & ~48) != 0 AND ended_at IS NOT NULL
GROUP BY 1, 2, 3, 4;

INSERT INTO transfer_directory_hours
SELECT ended_at / 3600000000000 * 3600, directory, username, COUNT(*)
FROM transfer_details
WHERE direction = 'Upload' AND state = 48 AND ended_at IS NOT NULL
GROUP BY 1, 2, 3;

-- Before the triggers exist, so the rollups keep these rows.
DELETE FROM transfers WHERE removed AND ended_at < (unixepoch() - 30 * 86400) * 1000000000;
DELETE FROM transfer_files WHERE file_id NOT IN (SELECT file_id FROM transfers);
DELETE FROM transfer_directories WHERE directory_id NOT IN (SELECT directory_id FROM transfer_files);
DELETE FROM transfer_errors WHERE error_id NOT IN (SELECT error_id FROM transfers WHERE error_id IS NOT NULL);

-- The rollup triggers, as before, but reading the interned texts. Deleting a row takes it
-- out of the rollups, except for rows the purge marks `removed = 2`: their history stays.

CREATE TRIGGER transfer_chart_insert AFTER INSERT ON transfers
WHEN NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND NEW.ended_at IS NOT NULL
BEGIN
    INSERT INTO transfer_chart_hours VALUES (NEW.ended_at / 3600000000000 * 3600, NEW.direction, NEW.state, NEW.size, 1)
    ON CONFLICT (hour, direction, state) DO UPDATE SET bytes = bytes + excluded.bytes, files = files + 1;
END;

CREATE TRIGGER transfer_chart_delete AFTER DELETE ON transfers
WHEN OLD.state IN (16, 48, 80, 144, 272, 528, 1040) AND OLD.ended_at IS NOT NULL AND OLD.removed < 2
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

CREATE TRIGGER transfer_report_hours_insert AFTER INSERT ON transfers
WHEN NEW.ended_at IS NOT NULL
BEGIN
    INSERT INTO transfer_user_hours
    SELECT NEW.ended_at / 3600000000000 * 3600, NEW.direction, NEW.state, NEW.username, 1, NEW.size,
        NEW.average_speed, coalesce(NEW.started_at - NEW.enqueued_at, 0), NEW.started_at - NEW.enqueued_at IS NOT NULL,
        coalesce(NEW.ended_at - NEW.started_at, 0), NEW.ended_at - NEW.started_at IS NOT NULL
    WHERE NEW.state IN (16, 48, 80, 144, 272, 528, 1040)
    ON CONFLICT (hour, direction, state, username) DO UPDATE SET files = files + 1,
        bytes = bytes + excluded.bytes, speed = speed + excluded.speed,
        wait = wait + excluded.wait, waits = waits + excluded.waits,
        duration = duration + excluded.duration, durations = durations + excluded.durations;

    INSERT INTO transfer_error_hours
    SELECT NEW.ended_at / 3600000000000 * 3600, NEW.direction,
        coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = NEW.error_id), ''),
        NEW.username, 1
    WHERE (NEW.state & ~48) != 0
    ON CONFLICT (direction, hour, error, username) DO UPDATE SET files = files + 1;

    INSERT INTO transfer_directory_hours
    SELECT NEW.ended_at / 3600000000000 * 3600,
        (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = NEW.file_id),
        NEW.username, 1
    WHERE NEW.direction = 'Upload' AND NEW.state = 48
    ON CONFLICT (hour, directory, username) DO UPDATE SET files = files + 1;
END;

CREATE TRIGGER transfer_report_hours_delete AFTER DELETE ON transfers
WHEN OLD.ended_at IS NOT NULL AND OLD.removed < 2
BEGIN
    UPDATE transfer_user_hours SET files = files - 1, bytes = bytes - OLD.size,
        speed = speed - OLD.average_speed,
        wait = wait - coalesce(OLD.started_at - OLD.enqueued_at, 0),
        waits = waits - (OLD.started_at - OLD.enqueued_at IS NOT NULL),
        duration = duration - coalesce(OLD.ended_at - OLD.started_at, 0),
        durations = durations - (OLD.ended_at - OLD.started_at IS NOT NULL)
    WHERE OLD.state IN (16, 48, 80, 144, 272, 528, 1040) AND hour = OLD.ended_at / 3600000000000 * 3600
        AND direction = OLD.direction AND state = OLD.state AND username = OLD.username;
    DELETE FROM transfer_user_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND state = OLD.state AND username = OLD.username AND files = 0;

    UPDATE transfer_error_hours SET files = files - 1
    WHERE (OLD.state & ~48) != 0 AND hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username AND files = 0;

    UPDATE transfer_directory_hours SET files = files - 1
    WHERE OLD.direction = 'Upload' AND OLD.state = 48 AND hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = OLD.file_id)
        AND username = OLD.username;
    DELETE FROM transfer_directory_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = OLD.file_id)
        AND username = OLD.username AND files = 0;
END;

-- Takes the old row out and puts the new one in. Saving an unfinished transfer touches
-- these columns too, but it has no end time until it completes.
CREATE TRIGGER transfer_report_hours_update
AFTER UPDATE OF ended_at, direction, state, username, size, average_speed, started_at, enqueued_at,
    error_id, file_id ON transfers
WHEN OLD.ended_at IS NOT NULL OR NEW.ended_at IS NOT NULL
BEGIN
    UPDATE transfer_user_hours SET files = files - 1, bytes = bytes - OLD.size,
        speed = speed - OLD.average_speed,
        wait = wait - coalesce(OLD.started_at - OLD.enqueued_at, 0),
        waits = waits - (OLD.started_at - OLD.enqueued_at IS NOT NULL),
        duration = duration - coalesce(OLD.ended_at - OLD.started_at, 0),
        durations = durations - (OLD.ended_at - OLD.started_at IS NOT NULL)
    WHERE OLD.state IN (16, 48, 80, 144, 272, 528, 1040) AND hour = OLD.ended_at / 3600000000000 * 3600
        AND direction = OLD.direction AND state = OLD.state AND username = OLD.username;
    DELETE FROM transfer_user_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND state = OLD.state AND username = OLD.username AND files = 0;

    UPDATE transfer_error_hours SET files = files - 1
    WHERE (OLD.state & ~48) != 0 AND hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username AND files = 0;

    UPDATE transfer_directory_hours SET files = files - 1
    WHERE OLD.direction = 'Upload' AND OLD.state = 48 AND hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = OLD.file_id)
        AND username = OLD.username;
    DELETE FROM transfer_directory_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = OLD.file_id)
        AND username = OLD.username AND files = 0;

    INSERT INTO transfer_user_hours
    SELECT NEW.ended_at / 3600000000000 * 3600, NEW.direction, NEW.state, NEW.username, 1, NEW.size,
        NEW.average_speed, coalesce(NEW.started_at - NEW.enqueued_at, 0), NEW.started_at - NEW.enqueued_at IS NOT NULL,
        coalesce(NEW.ended_at - NEW.started_at, 0), NEW.ended_at - NEW.started_at IS NOT NULL
    WHERE NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND NEW.ended_at IS NOT NULL
    ON CONFLICT (hour, direction, state, username) DO UPDATE SET files = files + 1,
        bytes = bytes + excluded.bytes, speed = speed + excluded.speed,
        wait = wait + excluded.wait, waits = waits + excluded.waits,
        duration = duration + excluded.duration, durations = durations + excluded.durations;

    INSERT INTO transfer_error_hours
    SELECT NEW.ended_at / 3600000000000 * 3600, NEW.direction,
        coalesce((SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END
            FROM transfer_errors WHERE error_id = NEW.error_id), ''),
        NEW.username, 1
    WHERE (NEW.state & ~48) != 0 AND NEW.ended_at IS NOT NULL
    ON CONFLICT (direction, hour, error, username) DO UPDATE SET files = files + 1;

    INSERT INTO transfer_directory_hours
    SELECT NEW.ended_at / 3600000000000 * 3600,
        (SELECT rtrim(path, '/\') FROM transfer_files JOIN transfer_directories USING (directory_id) WHERE file_id = NEW.file_id),
        NEW.username, 1
    WHERE NEW.direction = 'Upload' AND NEW.state = 48 AND NEW.ended_at IS NOT NULL
    ON CONFLICT (hour, directory, username) DO UPDATE SET files = files + 1;
END;
