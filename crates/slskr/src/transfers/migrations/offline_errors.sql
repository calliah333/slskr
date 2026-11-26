-- "User <name> appears to be offline" is stored as "User was offline": the transfer is
-- listed under its user already, and one message per user split the error reports.
INSERT OR IGNORE INTO transfer_errors (exception)
SELECT 'User was offline'
WHERE EXISTS (SELECT 1 FROM transfer_errors WHERE exception LIKE 'User % appears to be offline');

-- Moving the transfers runs the rollup update trigger, which moves their error counts.
UPDATE transfers
SET error_id = (SELECT error_id FROM transfer_errors WHERE exception = 'User was offline')
WHERE error_id IN (
    SELECT error_id FROM transfer_errors WHERE exception LIKE 'User % appears to be offline');

-- Purged transfers leave their counts behind in the rollups; move those too.
INSERT INTO transfer_error_hours (hour, direction, error, username, files)
SELECT hour, direction, 'User was offline', username, SUM(files)
FROM transfer_error_hours
WHERE error LIKE 'User % appears to be offline'
GROUP BY hour, direction, username
ON CONFLICT (direction, hour, error, username) DO UPDATE SET files = files + excluded.files;
DELETE FROM transfer_error_hours WHERE error LIKE 'User % appears to be offline';

DELETE FROM transfer_errors WHERE exception LIKE 'User % appears to be offline';
