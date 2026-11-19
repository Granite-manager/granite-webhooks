-- Indexes for the queries that still showed in the slow query log after the
-- 2026-11-15 query work. Timings are on a copy of the production data.

-- Customers, Companies view: each company's phones were read with a scan of
-- every customer of the company, once per company (2.2 s for 74 companies;
-- 16 ms with this index and the per-company sales join in the code).
ALTER TABLE customers
    ADD INDEX idx_customers_company_key (company_id, (LOWER(TRIM(company_name))));

-- Onboarding "added a cutout": the check parsed the drawing JSON of every
-- quote revision of the company (156 MB, 2 s from disk). The flag is stored
-- once per revision, and the index answers the check by itself (23 ms).
-- Adding a stored column rebuilds the table: writes to quote revisions wait
-- for it (about 10 s).
ALTER TABLE quote_revisions
    ADD COLUMN has_cutout TINYINT(1)
        GENERATED ALWAYS AS (JSON_EXTRACT(drawing_json, '$.counters[*].runs[*].cutouts[0]') IS NOT NULL) STORED,
    ADD INDEX idx_quote_revisions_cutout (quote_id, has_cutout, created_by, created_at);

-- Text inbox threads: the thread index lacked the two columns the line and
-- rep filters read, so every message of the listed threads was read from the
-- table (CloudTalk 300 ms -> 23 ms, RingCentral 37 ms -> 22 ms).
ALTER TABLE cloudtalk_sms
    DROP INDEX idx_sms_thread,
    ADD INDEX idx_sms_thread
        (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent,
         recipient10, sender_user_id);

ALTER TABLE ringcentral_sms
    DROP INDEX idx_sms_thread,
    ADD INDEX idx_sms_thread
        (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent,
         recipient10, sender_user_id);

ALTER TABLE telnyx_sms
    DROP INDEX idx_sms_thread,
    ADD INDEX idx_sms_thread
        (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent,
         recipient10, sender_user_id);
