-- A company can link a number to a position (one number per position, per
-- company): everyone with that position gets the number. via_position marks
-- the access that came from the position, apart from a user assigned by hand.
ALTER TABLE telnyx_numbers
  ADD COLUMN position_id INT NULL,
  ADD UNIQUE KEY uq_telnyx_numbers_company_position (company_id, position_id);

ALTER TABLE telnyx_number_users
  ADD COLUMN via_position TINYINT(1) NOT NULL DEFAULT 0;
