-- The company's Notification number: automated texts (bookings, surveys, schedule notices) go out from it.
-- Scheduling Managers can read and answer its threads. Additive only.
ALTER TABLE telnyx_numbers
    ADD COLUMN notifications TINYINT(1) NOT NULL DEFAULT 0;
