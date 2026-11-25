-- Hourly rollups behind the transfer reports. A long report window reads whole hours here
-- (tens of thousands of rows rather than a million transfers) and only the transfers of
-- the partial hours at its edges; see `telemetry/reports.rs`. Triggers keep them current.
-- Times are summed as REAL: one bad timestamp would overflow an INTEGER sum.

-- Completed transfers per hour, direction, state and user: summary, leaderboards and
-- the user report.
CREATE TABLE transfer_user_hours (
    hour INTEGER NOT NULL,
    direction TEXT NOT NULL,
    state INTEGER NOT NULL,
    username TEXT NOT NULL,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    -- Sum of average_speed.
    speed REAL NOT NULL,
    -- Sum and count of the non-null started_at - enqueued_at, in nanoseconds.
    wait REAL NOT NULL,
    waits INTEGER NOT NULL,
    -- Sum and count of the non-null ended_at - started_at, in nanoseconds.
    duration REAL NOT NULL,
    durations INTEGER NOT NULL,
    PRIMARY KEY (hour, direction, state, username)
) WITHOUT ROWID;

-- Failed transfers per direction, hour, error ('' for none) and user: the error types. The
-- reports always ask for one direction, and most failures are uploads.
CREATE TABLE transfer_error_hours (
    hour INTEGER NOT NULL,
    direction TEXT NOT NULL,
    error TEXT NOT NULL,
    username TEXT NOT NULL,
    files INTEGER NOT NULL,
    PRIMARY KEY (direction, hour, error, username)
) WITHOUT ROWID;

-- Successful uploads per hour, folder and user: the popular folders.
CREATE TABLE transfer_directory_hours (
    hour INTEGER NOT NULL,
    directory TEXT NOT NULL,
    username TEXT NOT NULL,
    files INTEGER NOT NULL,
    PRIMARY KEY (hour, directory, username)
) WITHOUT ROWID;

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
FROM transfers
WHERE (state & ~48) != 0 AND ended_at IS NOT NULL
GROUP BY 1, 2, 3, 4;

INSERT INTO transfer_directory_hours
SELECT ended_at / 3600000000000 * 3600,
    rtrim(rtrim(filename, replace(replace(filename, '/', ''), '\', '')), '/\'), username, COUNT(*)
FROM transfers
WHERE direction = 'Upload' AND state = 48 AND ended_at IS NOT NULL
GROUP BY 1, 2, 3;

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
        coalesce(CASE WHEN instr(NEW.exception, ':') > 0 THEN substr(NEW.exception, instr(NEW.exception, ':') + 2) ELSE NEW.exception END, ''),
        NEW.username, 1
    WHERE (NEW.state & ~48) != 0
    ON CONFLICT (direction, hour, error, username) DO UPDATE SET files = files + 1;

    INSERT INTO transfer_directory_hours
    SELECT NEW.ended_at / 3600000000000 * 3600,
        rtrim(rtrim(NEW.filename, replace(replace(NEW.filename, '/', ''), '\', '')), '/\'), NEW.username, 1
    WHERE NEW.direction = 'Upload' AND NEW.state = 48
    ON CONFLICT (hour, directory, username) DO UPDATE SET files = files + 1;
END;

CREATE TRIGGER transfer_report_hours_delete AFTER DELETE ON transfers
WHEN OLD.ended_at IS NOT NULL
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
        AND error = coalesce(CASE WHEN instr(OLD.exception, ':') > 0 THEN substr(OLD.exception, instr(OLD.exception, ':') + 2) ELSE OLD.exception END, '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce(CASE WHEN instr(OLD.exception, ':') > 0 THEN substr(OLD.exception, instr(OLD.exception, ':') + 2) ELSE OLD.exception END, '')
        AND username = OLD.username AND files = 0;

    UPDATE transfer_directory_hours SET files = files - 1
    WHERE OLD.direction = 'Upload' AND OLD.state = 48 AND hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = rtrim(rtrim(OLD.filename, replace(replace(OLD.filename, '/', ''), '\', '')), '/\')
        AND username = OLD.username;
    DELETE FROM transfer_directory_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = rtrim(rtrim(OLD.filename, replace(replace(OLD.filename, '/', ''), '\', '')), '/\')
        AND username = OLD.username AND files = 0;
END;

-- Takes the old row out and puts the new one in. Saving an unfinished transfer touches
-- these columns too, but it has no end time until it completes.
CREATE TRIGGER transfer_report_hours_update
AFTER UPDATE OF ended_at, direction, state, username, size, average_speed, started_at, enqueued_at,
    exception, filename ON transfers
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
        AND error = coalesce(CASE WHEN instr(OLD.exception, ':') > 0 THEN substr(OLD.exception, instr(OLD.exception, ':') + 2) ELSE OLD.exception END, '')
        AND username = OLD.username;
    DELETE FROM transfer_error_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600 AND direction = OLD.direction
        AND error = coalesce(CASE WHEN instr(OLD.exception, ':') > 0 THEN substr(OLD.exception, instr(OLD.exception, ':') + 2) ELSE OLD.exception END, '')
        AND username = OLD.username AND files = 0;

    UPDATE transfer_directory_hours SET files = files - 1
    WHERE OLD.direction = 'Upload' AND OLD.state = 48 AND hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = rtrim(rtrim(OLD.filename, replace(replace(OLD.filename, '/', ''), '\', '')), '/\')
        AND username = OLD.username;
    DELETE FROM transfer_directory_hours
    WHERE hour = OLD.ended_at / 3600000000000 * 3600
        AND directory = rtrim(rtrim(OLD.filename, replace(replace(OLD.filename, '/', ''), '\', '')), '/\')
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
        coalesce(CASE WHEN instr(NEW.exception, ':') > 0 THEN substr(NEW.exception, instr(NEW.exception, ':') + 2) ELSE NEW.exception END, ''),
        NEW.username, 1
    WHERE (NEW.state & ~48) != 0 AND NEW.ended_at IS NOT NULL
    ON CONFLICT (direction, hour, error, username) DO UPDATE SET files = files + 1;

    INSERT INTO transfer_directory_hours
    SELECT NEW.ended_at / 3600000000000 * 3600,
        rtrim(rtrim(NEW.filename, replace(replace(NEW.filename, '/', ''), '\', '')), '/\'), NEW.username, 1
    WHERE NEW.direction = 'Upload' AND NEW.state = 48 AND NEW.ended_at IS NOT NULL
    ON CONFLICT (hour, directory, username) DO UPDATE SET files = files + 1;
END;
