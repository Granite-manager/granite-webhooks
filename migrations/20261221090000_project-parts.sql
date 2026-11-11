-- Projects: parts. A project can hold several parts (Countertops, a
-- full-height backsplash, Cabinets), each going through the stages on its
-- own with its own template, fabrication and installation. A project with no
-- rows in `project_parts` works as before. Additive only.

-- The company's kinds of part, and which stages each one uses (Cabinets have
-- no template and no fabrication: they only wait for their installation).
CREATE TABLE project_part_types (
    id               INT         NOT NULL AUTO_INCREMENT,
    company_id       INT         NOT NULL,
    name             VARCHAR(80) NOT NULL,
    uses_template    TINYINT(1)  NOT NULL DEFAULT 1,
    uses_fabrication TINYINT(1)  NOT NULL DEFAULT 1,
    position         INT         NOT NULL DEFAULT 0,
    created_at       DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    deleted_at       DATETIME    NULL,
    PRIMARY KEY (id),
    KEY idx_project_part_types (company_id, deleted_at, position, id),
    CONSTRAINT fk_project_part_types_company FOREIGN KEY (company_id) REFERENCES company (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- A project's parts. `stage_code` is the last stage someone moved the part to
-- by hand, `stage_cap` a stage it was moved back to (as on `projects`).
CREATE TABLE project_parts (
    id            INT         NOT NULL AUTO_INCREMENT,
    project_id    INT         NOT NULL,
    company_id    INT         NOT NULL,
    type_id       INT         NOT NULL,
    name          VARCHAR(80) NOT NULL,
    stage_code    VARCHAR(20) NOT NULL DEFAULT 'new',
    stage_cap     VARCHAR(20) NULL,
    fabricated_at DATETIME    NULL,
    position      INT         NOT NULL DEFAULT 0,
    created_by    INT         NULL,
    created_at    DATETIME    NOT NULL,
    PRIMARY KEY (id),
    KEY idx_project_parts (project_id, position, id),
    CONSTRAINT fk_project_parts_project FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE,
    CONSTRAINT fk_project_parts_company FOREIGN KEY (company_id) REFERENCES company (id),
    CONSTRAINT fk_project_parts_type FOREIGN KEY (type_id) REFERENCES project_part_types (id),
    CONSTRAINT fk_project_parts_creator FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- The part a template or installation is for. NULL: the project's first part.
ALTER TABLE events
    ADD COLUMN project_part_id INT NULL,
    ADD KEY idx_events_project_part (project_part_id),
    ADD CONSTRAINT fk_events_project_part FOREIGN KEY (project_part_id)
        REFERENCES project_parts (id) ON DELETE SET NULL;

-- Steps are checked off per part. 0: a project without parts.
ALTER TABLE project_process_steps_done
    ADD COLUMN part_id INT NOT NULL DEFAULT 0,
    DROP PRIMARY KEY,
    ADD PRIMARY KEY (project_id, step_id, part_id);

ALTER TABLE project_process_step_auto_off
    ADD COLUMN part_id INT NOT NULL DEFAULT 0,
    DROP PRIMARY KEY,
    ADD PRIMARY KEY (project_id, step_id, part_id);

-- The part a history line is about.
ALTER TABLE project_stage_history
    ADD COLUMN part_id INT NULL;
