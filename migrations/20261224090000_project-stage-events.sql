-- What moving a project on from a stage does to its schedule events: the built-in
-- line at the end of each stage in Admin > Projects takes Schedule like a step
-- does (readiness and shop status, e.g. Cut after fabrication). Additive only.
CREATE TABLE project_stage_events (
    id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    stage_code VARCHAR(20) NOT NULL,
    target VARCHAR(40) NOT NULL,
    readiness VARCHAR(20) NULL,
    shop_status VARCHAR(20) NULL,
    UNIQUE KEY uq_project_stage_events_target (company_id, stage_code, target)
);
