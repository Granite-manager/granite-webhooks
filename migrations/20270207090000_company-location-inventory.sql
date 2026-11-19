-- Locations of one business share a location group: GMQ TOPS Montgomery,
-- Birmingham and Mobile. Admin > Stones > Settings > "Show all locations'
-- inventory" lets the company's employees look at the other locations'
-- stones on the Stones page (look only). Vadim asked for it at GMQ TOPS, so
-- it starts on there.
ALTER TABLE company
  ADD COLUMN location_group VARCHAR(64) NULL,
  ADD COLUMN show_all_locations_inventory TINYINT(1) NOT NULL DEFAULT 0;

UPDATE company
   SET location_group = 'GMQ TOPS', show_all_locations_inventory = 1
 WHERE id IN (4, 12, 13) AND name LIKE 'GMQ TOPS%';
