-- Pictures on a project task: optional, several per task (files of the
-- `events` upload folder, as on an issue). Additive only.
SET SESSION lock_wait_timeout = 3;

CREATE TABLE IF NOT EXISTS project_task_photos (
  id       INT          NOT NULL AUTO_INCREMENT,
  task_id  INT          NOT NULL,
  url      VARCHAR(512) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_project_task_photos_task (task_id),
  CONSTRAINT fk_project_task_photos_task FOREIGN KEY (task_id) REFERENCES project_activities (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
