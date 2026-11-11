-- A user's own picture (the phone app's Profile: Take photo / Choose from
-- photos): the public URL of a file they uploaded to the `user-photos`
-- folder (`POST /v1/uploads`, then `PUT /v1/me/photo`). NULL: no picture,
-- their initials show. Additive only.
ALTER TABLE users ADD COLUMN photo_url VARCHAR(512) NULL DEFAULT NULL;
