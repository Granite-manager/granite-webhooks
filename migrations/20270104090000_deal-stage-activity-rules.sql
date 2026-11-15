-- Admin › Deals › Automatic Activities: when a deal moves from one list to
-- another, add an activity for the deal's sales rep.
CREATE TABLE deal_stage_activity_rules (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  -- The list the deal leaves; NULL: any list.
  from_list_id INT NULL,
  -- The list the deal lands on.
  to_list_id INT NOT NULL,
  activity_name VARCHAR(255) NOT NULL,
  due_amount INT NOT NULL DEFAULT 24,
  due_unit ENUM('hours', 'days') NOT NULL DEFAULT 'hours',
  is_enabled TINYINT(1) NOT NULL DEFAULT 1,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  deleted_at DATETIME NULL,
  INDEX idx_deal_stage_activity_rules_to (company_id, to_list_id)
);

-- The rule that added an activity (NULL: added by hand).
ALTER TABLE deal_activities ADD COLUMN stage_rule_id INT NULL;
