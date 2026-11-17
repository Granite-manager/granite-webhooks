-- Admin > Projects: no more "Ends the stage" flag on a step (owner,
-- 2026-11-17). What it did is now the step's own Auto, which the company can
-- change, turn off, move or delete with the step:
-- * Auto "The project moves to the next stage" (`stage_moved`): checked off
--   when the project moves on from the stage.
-- * Auto's "Then move the project on" (`auto_moves`, Awaiting Template and
--   Templated): when its action happens, the project moves on.
-- The starting steps get these, so nothing changes until someone edits them.
-- `ends_stage` stays for the API still running until this one is deployed.
SET SESSION lock_wait_timeout = 3;

ALTER TABLE project_process_steps
    ADD COLUMN auto_moves TINYINT(1) NOT NULL DEFAULT 0;

UPDATE project_process_steps
   SET auto_moves = 1
 WHERE ends_stage = 1 AND auto_trigger IS NOT NULL
   AND stage_code IN ('new', 'templated');

UPDATE project_process_steps
   SET auto_trigger = 'stage_moved', auto_since = UTC_TIMESTAMP(3)
 WHERE ends_stage = 1 AND auto_trigger IS NULL;
