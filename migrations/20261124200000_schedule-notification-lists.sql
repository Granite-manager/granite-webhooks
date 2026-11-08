-- Admin > Schedule > Customer notifications.
--
-- Customer messages: each company makes its own lists (e.g. "Reminders")
-- that group its reminders and scheduling texts. A template is in one list
-- or none (`list_id`). Every company that has reminders or scheduling texts
-- gets the two lists it saw until now, "Reminders" and "Scheduling texts",
-- with those templates in them, so nothing moves out of sight.
--
-- Installer messages / Technician messages: the template technician gets
-- their own On my way message. It starts as a copy of the company's current
-- one, so technicians keep sending exactly what they send today.

CREATE TABLE schedule_notification_lists (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(100) NOT NULL,
    position INT NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    INDEX idx_schedule_notification_lists_company (company_id, deleted_at),
    CONSTRAINT fk_schedule_notification_lists_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);

ALTER TABLE schedule_notification_templates
    ADD COLUMN list_id INT NULL DEFAULT NULL,
    ADD CONSTRAINT fk_schedule_notification_templates_list
        FOREIGN KEY (list_id) REFERENCES schedule_notification_lists(id) ON DELETE SET NULL;

ALTER TABLE company
    ADD COLUMN on_my_way_tech_message TEXT NULL,
    ADD COLUMN on_my_way_tech_subject VARCHAR(255) NULL;

UPDATE company
   SET on_my_way_tech_message = on_my_way_message,
       on_my_way_tech_subject = on_my_way_subject
 WHERE on_my_way_message IS NOT NULL OR on_my_way_subject IS NOT NULL;

INSERT INTO schedule_notification_lists (company_id, name, position)
SELECT DISTINCT company_id, 'Reminders', 1
  FROM schedule_notification_templates
 WHERE deleted_at IS NULL AND kind = 'reminder';

INSERT INTO schedule_notification_lists (company_id, name, position)
SELECT DISTINCT company_id, 'Scheduling texts', 2
  FROM schedule_notification_templates
 WHERE deleted_at IS NULL AND kind = 'confirmation';

UPDATE schedule_notification_templates t
  JOIN schedule_notification_lists l
    ON l.company_id = t.company_id
   AND l.deleted_at IS NULL
   AND l.name = IF(t.kind = 'reminder', 'Reminders', 'Scheduling texts')
   SET t.list_id = l.id
 WHERE t.deleted_at IS NULL
   AND t.kind IN ('reminder', 'confirmation')
   AND t.list_id IS NULL;
