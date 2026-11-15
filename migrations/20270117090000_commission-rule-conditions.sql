-- Commission rules can depend on the quote's discount (over / at most a
-- percent), on who made the quote (the rep or someone else) and, for line
-- rules, on the customer paying for the line. The settings remember when the
-- sync last ran, so the page can run it by itself when it is stale.
ALTER TABLE commission_rules
  ADD COLUMN discount_min DECIMAL(6,2) NULL,
  ADD COLUMN discount_max DECIMAL(6,2) NULL,
  ADD COLUMN made_by VARCHAR(10) NOT NULL DEFAULT 'any',
  ADD COLUMN paid_only TINYINT(1) NOT NULL DEFAULT 0;

ALTER TABLE commission_settings
  ADD COLUMN last_synced_at DATETIME NULL;
