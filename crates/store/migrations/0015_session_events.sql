-- What Claude Code sessions did, as their hooks reported it.
CREATE TABLE session_events (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    session_id text NOT NULL,
    kind text NOT NULL,
    tool text,
    summary text,
    file_path text,
    detail jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX session_events_session ON session_events (node_id, session_id, created_at);

-- Messages the owner sends to a session. The desktop agent hands them over
-- when the session stops and waits.
CREATE TABLE session_messages (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    session_id text NOT NULL,
    text text NOT NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz
);

CREATE INDEX session_messages_pending ON session_messages (node_id, session_id, status, created_at);
