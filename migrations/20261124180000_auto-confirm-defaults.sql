-- Admin > Schedule > Customer notifications > Auto-confirm: every company
-- starts with these texts (`question` kind: AI reads the YES/NO reply), as
-- new companies get them from `seed_auto_confirm`. The template and
-- installation confirmations are each followed 24 hours later by the reminder
-- when the customer has not replied. Companies can edit or delete them.

-- Marks the rows added here until they are linked.
ALTER TABLE schedule_notification_templates ADD COLUMN seed_key TINYINT NULL DEFAULT NULL;

INSERT INTO schedule_notification_templates
    (company_id, name, kind, channel, body, position, seed_key)
SELECT c.id, d.name, 'question', 'sms', d.body,
       COALESCE((SELECT MAX(t.position) FROM schedule_notification_templates t
                  WHERE t.company_id = c.id AND t.deleted_at IS NULL), 0) + d.n,
       d.n
  FROM company c
  CROSS JOIN (
      SELECT 1 AS n, 'Template confirmation' AS name,
             'Hi {{customer.first_name}}, this is {{company.name}}. Our technician is scheduled to measure for your countertops on {{appointment.date}}, arriving between {{appointment.time}} and {{appointment.end_time}}. Does this time work for you? Reply YES to confirm, or NO and we''ll find another time that suits you.' AS body
      UNION ALL
      SELECT 2, 'Installation confirmation',
             'Hi {{customer.first_name}}, this is {{company.name}}. Our installation team is scheduled to install your countertops on {{appointment.date}}, arriving between {{appointment.time}} and {{appointment.end_time}}. Does this time work for you? Reply YES to confirm, or NO and we''ll find another time that suits you.'
      UNION ALL
      SELECT 3, 'Confirmation reminder',
             'Hi {{customer.first_name}}, just following up from {{company.name}}. Does {{appointment.date}} between {{appointment.time}} and {{appointment.end_time}} still work for you? Reply YES to confirm, or NO and we''ll find another time.'
  ) d;

-- Each text is for its built-in calendar(s).
INSERT IGNORE INTO schedule_notification_template_calendars (template_id, calendar_id)
SELECT t.id, cal.id
  FROM schedule_notification_templates t
  JOIN calendars cal ON cal.company_id = t.company_id AND cal.deleted_at IS NULL
 WHERE (t.seed_key = 1 AND cal.slug = 'templates')
    OR (t.seed_key = 2 AND cal.slug = 'installations')
    OR (t.seed_key = 3 AND cal.slug IN ('templates', 'installations'));

-- No reply in 24 hours: the reminder.
UPDATE schedule_notification_templates t
  JOIN schedule_notification_templates r
    ON r.company_id = t.company_id AND r.seed_key = 3
   SET t.followup_template_id = r.id, t.followup_after_hours = 24
 WHERE t.seed_key IN (1, 2);

ALTER TABLE schedule_notification_templates DROP COLUMN seed_key;

-- A YES reply to an Auto-confirm text turns the job Confirmed on its own:
-- reading replies is on for every company (they can still turn it off).
ALTER TABLE company MODIFY COLUMN schedule_ai_ready TINYINT(1) NOT NULL DEFAULT 1;
UPDATE company SET schedule_ai_ready = 1;
