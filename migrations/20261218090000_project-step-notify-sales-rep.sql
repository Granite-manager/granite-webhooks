-- A step can also tell the sales rep assigned to the project. Additive only.
ALTER TABLE project_process_steps
    ADD COLUMN notify_sales_rep TINYINT(1) NOT NULL DEFAULT 0;
