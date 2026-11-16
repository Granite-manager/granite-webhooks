-- Pay for templates and installs (Admin > Commissions): rules (dollars per
-- sq ft for a position or one person, and named extras), a row per finished
-- job and person, and the extras an admin adds to a job by hand.
CREATE TABLE job_pay_rules (
  id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(120) NOT NULL,
  work VARCHAR(10) NOT NULL,
  kind VARCHAR(10) NOT NULL,
  amount DECIMAL(10,2) NOT NULL DEFAULT 0,
  user_id INT NULL,
  active TINYINT(1) NOT NULL DEFAULT 1,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_job_pay_rules_company (company_id, deleted_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE job_pay (
  id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  event_id INT NOT NULL,
  user_id INT NOT NULL,
  work VARCHAR(10) NOT NULL,
  sq_ft DECIMAL(10,2) NULL,
  sq_ft_override DECIMAL(10,2) NULL,
  rate DECIMAL(10,2) NOT NULL DEFAULT 0,
  amount_override DECIMAL(12,2) NULL,
  done_date DATE NOT NULL,
  payroll_date DATE NULL,
  paid_out_at DATETIME NULL,
  synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_job_pay_event_user (event_id, user_id),
  KEY idx_job_pay_company (company_id, user_id, payroll_date)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE job_pay_extras (
  id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  job_pay_id INT NOT NULL,
  label VARCHAR(120) NOT NULL,
  amount DECIMAL(12,2) NOT NULL,
  KEY idx_job_pay_extras_job (job_pay_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- When job pay goes into the salary (the payroll of the week after the job,
-- or the first one on or after it) and the day jobs are paid from.
ALTER TABLE commission_settings
  ADD COLUMN job_pay_same_week TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN job_pay_from DATE NULL;
