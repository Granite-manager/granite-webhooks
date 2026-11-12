-- The event's history keeps the full text of each text or email sent to
-- the customer (a flow step, a reminder, a confirmation, a follow-up), so
-- the phone app shows it when the line is tapped. Older lines have none.
ALTER TABLE event_history ADD COLUMN body TEXT NULL DEFAULT NULL AFTER message;
