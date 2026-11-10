-- Invoice drafts learn from what an invoice became in QuickBooks, not only
-- from what the rep changed before creating it. Additive only.

-- The QuickBooks line a draft line became, so a later change or removal of
-- that line in QuickBooks is read back as the line's outcome.
ALTER TABLE qbo_invoice_draft_outcomes
  ADD COLUMN qbo_line_id VARCHAR(32) NULL AFTER saved_value,
  ADD KEY idx_qbo_invoice_draft_outcomes_invoice (company_id, qbo_invoice_id);

-- What the AI learned from the company's QuickBooks invoices about how it
-- writes invoice lines, as checked rules every new draft follows (the same
-- rules as qbo_invoice_instructions, which win over these).
CREATE TABLE qbo_invoice_style (
  company_id INT NOT NULL PRIMARY KEY,
  -- {"rules": [...], "notes": [...], "invoices_read": 37}
  rules_json JSON NOT NULL,
  learned_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
