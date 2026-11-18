-- Which companies have paid for which features (ENG-58). Only bookkeeping:
-- a superuser ticks it by hand on the Payments screen, nothing reads it to
-- grant access. Access stays with company.<feature>_enabled and
-- global_feature_flags, so a company may use a feature it has not paid for.
--
-- features.usage_metric names the usage count the API computes for the
-- feature (api/crates/admin/src/routes/paid_features.rs, METRICS); NULL or an
-- unknown name: no count shown. Superusers add and rename features themselves.
CREATE TABLE features (
    id           INT          NOT NULL AUTO_INCREMENT,
    name         VARCHAR(128) NOT NULL,
    usage_metric VARCHAR(64)  NULL,
    created_at   DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_features_name (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- One row per company and feature it has paid for.
CREATE TABLE company_features (
    company_id         INT      NOT NULL,
    feature_id         INT      NOT NULL,
    created_at         DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by_user_id INT      NULL,
    PRIMARY KEY (company_id, feature_id),
    KEY idx_company_features_feature (feature_id),
    CONSTRAINT fk_company_features_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_company_features_feature
        FOREIGN KEY (feature_id) REFERENCES features(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO features (name, usage_metric) VALUES
    ('QuickBooks', 'quickbooks_invoices'),
    ('E-signatures', 'signature_requests'),
    ('Projects', 'projects_created'),
    ('Phone calls', 'phone_calls'),
    ('Texting', 'texts_sent'),
    ('Text flows', 'text_flow_enrollments'),
    ('AI Chat', 'chat_questions'),
    ('AI replies', 'ai_replies'),
    ('AI call notes', 'ai_call_notes'),
    ('Call coaching', 'call_coaching');
