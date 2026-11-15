-- The crew's Paid button after Complete job (owner, 2026-11-14): when the
-- customer paid the installer or template maker, and who tapped it.
ALTER TABLE events
    ADD COLUMN paid_at DATETIME NULL AFTER completed_at,
    ADD COLUMN paid_by_user_id INT NULL AFTER paid_at;
