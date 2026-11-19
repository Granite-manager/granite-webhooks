-- Self sign-up, flat price (ENG-90 follow-up): a company from the sign-up
-- page pays a flat monthly price (company.subscription_price_cents, $800 at
-- sign-up), or reaches out and a superuser gives it time free. Sign-up no
-- longer picks features: features.monthly_price_cents and
-- company_feature_choices (20270209090000) stay for later, unused.
--
-- company.free_access_until: a superuser let this sign-up company in without
-- paying until then (a free month with a demo). The paywall and the billing
-- gate let it in while the time is in the future; afterwards it must pay.
-- NULL: no free time. Only superusers set it.
ALTER TABLE company
    ADD COLUMN free_access_until DATETIME NULL;
