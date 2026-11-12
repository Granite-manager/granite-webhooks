-- What an edit changed, for the event's Activity (website and phone app):
-- the value before and after (a readiness, a date, a label, a title, a
-- calendar, an address) and who made it by id, for their picture. Older
-- lines and lines that are not a change keep them empty.
ALTER TABLE event_history
    ADD COLUMN from_value VARCHAR(500) NULL DEFAULT NULL AFTER body,
    ADD COLUMN to_value VARCHAR(500) NULL DEFAULT NULL AFTER from_value,
    ADD COLUMN created_by_id INT NULL DEFAULT NULL AFTER created_by;
