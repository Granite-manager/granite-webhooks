-- The crew's job page sorts its photos: Before photos and Installed photos.
-- event_images.stage: 'before' or 'installed' for those; NULL for every
-- other file posted in an event's comments. Additive only.
SET SESSION lock_wait_timeout = 3;

ALTER TABLE event_images ADD COLUMN stage VARCHAR(16) NULL;
