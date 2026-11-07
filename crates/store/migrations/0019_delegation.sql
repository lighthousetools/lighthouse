-- Tasks handed to an agent, with the result of its run.
ALTER TABLE tasks ADD COLUMN delegated_agent text;
ALTER TABLE tasks ADD COLUMN delegated_run uuid;
ALTER TABLE tasks ADD COLUMN result text;
ALTER TABLE tasks ADD COLUMN result_status text;
CREATE INDEX tasks_delegated ON tasks (delegated_run) WHERE delegated_run IS NOT NULL;

-- Snoozed and cleared items of "Ready for you": a time hides them until then.
ALTER TABLE ready_dismissals ADD COLUMN until timestamptz;
