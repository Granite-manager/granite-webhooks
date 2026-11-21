-- Customer payments (ENG-61), fourth part: lost disputes, and asking Stripe
-- about a company's account less often.

-- `lost`: the customer's bank decided a dispute for the customer and took
-- the money back. Final; nothing of the payment counts toward the job any
-- more. (`disputed` is a dispute the bank has not decided yet: the company
-- still holds the money, so it counts as paid until then.)
ALTER TABLE customer_payments
  MODIFY COLUMN status
    ENUM('created', 'canceled', 'pending', 'paid', 'failed', 'refunded', 'disputed', 'lost')
    NOT NULL DEFAULT 'created';

-- When Stripe was last asked whether it approved the company's account, so
-- it is asked at most once a minute per company while the account is being
-- verified. NULL: ask on the next look at the settings.
ALTER TABLE company_payment_settings
  ADD COLUMN stripe_checked_at DATETIME NULL;
