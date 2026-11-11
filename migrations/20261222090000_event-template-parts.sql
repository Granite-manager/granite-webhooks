-- Admin > Quotes > Automation > Event templates: a template can be linked to a
-- part type (Backsplash, Cabinets...). The event it adds when a quote is sold
-- then belongs to a part of that type on the sold job's project, and the part
-- is added to the project when it has none of that type. Additive only.
ALTER TABLE quote_sale_event_templates
    ADD COLUMN part_type_id INT NULL,
    ADD CONSTRAINT fk_quote_sale_event_templates_part_type FOREIGN KEY (part_type_id)
        REFERENCES project_part_types (id) ON DELETE SET NULL;

-- The part type a sale's event is for, until the job has a project to put the
-- part on (the project is made after the sale's events). NULL: the project's
-- first part.
ALTER TABLE events
    ADD COLUMN planned_part_type_id INT NULL,
    ADD CONSTRAINT fk_events_planned_part_type FOREIGN KEY (planned_part_type_id)
        REFERENCES project_part_types (id) ON DELETE SET NULL;

-- A part a sale added by itself: Undo sale takes it away again when its jobs
-- are gone and nothing was done on it.
ALTER TABLE project_parts
    ADD COLUMN auto TINYINT(1) NOT NULL DEFAULT 0;
