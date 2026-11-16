-- A signature in columns (photo, icons and logo side by side, as Gmail makes
-- it) is a few thousand characters of HTML; VARCHAR(1000) cut it off. Pasted
-- pictures can be inline data, so MEDIUMTEXT. NULL: no signature.
ALTER TABLE users MODIFY COLUMN email_signature MEDIUMTEXT NULL;
