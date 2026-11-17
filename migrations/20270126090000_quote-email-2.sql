-- A quote's second email (Edit Quote Address → "Add email"). Kept on the
-- quote only, like `quotes.email`; the customer's emails are not changed.
ALTER TABLE quotes ADD COLUMN email_2 VARCHAR(255) NULL AFTER email;

-- Existing quotes take their customer's second email: the oldest one that is
-- not the primary and not already the quote's email. Never from the import's
-- shared customer, whose emails belong to many people.
UPDATE quotes q
  JOIN customers c ON c.id = q.customer_id
   SET q.email_2 = (
         SELECT TRIM(ce.email)
           FROM customers_emails ce
          WHERE ce.customer_id = c.id
            AND TRIM(COALESCE(ce.email, '')) <> ''
            AND NOT (ce.id <=> c.email_id)
            AND LOWER(TRIM(ce.email)) <> LOWER(TRIM(COALESCE(q.email, '')))
          ORDER BY ce.created_at, ce.id
          LIMIT 1)
 WHERE q.email_2 IS NULL
   AND NOT COALESCE(c.name LIKE '% import' AND c.created_by <=> c.name, 0);
