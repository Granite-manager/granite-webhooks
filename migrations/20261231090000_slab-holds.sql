-- Slab holds: a rep puts an available slab on hold for a few days (3, 5, 7,
-- 12 or 15) from the stone's slabs dialog. The hold ends by itself at
-- `hold_until`; a row with a past `hold_until` is not on hold. `hold_by` is
-- the user who placed it, `hold_at` when.
ALTER TABLE slab_inventory
  ADD COLUMN hold_until DATETIME NULL DEFAULT NULL,
  ADD COLUMN hold_by INT NULL DEFAULT NULL,
  ADD COLUMN hold_at DATETIME NULL DEFAULT NULL;
