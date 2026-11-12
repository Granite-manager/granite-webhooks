-- Who hears when a project moves on from a stage: the built-in line at the end
-- of each stage in Admin > Projects ("Then: Start fabrication") takes Notify like
-- a step does. Additive only.
CREATE TABLE project_stage_notify (
    company_id INT NOT NULL,
    stage_code VARCHAR(20) NOT NULL,
    user_id INT NOT NULL,
    PRIMARY KEY (company_id, stage_code, user_id),
    KEY idx_project_stage_notify_user (user_id)
);

-- Also tells the sales rep assigned to the project.
CREATE TABLE project_stage_notify_rep (
    company_id INT NOT NULL,
    stage_code VARCHAR(20) NOT NULL,
    PRIMARY KEY (company_id, stage_code)
);
