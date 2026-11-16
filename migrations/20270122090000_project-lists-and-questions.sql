-- Projects: to-do list tasks, who is responsible for each step, the
-- company's own to-do lists, assigned customer to-dos, and questions asked to
-- only the people they concern (owner, 2026-11-16). Additive only.
--
-- project_activities.kind: `text` (a plain task) or `list` (a to-do list).
-- project_task_items: a to-do list task's items, ticked one by one.
-- project_process_steps: who is responsible (a person, or the project's sales
--   rep) and how many days after the project reaches the stage it is due.
-- project_step_items: a step's own to-do list; project_step_items_done its ticks.
-- project_step_owners: a project's own person for a step, in place of the rule.
-- project_lists / project_list_items: the company's lists to fill a to-do list.
-- customer_checklist_items / customer_todos.assigned_user_id: who follows the
--   to-do up; null on a list item means the sales rep.
-- project_questions: a question with its own small chat; only its members
--   (or everyone, when `everyone`) are told.
SET SESSION lock_wait_timeout = 3;

ALTER TABLE project_activities
  ADD COLUMN kind VARCHAR(10) NOT NULL DEFAULT 'text';

CREATE TABLE IF NOT EXISTS project_task_items (
  id         INT          NOT NULL AUTO_INCREMENT,
  task_id    INT          NOT NULL,
  label      VARCHAR(200) NOT NULL,
  position   INT          NOT NULL DEFAULT 0,
  done_at    DATETIME(3)  NULL DEFAULT NULL,
  done_by    INT          NULL,
  PRIMARY KEY (id),
  KEY idx_project_task_items_task (task_id, position, id),
  CONSTRAINT fk_project_task_items_task FOREIGN KEY (task_id) REFERENCES project_activities (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_task_items_done_by FOREIGN KEY (done_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

ALTER TABLE project_process_steps
  ADD COLUMN responsible_user_id INT NULL,
  ADD COLUMN responsible_sales_rep TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN due_days INT NULL,
  ADD CONSTRAINT fk_project_process_steps_responsible FOREIGN KEY (responsible_user_id) REFERENCES users (id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS project_step_items (
  id        INT          NOT NULL AUTO_INCREMENT,
  step_id   INT          NOT NULL,
  label     VARCHAR(200) NOT NULL,
  position  INT          NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  KEY idx_project_step_items_step (step_id, position, id),
  CONSTRAINT fk_project_step_items_step FOREIGN KEY (step_id) REFERENCES project_process_steps (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_step_items_done (
  project_id INT         NOT NULL,
  item_id    INT         NOT NULL,
  part_id    INT         NOT NULL DEFAULT 0,
  done_by    INT         NULL,
  done_at    DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  PRIMARY KEY (project_id, item_id, part_id),
  KEY idx_project_step_items_done_item (item_id),
  CONSTRAINT fk_project_step_items_done_project FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_step_items_done_item FOREIGN KEY (item_id) REFERENCES project_step_items (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_step_items_done_by FOREIGN KEY (done_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_step_owners (
  project_id INT NOT NULL,
  step_id    INT NOT NULL,
  user_id    INT NOT NULL,
  PRIMARY KEY (project_id, step_id),
  KEY idx_project_step_owners_step (step_id),
  CONSTRAINT fk_project_step_owners_project FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_step_owners_step FOREIGN KEY (step_id) REFERENCES project_process_steps (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_step_owners_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_lists (
  id          INT          NOT NULL AUTO_INCREMENT,
  company_id  INT          NOT NULL,
  name        VARCHAR(120) NOT NULL,
  position    INT          NOT NULL DEFAULT 0,
  created_at  DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  deleted_at  DATETIME(3)  NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_project_lists_company (company_id, deleted_at, position),
  CONSTRAINT fk_project_lists_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_list_items (
  id        INT          NOT NULL AUTO_INCREMENT,
  list_id   INT          NOT NULL,
  label     VARCHAR(200) NOT NULL,
  position  INT          NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  KEY idx_project_list_items_list (list_id, position, id),
  CONSTRAINT fk_project_list_items_list FOREIGN KEY (list_id) REFERENCES project_lists (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

ALTER TABLE customer_checklist_items
  ADD COLUMN assigned_user_id INT NULL,
  ADD CONSTRAINT fk_customer_checklist_items_assignee FOREIGN KEY (assigned_user_id) REFERENCES users (id) ON DELETE SET NULL;

ALTER TABLE customer_todos
  ADD COLUMN assigned_user_id INT NULL,
  ADD CONSTRAINT fk_customer_todos_assignee FOREIGN KEY (assigned_user_id) REFERENCES users (id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS project_questions (
  id              INT          NOT NULL AUTO_INCREMENT,
  company_id      INT          NOT NULL,
  project_id      INT          NOT NULL,
  title           VARCHAR(200) NOT NULL,
  everyone        TINYINT(1)   NOT NULL DEFAULT 0,
  created_by      INT          NULL,
  created_at      DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  last_message_at DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  resolved_at     DATETIME(3)  NULL DEFAULT NULL,
  PRIMARY KEY (id),
  KEY idx_project_questions_project (project_id, last_message_at),
  CONSTRAINT fk_project_questions_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_questions_project FOREIGN KEY (project_id) REFERENCES projects (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_questions_created_by FOREIGN KEY (created_by) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_question_members (
  question_id  INT         NOT NULL,
  user_id      INT         NOT NULL,
  last_read_at DATETIME(3) NULL DEFAULT NULL,
  PRIMARY KEY (question_id, user_id),
  KEY idx_project_question_members_user (user_id),
  CONSTRAINT fk_project_question_members_question FOREIGN KEY (question_id) REFERENCES project_questions (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_question_members_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS project_question_messages (
  id          INT          NOT NULL AUTO_INCREMENT,
  question_id INT          NOT NULL,
  user_id     INT          NULL,
  user_name   VARCHAR(255) NULL,
  body        TEXT         NOT NULL,
  created_at  DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  PRIMARY KEY (id),
  KEY idx_project_question_messages_question (question_id, id),
  CONSTRAINT fk_project_question_messages_question FOREIGN KEY (question_id) REFERENCES project_questions (id) ON DELETE CASCADE,
  CONSTRAINT fk_project_question_messages_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
