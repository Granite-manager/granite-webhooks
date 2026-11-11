-- Issues can be marked solved (on the job card and the project's page), and
-- the office can report one on a project with no job. Additive only.
--
-- event_issues.event_id: NULL for an issue reported on a project itself.
-- event_issues.project_id: the project such an issue is on (NULL for a job's).
-- solved_at / solved_by / solved_by_name: who marked it solved, and when;
--   all NULL while it is open.
SET SESSION lock_wait_timeout = 3;

ALTER TABLE event_issues
  MODIFY event_id INT NULL,
  ADD COLUMN project_id INT NULL DEFAULT NULL AFTER event_id,
  ADD COLUMN solved_at TIMESTAMP NULL DEFAULT NULL,
  ADD COLUMN solved_by INT NULL DEFAULT NULL,
  ADD COLUMN solved_by_name VARCHAR(255) NULL DEFAULT NULL,
  ADD KEY idx_event_issues_project (project_id),
  ADD CONSTRAINT fk_event_issues_project FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE,
  ADD CONSTRAINT fk_event_issues_solved_by FOREIGN KEY (solved_by) REFERENCES users (id) ON DELETE SET NULL;
