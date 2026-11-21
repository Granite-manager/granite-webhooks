-- Hashes the web session ids stored before ids were hashed: a bare id
-- (a 36-character UUID) becomes its SHA-256 (hex), the value the API stores
-- for new sessions (`auth::session::session_key`). The cookies keep the bare
-- id, so nobody is signed out.
--
-- Applied only after the API release that reads hashed ids (PR #777) was on
-- every API Lambda: the old release looks rows up by the bare id and would
-- sign every web user out.
--
-- One transaction (sqlx wraps a migration in one): a browser phone's
-- `device_sessions.web_session_id` is a copy of its `sessions.id`, so both
-- change together and keep joining. Running it again changes nothing.

UPDATE device_sessions
   SET web_session_id = SHA2(web_session_id, 256)
 WHERE CHAR_LENGTH(web_session_id) = 36;

UPDATE sessions
   SET id = SHA2(id, 256)
 WHERE CHAR_LENGTH(id) = 36;
