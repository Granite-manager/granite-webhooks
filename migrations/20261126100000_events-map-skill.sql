-- The schedule's events map is its own skill, `events_map`, given to Scheduling
-- Manager automatically. Scheduling Manager positions a company already
-- configured (a saved `skills` list) get it here; those with no saved list get
-- it from the defaults.
UPDATE company_position_sections
   SET skills = CONCAT_WS(',', NULLIF(skills, ''), 'events_map')
 WHERE position_id = 13
   AND skills IS NOT NULL
   AND FIND_IN_SET('events_map', skills) = 0;
