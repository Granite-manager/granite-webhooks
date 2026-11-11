-- Contact groups are shared or private. A group made by a Manager, a Sales
-- Manager, an admin or a superuser is the company's (shared = 1): every user
-- sees it, and only those people rename, delete or refill it. A group made
-- by anyone else is their own (shared = 0): only they see and change it.
-- Every group made before this stays the company's. Additive only.
ALTER TABLE phone_contact_groups ADD COLUMN shared TINYINT(1) NOT NULL DEFAULT 1;
