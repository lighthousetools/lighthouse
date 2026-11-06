ALTER TABLE approvals ADD COLUMN reason text;
ALTER TABLE approvals ADD COLUMN consulted text[] NOT NULL DEFAULT '{}';
