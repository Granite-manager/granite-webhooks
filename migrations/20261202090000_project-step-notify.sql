-- Who hears when a project's process step is checked off. Additive only.
CREATE TABLE project_process_step_notify (
    step_id INT NOT NULL,
    user_id INT NOT NULL,
    PRIMARY KEY (step_id, user_id),
    KEY idx_project_process_step_notify_user (user_id),
    CONSTRAINT fk_project_process_step_notify_step FOREIGN KEY (step_id) REFERENCES project_process_steps(id) ON DELETE CASCADE,
    CONSTRAINT fk_project_process_step_notify_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
