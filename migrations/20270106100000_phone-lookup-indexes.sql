-- Phone lookups by the last ten digits (texts' and calls' customer names, the
-- quote portal, call coaching) compared an expression over every customer or
-- contact of the company: about 45 ms for the customers, 10 to 40 ms for the
-- contacts, several times per text inbox request. Indexes on exactly those
-- expressions let MySQL look the numbers up instead (0.3 ms on a copy of the
-- data). The queries are unchanged: MySQL uses a functional index for the
-- same expression.
ALTER TABLE customers
    ADD INDEX idx_customers_phone10
        (company_id, (RIGHT(REGEXP_REPLACE(COALESCE(phone, ''), '[^0-9]', ''), 10))),
    ADD INDEX idx_customers_phone2_10
        (company_id, (RIGHT(REGEXP_REPLACE(COALESCE(phone_2, ''), '[^0-9]', ''), 10)));

ALTER TABLE cloudtalk_contacts
    ADD INDEX idx_cloudtalk_contacts_phone1_10 (company_id, (RIGHT(phone_e164_1, 10))),
    ADD INDEX idx_cloudtalk_contacts_phone2_10 (company_id, (RIGHT(phone_e164_2, 10)));

ALTER TABLE ringcentral_contacts
    ADD INDEX idx_ringcentral_contacts_phone1_10 (company_id, (RIGHT(phone_e164_1, 10))),
    ADD INDEX idx_ringcentral_contacts_phone2_10 (company_id, (RIGHT(phone_e164_2, 10)));
