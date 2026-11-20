-- Customer payments (ENG-61): a company's customers pay for their jobs by card
-- or ACH through the company's own Stripe account (Stripe Connect).

-- One row per company that opened Admin -> Company -> Payments: its connected
-- Stripe account, what Stripe last said about it (kept so pages need not ask
-- Stripe), and how much of each fee the company passes on to its customers.
CREATE TABLE company_payment_settings (
    company_id INT NOT NULL PRIMARY KEY,
    stripe_account_id VARCHAR(255) NULL,
    charges_enabled TINYINT(1) NOT NULL DEFAULT 0,
    bank_name VARCHAR(255) NULL,
    bank_last4 VARCHAR(8) NULL,
    card_customer_share_bps INT NOT NULL DEFAULT 0,
    ach_customer_share_bps INT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_company_payment_account (stripe_account_id)
);

-- A payment link and what became of it. `public_id` is the unguessable id in
-- the link (`/pay/<public_id>`). A payment is for one job of the customer: a
-- deal, or a sale that is not part of a deal. Money is in US cents; the fee
-- split is the one in effect when the link was made. Times are UTC.
CREATE TABLE customer_payments (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    public_id VARCHAR(40) NOT NULL,
    company_id INT NOT NULL,
    customer_id INT NOT NULL,
    deal_id BIGINT UNSIGNED NULL,
    sale_id INT NULL,
    method ENUM('card', 'ach') NOT NULL,
    status ENUM('created', 'canceled', 'pending', 'paid', 'failed', 'refunded', 'disputed')
        NOT NULL DEFAULT 'created',
    amount_cents INT NOT NULL,
    fee_cents INT NOT NULL,
    customer_fee_cents INT NOT NULL,
    company_fee_cents INT NOT NULL,
    application_fee_cents INT NOT NULL,
    stripe_account_id VARCHAR(255) NOT NULL,
    stripe_session_id VARCHAR(255) NULL,
    stripe_payment_intent_id VARCHAR(255) NULL,
    failure_reason VARCHAR(255) NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL,
    paid_at DATETIME NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_customer_payments_public (public_id),
    KEY idx_customer_payments_customer (company_id, customer_id, created_at),
    KEY idx_customer_payments_intent (stripe_payment_intent_id)
);
