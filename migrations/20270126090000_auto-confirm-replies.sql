-- Auto-confirm: the text the customer gets back when AI reads their reply to
-- an auto-confirm text as a YES (the job turned Confirmed) or a NO. NULL sends
-- nothing (Admin > Schedule > Customer notifications > Auto-confirm).
ALTER TABLE company
    ADD COLUMN schedule_yes_reply TEXT NULL,
    ADD COLUMN schedule_no_reply TEXT NULL;
