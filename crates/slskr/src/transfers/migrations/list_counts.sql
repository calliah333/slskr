-- How many transfers each list holds per state (and their size), kept by triggers so the
-- list summary does not count every listed row: a busy uploader lists hundreds of
-- thousands. `removed` is 0 for listed rows and 1 for every removed one.
CREATE TABLE transfer_list_counts (
    direction TEXT NOT NULL,
    state INTEGER NOT NULL,
    removed INTEGER NOT NULL,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (direction, state, removed)
) WITHOUT ROWID;

INSERT INTO transfer_list_counts
SELECT direction, state, removed != 0, COUNT(*), SUM(size) FROM transfers GROUP BY 1, 2, 3;

CREATE TRIGGER transfer_list_counts_insert AFTER INSERT ON transfers
BEGIN
    INSERT INTO transfer_list_counts VALUES (NEW.direction, NEW.state, NEW.removed != 0, 1, NEW.size)
    ON CONFLICT (direction, state, removed) DO UPDATE
    SET files = files + 1, bytes = bytes + excluded.bytes;
END;

CREATE TRIGGER transfer_list_counts_delete AFTER DELETE ON transfers
BEGIN
    UPDATE transfer_list_counts SET files = files - 1, bytes = bytes - OLD.size
    WHERE direction = OLD.direction AND state = OLD.state AND removed = (OLD.removed != 0);
END;

CREATE TRIGGER transfer_list_counts_update AFTER UPDATE OF direction, state, removed, size ON transfers
WHEN OLD.direction != NEW.direction OR OLD.state != NEW.state
    OR (OLD.removed != 0) != (NEW.removed != 0) OR OLD.size != NEW.size
BEGIN
    UPDATE transfer_list_counts SET files = files - 1, bytes = bytes - OLD.size
    WHERE direction = OLD.direction AND state = OLD.state AND removed = (OLD.removed != 0);
    INSERT INTO transfer_list_counts VALUES (NEW.direction, NEW.state, NEW.removed != 0, 1, NEW.size)
    ON CONFLICT (direction, state, removed) DO UPDATE
    SET files = files + 1, bytes = bytes + excluded.bytes;
END;
