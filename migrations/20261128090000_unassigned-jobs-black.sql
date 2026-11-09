-- Unassigned events on Installations and Templates are black. Turn the ones
-- still in the old default blue black.
UPDATE events e
  JOIN calendars c ON c.id = e.calendar_id
   SET e.color = 'black'
 WHERE c.slug IN ('installations', 'templates')
   AND e.assigned_user_id IS NULL
   AND e.color = 'blue';
