-- A company's own steps in the Projects process, per stage, and which projects
-- have done them. Additive only.
CREATE TABLE project_process_steps (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    stage_code VARCHAR(20) NOT NULL,
    name VARCHAR(120) NOT NULL,
    position INT NOT NULL DEFAULT 0,
    created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    KEY idx_project_process_steps_stage (company_id, stage_code, position, id),
    CONSTRAINT fk_project_process_steps_company FOREIGN KEY (company_id) REFERENCES company(id)
);

CREATE TABLE project_process_steps_done (
    project_id INT NOT NULL,
    step_id INT NOT NULL,
    done_by INT NULL,
    done_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (project_id, step_id),
    KEY idx_project_process_steps_done_step (step_id),
    CONSTRAINT fk_project_process_steps_done_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_project_process_steps_done_step FOREIGN KEY (step_id) REFERENCES project_process_steps(id) ON DELETE CASCADE,
    CONSTRAINT fk_project_process_steps_done_user FOREIGN KEY (done_by) REFERENCES users(id) ON DELETE SET NULL
);
