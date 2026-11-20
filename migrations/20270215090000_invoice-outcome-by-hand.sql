-- Whether the rep made an invoice draft outcome by hand (1), or only left the
-- line as the editor showed it (0). NULL for invoices created before this was
-- kept. A line changed by hand says what the company wants; one only left as
-- shown says little, so the draft rules weigh it less.
ALTER TABLE qbo_invoice_draft_outcomes
  ADD COLUMN by_hand TINYINT(1) NULL AFTER qbo_line_id;
