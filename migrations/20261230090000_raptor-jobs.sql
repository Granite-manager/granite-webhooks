-- Raptor (LPI Cloud, templator.com): a company's Raptor login, set in the
-- super admin's Edit company form, and the Raptor jobs made from its
-- projects. `POST /v1/jobs/raptor-jobs` makes the job of each project created
-- after `raptor_since` (when the login was first saved), so older projects
-- are never sent.
ALTER TABLE company
    ADD COLUMN raptor_email VARCHAR(255) NULL,
    ADD COLUMN raptor_password TEXT NULL COMMENT 'AES-GCM with AES_KEY, as the CRM encrypts',
    ADD COLUMN raptor_since DATETIME NULL;

-- One row per project sent to Raptor: claimed as `working`, then `done` with
-- the Raptor jobs made ([{id, name, reference}]), or `failed` with the reason
-- (tried again until `attempts` reaches 3).
CREATE TABLE project_raptor_jobs (
    project_id INT NOT NULL PRIMARY KEY,
    company_id INT NOT NULL,
    status ENUM('working', 'done', 'failed') NOT NULL DEFAULT 'working',
    attempts INT NOT NULL DEFAULT 0,
    jobs JSON NULL,
    error VARCHAR(1000) NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    KEY idx_project_raptor_jobs_company (company_id, status),
    CONSTRAINT fk_project_raptor_jobs_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
