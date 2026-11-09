-- Claude Code sessions the server asks a desktop agent to start or resume.
CREATE TABLE session_launches (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    cwd text NOT NULL,
    prompt text NOT NULL,
    resume text,
    model text,
    status text NOT NULL CHECK (status IN ('queued', 'started', 'running', 'succeeded', 'failed', 'cancelled')),
    cancel_requested boolean NOT NULL DEFAULT false,
    session_id text,
    result text,
    cost_usd double precision,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

CREATE INDEX session_launches_queue ON session_launches (node_id, status, created_at);
CREATE INDEX session_launches_session ON session_launches (node_id, session_id);
