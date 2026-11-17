-- A company can hide the shared starter pipeline (groups_list id 1) from its
-- deal boards once it has its own default pipeline.
ALTER TABLE company
  ADD COLUMN shared_pipeline_hidden TINYINT(1) NOT NULL DEFAULT 0;
