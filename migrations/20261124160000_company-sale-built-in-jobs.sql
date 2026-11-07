-- Admin › Quotes › Automation › Event templates: the built-in Template and
-- Installation events a sold quote adds (with Jobs to Unscheduled on) can be
-- removed one by one. Both stay on for every company.
ALTER TABLE company
  ADD COLUMN sale_template_job TINYINT(1) NOT NULL DEFAULT 1,
  ADD COLUMN sale_installation_job TINYINT(1) NOT NULL DEFAULT 1;
