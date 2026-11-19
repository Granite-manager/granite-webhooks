-- Customer payments (ENG-61), second part: partial refunds, the fee's
-- percentage on Stripe's page, Stripe events applied once, and the payment a
-- bell row is about.

-- `customer_fee_bps`: the customer's share of the fee when the link was made
-- (50 = 0.5%), named on Stripe's page ("Processing fee (0.5%)").
-- `refunded_cents`: what was given back of what the customer was charged, in
-- US cents; a partial refund leaves the payment `paid`.
ALTER TABLE customer_payments
    ADD COLUMN customer_fee_bps INT NOT NULL DEFAULT 0 AFTER customer_fee_cents,
    ADD COLUMN refunded_cents INT NOT NULL DEFAULT 0 AFTER application_fee_cents;

-- Stripe events whose effect no status change guards (a declined card leaves
-- the link payable): each is applied once, however often Stripe sends it.
CREATE TABLE customer_payment_events (
    event_id VARCHAR(255) NOT NULL PRIMARY KEY,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The payment a `payment_*` bell row is about: the bell opens its project.
ALTER TABLE notifications ADD COLUMN customer_payment_id BIGINT UNSIGNED NULL;
