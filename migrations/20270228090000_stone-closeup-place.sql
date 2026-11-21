-- Admin > Stones > Edit image > Close-up: the admin drags the red circle and
-- changes its size. Its middle as a share of the picture's width and height,
-- its size across as a share of the shorter side; null: at the top right.
ALTER TABLE stones
  ADD COLUMN closeup_x DOUBLE NULL,
  ADD COLUMN closeup_y DOUBLE NULL,
  ADD COLUMN closeup_size DOUBLE NULL;
