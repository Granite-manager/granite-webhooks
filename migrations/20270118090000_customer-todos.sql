-- Customer to-dos: what the customer must do before install (put in
-- overhang supports, level the cabinets, bring the sink to the shop). Admins
-- keep the lists in Admin > Schedule; the template maker ticks what applies
-- at the template appointment; the sales rep and everyone on the project see
-- them and tick them off. Additive only.
--
-- customer_checklists: a company's lists, in `sort_order`.
-- customer_checklist_items: a list's to-dos, in `sort_order`.
-- customer_todos: one to-do of one customer, from a template appointment
--   (event_id) or added later; project_id is the project it was for then.
-- customer_todo_reports: a template appointment's to-dos were sent (count) or
--   the template maker said there is nothing to do (count 0).
SET SESSION lock_wait_timeout = 3;

CREATE TABLE IF NOT EXISTS customer_checklists (
  id          INT          NOT NULL AUTO_INCREMENT,
  company_id  INT          NOT NULL,
  name        VARCHAR(120) NOT NULL,
  sort_order  INT          NOT NULL DEFAULT 0,
  created_at  TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  deleted_at  TIMESTAMP    NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_customer_checklists_company (company_id),
  CONSTRAINT fk_customer_checklists_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS customer_checklist_items (
  id            INT          NOT NULL AUTO_INCREMENT,
  checklist_id  INT          NOT NULL,
  label         VARCHAR(200) NOT NULL,
  sort_order    INT          NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  KEY idx_customer_checklist_items_list (checklist_id),
  CONSTRAINT fk_customer_checklist_items_list FOREIGN KEY (checklist_id) REFERENCES customer_checklists (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS customer_todos (
  id              INT          NOT NULL AUTO_INCREMENT,
  company_id      INT          NOT NULL,
  customer_id     INT          NOT NULL,
  event_id        INT          NULL,
  project_id      INT          NULL,
  checklist_id    INT          NULL,
  label           VARCHAR(200) NOT NULL,
  note            VARCHAR(1000) NULL,
  created_by      INT          NULL,
  created_by_name VARCHAR(255) NULL,
  created_at      TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  done_at         TIMESTAMP    NULL DEFAULT NULL,
  done_by         INT          NULL,
  done_by_name    VARCHAR(255) NULL,
  deleted_at      TIMESTAMP    NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_customer_todos_customer (customer_id),
  KEY idx_customer_todos_event (event_id),
  KEY idx_customer_todos_project (project_id),
  CONSTRAINT fk_customer_todos_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_customer_todos_customer FOREIGN KEY (customer_id) REFERENCES customers (id) ON DELETE CASCADE,
  CONSTRAINT fk_customer_todos_event FOREIGN KEY (event_id) REFERENCES events (id) ON DELETE SET NULL,
  CONSTRAINT fk_customer_todos_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL,
  CONSTRAINT fk_customer_todos_done_by FOREIGN KEY (done_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS customer_todo_reports (
  event_id        INT          NOT NULL,
  company_id      INT          NOT NULL,
  todo_count      INT          NOT NULL DEFAULT 0,
  created_by      INT          NULL,
  created_by_name VARCHAR(255) NULL,
  created_at      TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (event_id),
  CONSTRAINT fk_customer_todo_reports_event FOREIGN KEY (event_id) REFERENCES events (id) ON DELETE CASCADE,
  CONSTRAINT fk_customer_todo_reports_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_customer_todo_reports_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
