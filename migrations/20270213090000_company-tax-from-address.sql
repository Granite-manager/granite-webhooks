-- The company's sales tax is worked out from its address (never typed): when
-- it was last looked up. NULL: never, or the lookup did not answer.
ALTER TABLE company ADD COLUMN state_taxes_checked_at DATETIME NULL AFTER state_taxes;
