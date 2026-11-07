-- Proposals of Lighthouse itself carry their origin instead of a placeholder run.
UPDATE tasks SET origin = agent_id, agent_id = NULL, run_id = NULL
WHERE origin = 'agent' AND agent_id IN ('pipeline', 'holdings')
  AND run_id = '00000000-0000-0000-0000-000000000000';
