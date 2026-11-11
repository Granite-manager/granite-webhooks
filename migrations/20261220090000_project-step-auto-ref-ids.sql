-- A step's Auto rule can name several email or text templates (comma-separated
-- ids; `auto_ref_id` keeps the first). Additive only.
ALTER TABLE project_process_steps
    ADD COLUMN auto_ref_ids VARCHAR(255) NULL;
