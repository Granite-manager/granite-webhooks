-- users.undo_send: a sent text or email waits 10 seconds with Undo before it goes out (on
-- by default). Off (My Account on the website, Settings > General in the phone app): it goes
-- out at once, on the website and in the app.
--
-- Guarded, so a database that has it already is left alone; INSTANT, with a short lock wait.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users'
      AND COLUMN_NAME = 'undo_send') = 0,
  'ALTER TABLE users ADD COLUMN undo_send TINYINT(1) NOT NULL DEFAULT 1, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
