-- Events have only notes (owner, 2026-11-19). Every description was moved
-- into notes on main that day; this moves any written since (by the API
-- still live before the notes-only release) and drops the column.
UPDATE events
   SET notes = CASE WHEN TRIM(COALESCE(notes, '')) = '' THEN TRIM(description)
                    ELSE CONCAT(TRIM(description), '\n\n', notes) END,
       description = NULL
 WHERE TRIM(COALESCE(description, '')) <> '';

ALTER TABLE events DROP COLUMN description;
