-- The shop's progress on a job: cut, then complete. Shop Managers and Shop
-- Workers set it; they and Installers see it on the schedule (the dot before
-- the event's name). Unlike the crew's status it stays when the event moves
-- to another day.
ALTER TABLE events
    ADD COLUMN shop_status VARCHAR(16) NULL AFTER completed_at,
    ADD COLUMN shop_status_at DATETIME NULL AFTER shop_status,
    ADD COLUMN shop_status_by INT NULL AFTER shop_status_at;
