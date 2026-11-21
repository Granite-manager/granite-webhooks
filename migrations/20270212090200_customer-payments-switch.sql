-- Customer payments are turned on company by company (Superuser > Companies);
-- off for everyone until then. Links made while it was on stay payable.
ALTER TABLE company
  ADD COLUMN customer_payments_enabled TINYINT(1) NOT NULL DEFAULT 0;

-- When Stripe was last asked about a payment whose webhook may not have come
-- (its Checkout session and payment intent), so it is asked at most once a
-- minute.
ALTER TABLE customer_payments
  ADD COLUMN stripe_checked_at DATETIME NULL;
