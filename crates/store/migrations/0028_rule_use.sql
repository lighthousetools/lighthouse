-- How often a rule reached a run.
ALTER TABLE rules ADD COLUMN applied_count integer NOT NULL DEFAULT 0, ADD COLUMN last_applied_at timestamptz;
