-- Admin > Stones > Edit image > Slab background: a stone can be shown
-- without the company's slab picture background. On (1) by default.
ALTER TABLE stones
  ADD COLUMN slab_background_on TINYINT(1) NOT NULL DEFAULT 1;
