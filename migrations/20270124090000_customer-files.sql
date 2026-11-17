-- Files kept on a customer (GMQ TOPS's Slabware invoices, signed contracts
-- and uploaded files), listed by GET /v1/customers/{id}/files.
CREATE TABLE customer_files (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  customer_id INT NOT NULL,
  name VARCHAR(255) NOT NULL,
  -- invoice, signed_contract, signed_invoice or upload
  kind VARCHAR(30) NOT NULL,
  -- what it belongs to, e.g. "Quote #11934"
  label VARCHAR(255) NULL,
  file_date DATE NULL,
  url VARCHAR(1024) NOT NULL,
  source VARCHAR(30) NOT NULL DEFAULT 'slabware',
  -- the file's id in its source, so an import runs again without duplicates
  source_id VARCHAR(255) NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_customer_files_customer (company_id, customer_id),
  UNIQUE KEY uq_customer_files_source (company_id, source, source_id)
);
