CREATE TABLE nodes (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    paused boolean NOT NULL DEFAULT false,
    last_seen_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    project text NOT NULL,
    branch text,
    status text NOT NULL,
    summary text,
    cost_eur double precision NOT NULL DEFAULT 0,
    started_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

CREATE INDEX sessions_recent ON sessions (tenant_id, updated_at DESC);

CREATE TABLE permission_requests (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    session_id text NOT NULL,
    project text NOT NULL,
    tool text NOT NULL,
    input jsonb NOT NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    decided_at timestamptz
);

CREATE INDEX permission_requests_open ON permission_requests (tenant_id, status, created_at DESC);
