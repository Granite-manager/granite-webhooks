-- A step on a project can have a note and files, both optional (owner,
-- 2026-11-18: Dasha wants to put the layout on the Layout step). One note per
-- step of a project (part 0: a project with no parts); files stay in
-- project_files, tied to the step they were added on.

CREATE TABLE project_step_notes (
    project_id INT NOT NULL,
    step_id INT NOT NULL,
    part_id INT NOT NULL DEFAULT 0,
    note TEXT NOT NULL,
    updated_by INT NULL,
    updated_at DATETIME(3) NOT NULL,
    PRIMARY KEY (project_id, step_id, part_id),
    KEY idx_project_step_notes_step (step_id),
    CONSTRAINT fk_project_step_note_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_project_step_note_step FOREIGN KEY (step_id) REFERENCES project_process_steps(id) ON DELETE CASCADE
);

ALTER TABLE project_files
    ADD COLUMN step_id INT NULL,
    ADD COLUMN part_id INT NULL,
    ADD KEY idx_project_files_step (step_id);
