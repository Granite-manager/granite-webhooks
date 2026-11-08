-- A task's remembered time can be reset; the uses before it stop counting.
ALTER TABLE project_activities
    ADD COLUMN counts_for_time TINYINT(1) NOT NULL DEFAULT 1;
