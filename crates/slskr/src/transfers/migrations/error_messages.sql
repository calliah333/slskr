-- Errors are stored as the message alone. They used to start with a .NET exception name
-- ("TimeoutException: …", from slskd), which every report and rollup cut off again; the
-- transfer state already says how it failed. The rollups were keyed by the text after
-- the name, which is now the stored text, so they stay as they are.
--
-- Two names with the same message ("TimeoutException: x" and "System.TimeoutException:
-- x" from imported history) become one error; transfers move to the lowest id.

-- The rollup triggers read the old form; they come back below. Moving transfers between
-- errors with the same message does not change the rollups.
DROP TRIGGER transfer_report_hours_insert;
DROP TRIGGER transfer_report_hours_delete;
DROP TRIGGER transfer_report_hours_update;

CREATE TEMP TABLE error_texts AS
SELECT error_id,
    CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END AS text
FROM transfer_errors;
CREATE TEMP TABLE error_kept AS
SELECT text, MIN(error_id) AS error_id FROM error_texts GROUP BY text;

UPDATE transfers SET error_id = (
    SELECT k.error_id FROM error_texts t JOIN error_kept k USING (text)
    WHERE t.error_id = transfers.error_id)
WHERE error_id IN (
    SELECT t.error_id FROM error_texts t JOIN error_kept k USING (text)
    WHERE t.error_id != k.error_id);
DELETE FROM transfer_errors WHERE error_id NOT IN (SELECT error_id FROM error_kept);
-- In two steps: a new text can equal another row's old one ("A: B: c" and "B: c").
UPDATE transfer_errors SET exception = char(1) || error_id;
UPDATE transfer_errors SET exception = (
    SELECT text FROM error_texts WHERE error_texts.error_id = transfer_errors.error_id);

DROP TABLE error_texts;
DROP TABLE error_kept;

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
        coalesce((SELECT exception
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
        AND error = coalesce((SELECT exception
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT exception
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
        AND error = coalesce((SELECT exception
            FROM transfer_errors WHERE error_id = OLD.error_id), '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce((SELECT exception
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
        coalesce((SELECT exception
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
