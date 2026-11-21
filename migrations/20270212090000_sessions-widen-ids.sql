-- Web session ids are stored hashed (`auth::session::session_key`): new rows
-- hold the SHA-256 (hex, 64 characters) of the cookie's session id, so a copy
-- of the database signs nobody in. Widen the two columns that hold one.
--
-- Apply before the API release that hashes them: it writes 64 characters.
-- The old API keeps working on the wider columns. Rows written before stay
-- bare until the pending `20270222090000_sessions-hash-existing-ids.sql`
-- hashes them after that release is deployed.
--
-- Both tables are small (prod 2026-11-20: 1572 sessions, 86 device sessions),
-- so the table copy a CHAR length change needs takes well under a second.
-- A short lock wait (3 s): behind a long transaction the migration fails and
-- is run again, instead of stalling the tables' other queries.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'sessions'
      AND COLUMN_NAME = 'id' AND CHARACTER_MAXIMUM_LENGTH < 64) = 1,
  'ALTER TABLE sessions MODIFY COLUMN id CHAR(64) NOT NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'device_sessions'
      AND COLUMN_NAME = 'web_session_id' AND CHARACTER_MAXIMUM_LENGTH < 64) = 1,
  'ALTER TABLE device_sessions MODIFY COLUMN web_session_id CHAR(64) NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
