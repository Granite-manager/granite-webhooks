-- The steps each stage starts with check off by the real work, not by a move
-- (owner, 2026-11-18). Awaiting Template: "Schedule the template" when the
-- template is booked, then a new "Template" when the template maker taps
-- Completed. In Fabrication: "Schedule the installation" when the installation
-- is booked, then a new "Installation" when the installer taps Completed.
-- Installed: "Confirm full payment" when it is paid in full. Templated keeps
-- "Start fabrication" (the move to In Fabrication). Data only.

-- Room for the new step right after each scheduling step.
UPDATE project_process_steps s
  JOIN project_process_steps b
    ON b.company_id = s.company_id AND b.stage_code = s.stage_code
   AND ((b.stage_code = 'new' AND b.name = 'Schedule the template')
        OR (b.stage_code = 'fabrication' AND b.name = 'Schedule the installation'))
   AND (b.auto_trigger IS NULL OR b.auto_trigger = 'stage_moved')
   SET s.position = s.position + 1
 WHERE s.position > b.position;

-- The template and the installation themselves.
INSERT INTO project_process_steps (company_id, stage_code, name, position, auto_trigger, auto_since)
SELECT company_id, stage_code,
       IF(stage_code = 'new', 'Template', 'Installation'),
       position + 1,
       IF(stage_code = 'new', 'template_completed', 'install_completed'),
       COALESCE(auto_since, UTC_TIMESTAMP(3))
  FROM project_process_steps
 WHERE ((stage_code = 'new' AND name = 'Schedule the template')
        OR (stage_code = 'fabrication' AND name = 'Schedule the installation'))
   AND (auto_trigger IS NULL OR auto_trigger = 'stage_moved');

-- Scheduling is done once the job is booked.
UPDATE project_process_steps
   SET auto_trigger = IF(stage_code = 'new', 'template_booked', 'install_booked'),
       auto_ref_id = NULL, auto_ref_ids = NULL, auto_text = NULL, auto_days = NULL,
       auto_moves = 0, auto_since = COALESCE(auto_since, UTC_TIMESTAMP(3))
 WHERE ((stage_code = 'new' AND name = 'Schedule the template')
        OR (stage_code = 'fabrication' AND name = 'Schedule the installation'))
   AND (auto_trigger IS NULL OR auto_trigger = 'stage_moved');

-- Any other step waiting for the move on from these stages waits for the work.
UPDATE project_process_steps
   SET auto_trigger = CASE stage_code
                        WHEN 'new' THEN 'template_completed'
                        WHEN 'fabrication' THEN 'install_completed'
                        ELSE 'payment_full'
                      END
 WHERE auto_trigger = 'stage_moved' AND stage_code IN ('new', 'fabrication', 'installed');
