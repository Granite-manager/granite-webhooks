-- Every company gets its own copy of the shared starter pipeline (groups_list
-- id 1) and may then change, hide or delete it. Deals, their stage history,
-- templates, scheduled messages and stage rules move to the company's copy;
-- the shared pipeline is then retired.

SET @last_group = (SELECT COALESCE(MAX(id), 0) FROM groups_list);

-- The copy is the company's default while it has no default of its own, and
-- stays hidden where the company hid the shared one.
INSERT INTO groups_list (name, created_at, company_id, is_displayed, is_default)
SELECT shared.name, NOW(), c.id,
       c.shared_pipeline_hidden = 0,
       NOT EXISTS (SELECT 1 FROM groups_list own
                    WHERE own.company_id = c.id AND own.deleted_at IS NULL AND own.is_default = 1)
  FROM company c
  JOIN groups_list shared ON shared.id = 1 AND shared.company_id IS NULL AND shared.deleted_at IS NULL
 ORDER BY c.id;

-- Every stage, the retired ones too, so old deals keep a matching stage.
INSERT INTO deals_list (name, position, created_at, deleted_at, group_id)
SELECT dl.name, dl.position, dl.created_at, dl.deleted_at, copy.id
  FROM groups_list copy
  JOIN deals_list dl ON dl.group_id = 1
 WHERE copy.id > @last_group
 ORDER BY copy.id, dl.position, dl.id;

CREATE TEMPORARY TABLE starter_map (
  company_id INT NOT NULL,
  old_list INT NOT NULL,
  new_list INT NOT NULL,
  new_group INT NOT NULL,
  PRIMARY KEY (company_id, old_list)
);

INSERT INTO starter_map (company_id, old_list, new_list, new_group)
SELECT copy.company_id, old.id, nl.id, copy.id
  FROM groups_list copy
  JOIN deals_list nl ON nl.group_id = copy.id
  JOIN deals_list old ON old.group_id = 1 AND old.name = nl.name AND old.position = nl.position
 WHERE copy.id > @last_group;

UPDATE deals d
  JOIN customers c ON c.id = d.customer_id
  JOIN starter_map m ON m.company_id = c.company_id AND m.old_list = d.list_id
   SET d.list_id = m.new_list;

UPDATE deal_stage_history h
  JOIN deals d ON d.id = h.deal_id
  JOIN customers c ON c.id = d.customer_id
  JOIN starter_map m ON m.company_id = c.company_id AND m.old_list = h.list_id
   SET h.list_id = m.new_list;

UPDATE cloudtalk_sms_templates t
  JOIN starter_map m ON m.company_id = t.company_id AND m.old_list = t.lead_list_id
   SET t.lead_list_id = m.new_list;

UPDATE email_templates t
  JOIN starter_map m ON m.company_id = t.company_id AND m.old_list = t.lead_list_id
   SET t.lead_list_id = m.new_list;

UPDATE email_templates t
  JOIN (SELECT DISTINCT company_id, new_group FROM starter_map) m ON m.company_id = t.company_id
   SET t.lead_group_id = m.new_group
 WHERE t.lead_group_id = 1;

UPDATE deal_stage_activity_rules r
  JOIN starter_map m ON m.company_id = r.company_id AND m.old_list = r.from_list_id
   SET r.from_list_id = m.new_list;

UPDATE deal_stage_activity_rules r
  JOIN starter_map m ON m.company_id = r.company_id AND m.old_list = r.to_list_id
   SET r.to_list_id = m.new_list;

UPDATE scheduled_emails s
  JOIN starter_map m ON m.company_id = s.company_id AND m.old_list = s.list_id
   SET s.list_id = m.new_list;

UPDATE scheduled_sms s
  JOIN starter_map m ON m.company_id = s.company_id AND m.old_list = s.list_id
   SET s.list_id = m.new_list;

DROP TEMPORARY TABLE starter_map;

-- Retired: no company sees it any more. Its rows stay for old references.
UPDATE groups_list SET deleted_at = NOW(), is_default = 0 WHERE id = 1 AND company_id IS NULL;
UPDATE deals_list SET deleted_at = COALESCE(deleted_at, NOW()) WHERE group_id = 1;

ALTER TABLE company DROP COLUMN shared_pipeline_hidden;
