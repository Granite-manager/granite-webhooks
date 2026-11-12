-- What moves a project on from a stage by itself: the built-in line at the end of
-- Awaiting Template and Templated in Admin > Projects takes Auto like a step does
-- (same triggers). Additive only.
CREATE TABLE project_stage_auto (
    id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    stage_code VARCHAR(20) NOT NULL,
    auto_trigger VARCHAR(40) NOT NULL,
    auto_ref_id INT NULL,
    auto_ref_ids VARCHAR(255) NULL,
    auto_text VARCHAR(255) NULL,
    auto_days INT NULL,
    auto_since DATETIME(3) NOT NULL,
    UNIQUE KEY uq_project_stage_auto (company_id, stage_code)
);
