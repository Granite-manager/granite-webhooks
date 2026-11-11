-- Projects process steps that check themselves off when something happens
-- (Admin > Projects > Auto). Additive only.

-- The step's rule: what happens, and which template, document, task or days.
-- `auto_since` is when the rule was set: only what happens after it counts.
ALTER TABLE project_process_steps
    ADD COLUMN auto_trigger VARCHAR(40) NULL,
    ADD COLUMN auto_ref_id INT NULL,
    ADD COLUMN auto_text VARCHAR(120) NULL,
    ADD COLUMN auto_days INT NULL,
    ADD COLUMN auto_since DATETIME(3) NULL;

-- Checked off by its rule, not by a person.
ALTER TABLE project_process_steps_done
    ADD COLUMN auto TINYINT(1) NOT NULL DEFAULT 0;

-- A person unchecked a step its rule checked: the rule leaves it alone on
-- that project until someone checks it again.
CREATE TABLE project_process_step_auto_off (
    project_id INT NOT NULL,
    step_id INT NOT NULL,
    PRIMARY KEY (project_id, step_id),
    CONSTRAINT fk_project_step_auto_off_project FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_project_step_auto_off_step FOREIGN KEY (step_id) REFERENCES project_process_steps(id) ON DELETE CASCADE
);

-- Every email and text sent from a template, and to whom.
CREATE TABLE sent_templates (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    kind ENUM('email', 'sms') NOT NULL,
    template_id INT NOT NULL,
    deal_id BIGINT UNSIGNED NULL,
    customer_id INT NULL,
    phone10 VARCHAR(20) NULL,
    email VARCHAR(255) NULL,
    sent_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    KEY idx_sent_templates (company_id, kind, template_id, sent_at)
);

-- When QuickBooks first showed any payment on the project's invoices.
ALTER TABLE projects ADD COLUMN qbo_first_paid_at DATETIME NULL;
