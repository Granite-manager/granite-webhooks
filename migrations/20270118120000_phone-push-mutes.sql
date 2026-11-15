-- The phone pushes a user turned off in the app (Settings > Notifications).
-- A row means "off"; no row means on, so everyone keeps every push until
-- they turn one off. `push_key` is `crate::phone::push_mutes::PushSetting`.
CREATE TABLE phone_push_mutes (
  user_id INT NOT NULL,
  push_key VARCHAR(40) NOT NULL,
  muted_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, push_key)
);
