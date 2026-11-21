-- Admin > Stones > Add / Edit > Close-up: a second picture of the stone,
-- drawn in a red circle at the top right of its picture (over the slab
-- picture background too). The stone keeps the picture as uploaded in
-- `original_url`.
ALTER TABLE stones
  ADD COLUMN closeup_url VARCHAR(1024) NULL;
