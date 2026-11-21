-- AI call notes are off unless the user turns them on (owner, 2026-11-20).
ALTER TABLE phone_user_settings MODIFY ai_notes TINYINT(1) NOT NULL DEFAULT 0;
UPDATE phone_user_settings SET ai_notes = 0 WHERE ai_notes <> 0;
