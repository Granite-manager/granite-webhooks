-- Issues on schedule events: a problem the crew (an installer, a template
-- maker, anyone on the job) reports from the app, with a description and
-- pictures. Filing one tells the job's sales rep (a bell notification and a
-- phone push). Additive only.
--
-- event_issues: one row per report; created_by is the reporter's user id
--   (NULL once that user is gone), created_by_name their name then.
-- event_issue_images: its pictures (files of the `events` upload folder).
-- notifications.event_id: the event a notification is about, so the app
--   opens it (set for `event_issue`; NULL for every other type).
SET SESSION lock_wait_timeout = 3;

CREATE TABLE IF NOT EXISTS event_issues (
  id              INT          NOT NULL AUTO_INCREMENT,
  event_id        INT          NOT NULL,
  company_id      INT          NOT NULL,
  -- 1 to 2000 characters
  description     TEXT         NOT NULL,
  created_by      INT          NULL,
  created_by_name VARCHAR(255) NULL,
  created_at      TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  deleted_at      TIMESTAMP    NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_event_issues_event (event_id),
  CONSTRAINT fk_event_issues_event FOREIGN KEY (event_id) REFERENCES events (id) ON DELETE CASCADE,
  CONSTRAINT fk_event_issues_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_event_issues_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS event_issue_images (
  id       INT          NOT NULL AUTO_INCREMENT,
  issue_id INT          NOT NULL,
  url      VARCHAR(512) NOT NULL,
  PRIMARY KEY (id),
  KEY idx_event_issue_images_issue (issue_id),
  CONSTRAINT fk_event_issue_images_issue FOREIGN KEY (issue_id) REFERENCES event_issues (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

ALTER TABLE notifications ADD COLUMN event_id INT NULL DEFAULT NULL;
