-- Steps whose people were told the step is up next on a project (Admin >
-- Projects > Notify): one row per project, part (0: none) and step.
CREATE TABLE project_process_step_arrived (
    project_id INT NOT NULL,
    part_id INT NOT NULL DEFAULT 0,
    step_id INT NOT NULL,
    arrived_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    PRIMARY KEY (project_id, part_id, step_id),
    KEY idx_project_step_arrived_step (step_id)
);

-- Companies whose steps already up were noted quietly (the first run).
CREATE TABLE project_step_arrivals_started (
    company_id INT NOT NULL PRIMARY KEY,
    started_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3)
);

-- A customer's events made since their deal began (a template or
-- installation booked by hand) belong to the deal's project, when the
-- customer has no other project. New projects do this when they start.
UPDATE events e
  JOIN projects p ON p.customer_id = e.customer_id
  JOIN deals d ON d.id = p.source_deal_id
  JOIN calendars cal ON cal.id = e.calendar_id AND cal.company_id = p.company_id
   SET e.project_id = p.id
 WHERE e.project_id IS NULL
   AND e.deleted_date IS NULL
   AND e.created_date >= d.created_at - INTERVAL 1 DAY
   AND NOT EXISTS (SELECT 1 FROM projects o
                    WHERE o.customer_id = p.customer_id AND o.id <> p.id);
