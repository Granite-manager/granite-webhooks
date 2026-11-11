-- Walk-in check-in tablets: the walk-in form runs on a paired tablet, not on
-- a user account. An admin makes a 6-digit pairing code on Company; typed on
-- the tablet it becomes the tablet's key (only its SHA-256 is kept here).
CREATE TABLE check_in_devices (
  id INT NOT NULL AUTO_INCREMENT,
  company_id INT NOT NULL,
  name VARCHAR(100) NOT NULL,
  -- While the tablet waits to be paired: the code to type, until it expires.
  pairing_code CHAR(6) NULL,
  pairing_expires_at DATETIME NULL,
  -- Once paired: SHA-256 (hex) of the tablet's key.
  token_hash CHAR(64) NULL,
  paired_at DATETIME NULL,
  last_seen_at DATETIME NULL,
  created_by INT NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  revoked_at DATETIME NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_check_in_devices_token (token_hash),
  UNIQUE KEY uq_check_in_devices_code (pairing_code),
  KEY idx_check_in_devices_company (company_id, revoked_at),
  CONSTRAINT fk_check_in_devices_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_check_in_devices_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
);
