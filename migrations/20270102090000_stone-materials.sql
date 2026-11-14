-- Admin › Stones › Settings "Stone view": a company sees its stones directly
-- (`stones`, the old view, default for every company) or materials first
-- (`materials`): Fantasy Brown, then each stone of it with its bundle and
-- block number, then the slabs.
ALTER TABLE company
  ADD COLUMN stone_view VARCHAR(16) NOT NULL DEFAULT 'stones';

-- A material groups the stones of one name. Stones are linked to the
-- material of their name by the API (on switching to materials, on listing
-- materials, and on saving a stone), so existing stones need no backfill here.
CREATE TABLE stone_materials (
  id INT PRIMARY KEY AUTO_INCREMENT,
  company_id INT NOT NULL,
  name VARCHAR(100) NOT NULL,
  type VARCHAR(50) NULL,
  url VARCHAR(255) NULL,
  created_date TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_stone_materials_company_name (company_id, name),
  CONSTRAINT fk_stone_materials_company FOREIGN KEY (company_id) REFERENCES company(id)
);

ALTER TABLE stones
  ADD COLUMN material_id INT NULL,
  ADD COLUMN block_number VARCHAR(255) NULL,
  ADD CONSTRAINT fk_stones_material FOREIGN KEY (material_id)
    REFERENCES stone_materials(id) ON DELETE SET NULL;
