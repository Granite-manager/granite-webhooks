-- Auto-confirm: the "Confirmation reminder" now follows an unanswered
-- confirmation after 6 hours instead of 24 (new companies get 6 from
-- `seed_auto_confirm`). Only texts still on the old 24-hour default change;
-- admins adjust each text under Admin > Schedule > Auto-confirm.
UPDATE schedule_notification_templates
   SET followup_after_hours = 6
 WHERE kind = 'question'
   AND followup_after_hours = 24
   AND deleted_at IS NULL;
