-- Area names and misc / text items taken out of the quote editor's suggestions
-- with the x next to them (GET /v1/quotes/suggestions leaves these out).
-- Quotes are not changed; a row only hides the suggestion for the company.
CREATE TABLE quote_suggestion_hidden (
  company_id INT NOT NULL,
  -- 'area' (an area name), 'misc' or 'text' (a custom item's description)
  kind VARCHAR(8) NOT NULL,
  -- The text lowercased with runs of spaces collapsed, as suggestions are matched
  text_key VARCHAR(200) NOT NULL,
  -- The text as it was shown when hidden
  text VARCHAR(200) NOT NULL,
  hidden_by INT NULL,
  hidden_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, kind, text_key),
  CONSTRAINT fk_quote_suggestion_hidden_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
