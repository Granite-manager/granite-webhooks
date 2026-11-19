-- Jobs never move to another stage by hand (owner, 2026-11-19): checking a
-- stage's last step moves them on. So no step waits for "the project moves
-- to the next stage" (`stage_moved`): "Start fabrication" is checked off by
-- hand, which moves the job to In Fabrication. Data only.
UPDATE project_process_steps
   SET auto_trigger = NULL, auto_ref_id = NULL, auto_ref_ids = NULL, auto_text = NULL,
       auto_days = NULL, auto_moves = 0, auto_since = NULL
 WHERE auto_trigger = 'stage_moved';
