-- Each person sets their own daily minimum calls, texts and emails on the
-- dashboard's Outreach activity widget. Everyone starts from the minimums
-- their company had set.
ALTER TABLE users
  ADD COLUMN daily_calls_goal SMALLINT UNSIGNED NULL,
  ADD COLUMN daily_sms_goal SMALLINT UNSIGNED NULL,
  ADD COLUMN daily_emails_goal SMALLINT UNSIGNED NULL;

UPDATE users u
  JOIN company c ON c.id = u.company_id
   SET u.daily_calls_goal = c.daily_calls_goal,
       u.daily_sms_goal = c.daily_sms_goal,
       u.daily_emails_goal = c.daily_emails_goal;
