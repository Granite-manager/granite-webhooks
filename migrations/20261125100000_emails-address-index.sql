-- The bare address of each message's sender and receiver ("Jen <JEN@x.com>"
-- -> "jen@x.com"), indexed, so a customer's mail is found by address without
-- reading every message: `GET /v1/deals/{id}/emails` scanned all of `emails`
-- (about 13k rows with their bodies) on every call, and on 2026-11-06 ~170
-- calls a minute at 2 s each filled the database's connections.
--
-- VIRTUAL: nothing is stored or rewritten, so adding them is instant; the
-- indexes are built in place without blocking writes. The expression is the
-- one the queries used inline, and stays within VARCHAR(255) because it is a
-- part of a VARCHAR(255) column.
ALTER TABLE emails
  ADD COLUMN sender_address VARCHAR(255)
    GENERATED ALWAYS AS (LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(sender_email, '<', -1), '>', 1)))) VIRTUAL,
  ADD COLUMN receiver_address VARCHAR(255)
    GENERATED ALWAYS AS (LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(receiver_email, '<', -1), '>', 1)))) VIRTUAL,
  ADD INDEX idx_emails_sender_address (sender_address),
  ADD INDEX idx_emails_receiver_address (receiver_address);
