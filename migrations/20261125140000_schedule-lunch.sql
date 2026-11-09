-- Lunch in a job kind's work day (Admin > Schedule > Job time). The Auto-plan
-- keeps it free. Start is minutes after midnight in the shop's time zone;
-- 0 minutes means no lunch.
ALTER TABLE schedule_time_rules
    ADD COLUMN lunch_start_minute SMALLINT UNSIGNED NOT NULL DEFAULT 720 AFTER day_end_minute,
    ADD COLUMN lunch_minutes SMALLINT UNSIGNED NOT NULL DEFAULT 0 AFTER lunch_start_minute;
