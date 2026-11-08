-- Auto-confirm: an unclear reply ("ues") gets one "did you mean YES or NO?"
-- text; this marks that it went, so it is never sent twice.
ALTER TABLE schedule_text_answers ADD COLUMN clarified TINYINT(1) NOT NULL DEFAULT 0;
