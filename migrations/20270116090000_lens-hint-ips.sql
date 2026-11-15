-- The customer stones page highlights its magnifier (lens) button once per
-- visitor IP address; a row here means that address has already seen it.
CREATE TABLE lens_hint_ips (
    ip VARCHAR(64) NOT NULL PRIMARY KEY,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
