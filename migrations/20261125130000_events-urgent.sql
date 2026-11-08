-- Urgent events: a red flag set in Mark as Sold or the event's edit form,
-- meaning the event must be scheduled fast.
ALTER TABLE events ADD COLUMN urgent TINYINT(1) NOT NULL DEFAULT 0;
