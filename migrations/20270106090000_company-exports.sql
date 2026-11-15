-- Daily export of a company's data (owner, 2026-11-14): every morning the
-- `company-exports` job zips the company's rows (CSV, one file per table) and
-- its pictures and documents, stores the zip in the private bucket and emails
-- the recipients a link that works for 7 days.

-- Which companies get the export, who receives it and when. Recipients are
-- kept here, not in code. One row per company; no row: no export.
CREATE TABLE company_exports (
  company_id INT NOT NULL PRIMARY KEY,
  -- Comma-separated email addresses that get the daily link.
  recipients TEXT NOT NULL,
  -- Comma-separated email addresses told when a day's export fails.
  alert_recipients TEXT NULL,
  -- Local hour (0-23) in `time_zone` from which the day's export goes out.
  send_hour TINYINT NOT NULL DEFAULT 6,
  -- IANA time zone of `send_hour` and of the export's date.
  time_zone VARCHAR(64) NOT NULL DEFAULT 'America/New_York',
  -- Days a zip (and its link) is kept.
  keep_days SMALLINT NOT NULL DEFAULT 7,
  is_enabled TINYINT(1) NOT NULL DEFAULT 1,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- One export per company and local day: the unique key makes the
-- every-minute job send it once.
CREATE TABLE company_export_runs (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  export_date DATE NOT NULL,
  status ENUM('running', 'sent', 'failed') NOT NULL,
  attempts TINYINT NOT NULL DEFAULT 1,
  started_at DATETIME NOT NULL,
  finished_at DATETIME NULL,
  -- The zip in the private bucket; NULL until uploaded.
  object_key VARCHAR(512) NULL,
  zip_bytes BIGINT NULL,
  table_count INT NULL,
  row_count INT NULL,
  -- Files put in this zip (new since the last export, or all on a full day).
  file_count INT NULL,
  full_files TINYINT(1) NULL,
  link_expires_at DATETIME NULL,
  error TEXT NULL,
  -- When the failure alert went out.
  alerted_at DATETIME NULL,
  -- When the zip was deleted from the bucket.
  deleted_at DATETIME NULL,
  UNIQUE KEY uq_company_export_runs_day (company_id, export_date),
  INDEX idx_company_export_runs_cleanup (deleted_at, started_at)
);

-- Files already sent in an export, so the next day's zip only adds new ones.
CREATE TABLE company_export_files (
  company_id INT NOT NULL,
  -- SHA-256 (hex) of "<bucket>/<key>".
  file_hash CHAR(64) NOT NULL,
  bucket VARCHAR(255) NOT NULL,
  object_key TEXT NOT NULL,
  run_id BIGINT NOT NULL,
  exported_at DATETIME NOT NULL,
  PRIMARY KEY (company_id, file_hash)
);
