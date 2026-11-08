-- Wrong Telegram link codes per user, counted in a one-hour window that starts
-- at the first wrong code. A new code does not reset the count; linking does.
ALTER TABLE users
  ADD COLUMN telegram_conf_failures TINYINT UNSIGNED NOT NULL DEFAULT 0 AFTER telegram_conf_expires_at,
  ADD COLUMN telegram_conf_failed_at TIMESTAMP NULL AFTER telegram_conf_failures;
