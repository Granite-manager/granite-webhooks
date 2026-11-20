-- Admin > Stones > Settings > Slab picture background: the company's own
-- picture (a warehouse, a rack, its logo) and the four-corner spot on it where
-- every new stone picture is placed. The spot is JSON, four [x, y] corners as
-- fractions of the picture, clockwise from top left. The version goes up on
-- every change, so stone pictures made with an older background are remade.
ALTER TABLE company
  ADD COLUMN slab_background_url VARCHAR(1024) NULL,
  ADD COLUMN slab_background_spot VARCHAR(255) NULL,
  ADD COLUMN slab_background_version INT NOT NULL DEFAULT 0;

-- A stone whose picture (`url`) is on the background keeps the picture as it
-- was uploaded and the background version it was made with.
ALTER TABLE stones
  ADD COLUMN original_url VARCHAR(1024) NULL,
  ADD COLUMN slab_background_version INT NULL;
