-- Self sign-up (ENG-90): a company nobody at Granite Manager set up creates
-- its own account from the landing page, picks the features it wants and
-- pays for them with Stripe right away. No free month.
--
-- features.monthly_price_cents: what a feature costs a month (USD cents).
--   A sign-up pays the sum of the features it picks, 20% off when it picks
--   every priced one. NULL: no price yet, so sign-up does not offer it.
--   Superusers set it on the Payments screen.
ALTER TABLE features
    ADD COLUMN monthly_price_cents INT NULL;

-- company.source: where the company came from. `superuser`: a superuser
--   created it (every company so far). `signup_page`: it signed itself up;
--   the API refuses everything but sign-in and billing (403) until Stripe
--   shows its subscription paid (company_billing, from Stripe's signed
--   webhooks or Stripe's API), whatever billing_required says.
-- company.signed_up_at: when it signed itself up; NULL otherwise.
-- company.requested_subdomain: the web address it asked for
--   (`acme` of acme.granite-manager.com). A superuser sets company.subdomain
--   once DNS and Caddy serve it; until then the company uses the main host.
-- company.signup_ip_hash: SHA-256 of the address it signed up from, for the
--   sign-up rate limit.
ALTER TABLE company
    ADD COLUMN source VARCHAR(32) NOT NULL DEFAULT 'superuser',
    ADD COLUMN signed_up_at DATETIME NULL,
    ADD COLUMN requested_subdomain VARCHAR(63) NULL,
    ADD COLUMN signup_ip_hash CHAR(64) NULL,
    ADD KEY idx_company_signup_ip (signup_ip_hash, signed_up_at);

-- The features a signed-up company picked and pays for, at the price it was
-- quoted. Bookkeeping, like company_features (marked paid by a superuser):
-- access stays with the feature switches.
CREATE TABLE company_feature_choices (
    company_id          INT      NOT NULL,
    feature_id          INT      NOT NULL,
    monthly_price_cents INT      NULL,
    created_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (company_id, feature_id),
    KEY idx_company_feature_choices_feature (feature_id),
    CONSTRAINT fk_company_feature_choices_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_company_feature_choices_feature
        FOREIGN KEY (feature_id) REFERENCES features(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Items of the dashboard's "Finish setting up" checklist a manager ticked by
-- hand (done, or not needed). Items the API sees done need no row.
CREATE TABLE company_setup_ticks (
    company_id        INT         NOT NULL,
    item_key          VARCHAR(64) NOT NULL,
    ticked_by_user_id INT         NULL,
    ticked_at         DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (company_id, item_key),
    CONSTRAINT fk_company_setup_ticks_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
