-- The overview chart needs totals only. Keep one row per completed hour, direction and state.
CREATE TABLE transfer_chart_hours (
    hour INTEGER NOT NULL,
    direction TEXT NOT NULL,
    state INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    files INTEGER NOT NULL,
    PRIMARY KEY (hour, direction, state)
) WITHOUT ROWID;

INSERT INTO transfer_chart_hours
SELECT unixepoch(substr(ended_at, 1, 19)) / 3600 * 3600, direction, state, SUM(size), COUNT(*)
FROM transfers
WHERE state IN (16, 48, 80, 144, 272, 528, 1040) AND unixepoch(substr(ended_at, 1, 19)) IS NOT NULL
GROUP BY 1, 2, 3;

CREATE TRIGGER transfer_chart_insert AFTER INSERT ON transfers
WHEN NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND unixepoch(substr(NEW.ended_at, 1, 19)) IS NOT NULL
BEGIN
    INSERT INTO transfer_chart_hours VALUES (unixepoch(substr(NEW.ended_at, 1, 19)) / 3600 * 3600, NEW.direction, NEW.state, NEW.size, 1)
    ON CONFLICT (hour, direction, state) DO UPDATE SET bytes = bytes + excluded.bytes, files = files + 1;
END;

CREATE TRIGGER transfer_chart_delete AFTER DELETE ON transfers
WHEN OLD.state IN (16, 48, 80, 144, 272, 528, 1040) AND unixepoch(substr(OLD.ended_at, 1, 19)) IS NOT NULL
BEGIN
    UPDATE transfer_chart_hours SET bytes = bytes - OLD.size, files = files - 1
    WHERE hour = unixepoch(substr(OLD.ended_at, 1, 19)) / 3600 * 3600 AND direction = OLD.direction AND state = OLD.state;
    DELETE FROM transfer_chart_hours
    WHERE hour = unixepoch(substr(OLD.ended_at, 1, 19)) / 3600 * 3600 AND direction = OLD.direction AND state = OLD.state AND files = 0;
END;

CREATE TRIGGER transfer_chart_update AFTER UPDATE OF ended_at, direction, state, size ON transfers
WHEN OLD.ended_at IS NOT NEW.ended_at OR OLD.direction != NEW.direction OR OLD.state != NEW.state OR OLD.size != NEW.size
BEGIN
    UPDATE transfer_chart_hours SET bytes = bytes - OLD.size, files = files - 1
    WHERE OLD.state IN (16, 48, 80, 144, 272, 528, 1040)
      AND hour = unixepoch(substr(OLD.ended_at, 1, 19)) / 3600 * 3600 AND direction = OLD.direction AND state = OLD.state;
    DELETE FROM transfer_chart_hours
    WHERE hour = unixepoch(substr(OLD.ended_at, 1, 19)) / 3600 * 3600 AND direction = OLD.direction AND state = OLD.state AND files = 0;

    INSERT INTO transfer_chart_hours
    SELECT unixepoch(substr(NEW.ended_at, 1, 19)) / 3600 * 3600, NEW.direction, NEW.state, NEW.size, 1
    WHERE NEW.state IN (16, 48, 80, 144, 272, 528, 1040) AND unixepoch(substr(NEW.ended_at, 1, 19)) IS NOT NULL
    ON CONFLICT (hour, direction, state) DO UPDATE SET bytes = bytes + excluded.bytes, files = files + 1;
END;
