-- Two-step sign-in (authenticator app codes) and the steps a sign-in may
-- need before it gets a session: a code, a new password (the account still
-- has the password 1111), setting up the authenticator (superusers).

-- A user's authenticator app. `secret` is the enabled one, `pending_secret`
-- the one being set up (shown as a QR code until a code from it confirms
-- it). Both are encrypted (AES-256-GCM). `last_step` is the 30-second step
-- of the last code used, so a code works once.
CREATE TABLE IF NOT EXISTS user_mfa (
  user_id            INT          NOT NULL PRIMARY KEY,
  secret             VARCHAR(255) NULL,
  enabled_at         DATETIME     NULL,
  pending_secret     VARCHAR(255) NULL,
  pending_created_at DATETIME     NULL,
  last_step          BIGINT       NULL,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- A sign-in whose password was right but that needs more steps. Only the
-- SHA-256 of its token is stored; it works for 10 minutes. A phone sign-in
-- keeps the phone's details for the device session it ends in.
CREATE TABLE IF NOT EXISTS login_challenges (
  id               BIGINT       AUTO_INCREMENT PRIMARY KEY,
  token_hash       CHAR(64)     NOT NULL,
  user_id          INT          NOT NULL,
  email_hash       CHAR(64)     NOT NULL,
  ip_hash          CHAR(64)     NOT NULL,
  mfa_passed       BOOLEAN      NOT NULL DEFAULT FALSE,
  password_change  BOOLEAN      NOT NULL DEFAULT FALSE,
  failed_codes     INT          NOT NULL DEFAULT 0,
  device_name      VARCHAR(100) NULL,
  platform         VARCHAR(16)  NULL,
  app_version      VARCHAR(32)  NULL,
  expires_at       DATETIME     NOT NULL,
  created_at       DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_login_challenges_token (token_hash),
  KEY idx_login_challenges_user (user_id),
  KEY idx_login_challenges_expires (expires_at),
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
