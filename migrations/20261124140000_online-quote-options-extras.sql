-- Customer quote options are now the company's real extras (Admin > Quotes >
-- Extras), not typed names: a picked option goes on the quote as that extra,
-- priced by the price list.
ALTER TABLE online_quote_options
  ADD COLUMN extra_id INT NULL AFTER company_id,
  ADD CONSTRAINT fk_online_quote_options_extra
    FOREIGN KEY (extra_id) REFERENCES quote_extras(id) ON DELETE SET NULL;

-- Link the three starter options to the company's matching extra, if any.
UPDATE online_quote_options o
   SET o.extra_id = (
     SELECT x.id FROM quote_extras x
      WHERE x.company_id = o.company_id AND x.deleted_at IS NULL
        AND CASE o.name
              WHEN 'Laminate tear out' THEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(x.item_json, '$.name'))) LIKE '%laminate%'
              WHEN 'Stone tear out' THEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(x.item_json, '$.name'))) LIKE '%stone%tear%out%'
              WHEN '10 year sealer' THEN LOWER(JSON_UNQUOTE(JSON_EXTRACT(x.item_json, '$.name'))) LIKE '%sealer%'
              ELSE LOWER(TRIM(JSON_UNQUOTE(JSON_EXTRACT(x.item_json, '$.name')))) = LOWER(TRIM(o.name))
            END
      ORDER BY x.sequence, x.id
      LIMIT 1)
 WHERE o.extra_id IS NULL AND o.deleted_at IS NULL;

-- Options with no matching extra go; the admin picks real extras instead.
UPDATE online_quote_options SET deleted_at = NOW()
 WHERE extra_id IS NULL AND deleted_at IS NULL;
