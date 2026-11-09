-- Each user's own maps app for addresses and directions ('google' or
-- 'apple'; NULL: the device's own), and when the Schedule asked them once
-- for their 12/24-hour times and maps app (NULL: not asked yet).
ALTER TABLE users
    ADD COLUMN maps_app VARCHAR(10) NULL,
    ADD COLUMN schedule_prefs_asked_at DATETIME NULL;
