-- The payments page: the day a company's next new subscription is first
-- charged (its card is saved at checkout); NULL charges at checkout. Read
-- when it subscribes; a running subscription keeps its own dates.
ALTER TABLE company
    ADD COLUMN subscription_first_payment_on DATE NULL;
