-- Admin > Projects: the built-in line at the end of each stage ("Then: Start
-- fabrication") becomes an ordinary step the company can rename, move and
-- delete (owner, 2026-11-17). A step that ends the stage (`ends_stage`) is
-- checked off by itself when the project moves on from the stage, and its Auto
-- moves the project on (Awaiting Template, Templated), as the built-in line
-- did. Each stage's Notify, Schedule and Auto are copied onto its new step, so
-- nothing changes until someone edits it. `project_process_seeded` marks a
-- company that has its starting steps, so a deleted one doesn't come back.
SET SESSION lock_wait_timeout = 3;

ALTER TABLE project_process_steps
    ADD COLUMN ends_stage TINYINT(1) NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS project_process_seeded (
    company_id INT NOT NULL,
    seeded_at  DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO project_process_steps
    (company_id, stage_code, name, position, ends_stage, notify_sales_rep,
     auto_trigger, auto_ref_id, auto_ref_ids, auto_text, auto_days, auto_since)
SELECT c.id, b.stage_code, b.name,
       COALESCE((SELECT MAX(s.position) + 1 FROM project_process_steps s
                  WHERE s.company_id = c.id AND s.stage_code = b.stage_code), 0),
       1,
       EXISTS (SELECT 1 FROM project_stage_notify_rep r
                WHERE r.company_id = c.id AND r.stage_code = b.stage_code),
       a.auto_trigger, a.auto_ref_id, a.auto_ref_ids, LEFT(a.auto_text, 120), a.auto_days,
       a.auto_since
  FROM company c
  CROSS JOIN (SELECT 'new' AS stage_code, 'Schedule the template' AS name, 1 AS ord
              UNION ALL SELECT 'templated', 'Start fabrication', 2
              UNION ALL SELECT 'fabrication', 'Schedule the installation', 3
              UNION ALL SELECT 'installed', 'Confirm full payment', 4) b
  LEFT JOIN project_stage_auto a ON a.company_id = c.id AND a.stage_code = b.stage_code
 ORDER BY c.id, b.ord;

INSERT IGNORE INTO project_process_step_notify (step_id, user_id)
SELECT s.id, n.user_id
  FROM project_stage_notify n
  JOIN project_process_steps s
    ON s.company_id = n.company_id AND s.stage_code = n.stage_code AND s.ends_stage = 1
  JOIN users u ON u.id = n.user_id;

INSERT IGNORE INTO project_process_step_events (step_id, target, readiness, shop_status)
SELECT s.id, e.target, LEFT(e.readiness, 16), LEFT(e.shop_status, 16)
  FROM project_stage_events e
  JOIN project_process_steps s
    ON s.company_id = e.company_id AND s.stage_code = e.stage_code AND s.ends_stage = 1;

INSERT IGNORE INTO project_process_seeded (company_id)
SELECT id FROM company;
