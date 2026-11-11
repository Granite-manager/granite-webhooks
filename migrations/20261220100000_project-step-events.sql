-- Admin > Projects > Schedule: checking a step off can change the readiness
-- (`events.status`) and the shop status of the project's events. `target` is
-- `templates`, `installations` (the built-in calendars) or `event:<id>` (a
-- quote_sale_event_templates row). Additive only.
CREATE TABLE project_process_step_events (
    id          INT         NOT NULL AUTO_INCREMENT,
    step_id     INT         NOT NULL,
    target      VARCHAR(40) NOT NULL,
    readiness   VARCHAR(16) NULL,
    shop_status VARCHAR(16) NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uq_project_step_events (step_id, target),
    CONSTRAINT fk_project_step_events_step FOREIGN KEY (step_id)
        REFERENCES project_process_steps (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
