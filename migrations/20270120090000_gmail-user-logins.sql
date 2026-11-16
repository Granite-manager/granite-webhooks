-- A user's own Gmail sign-in (My Account > Connect Gmail), with Google's
-- send-only permission. Emails the user sends from the CRM go out through
-- their Gmail instead of Amazon SES, so the company needs no domain set up
-- in SES. `email` is the Gmail address that signed in (it becomes the From).
-- The refresh token is encrypted like `ringcentral_user_logins`.
CREATE TABLE gmail_user_logins (
  user_id INT NOT NULL PRIMARY KEY,
  company_id INT NOT NULL,
  email VARCHAR(255) NOT NULL,
  refresh_token BLOB NOT NULL,
  connected_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_gmail_user_logins_company (company_id)
);
