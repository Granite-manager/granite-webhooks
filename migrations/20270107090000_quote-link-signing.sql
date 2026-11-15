-- Admin › Quotes › Automation "Sign on the quote link": the customer's quote
-- link shows Accept & Sign. Off for every company (signing left the link on
-- 2026-10-30; this brings it back as a choice).
ALTER TABLE company ADD COLUMN quote_link_signing TINYINT(1) NOT NULL DEFAULT 0;
