-- Send SMS from an event's details used to come with the "Plan the schedule"
-- skill; it is now its own skill, `send_sms`. Positions a company already
-- configured (a saved `skills` list) keep Send SMS when they plan the
-- schedule. Positions with no saved list get it from the defaults
-- (Scheduling Manager).
UPDATE company_position_sections
   SET skills = CONCAT(skills, ',send_sms')
 WHERE skills IS NOT NULL
   AND FIND_IN_SET('plan_schedule', skills) > 0
   AND FIND_IN_SET('send_sms', skills) = 0;
