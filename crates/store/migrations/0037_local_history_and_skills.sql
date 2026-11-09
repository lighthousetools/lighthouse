-- Claude Code sessions a desktop agent found on its machine, to resume them from the apps.
CREATE TABLE session_history (
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    session_id text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    project text NOT NULL,
    cwd text,
    branch text,
    first_prompt text,
    messages integer NOT NULL DEFAULT 0,
    last_active_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, session_id)
);

CREATE INDEX session_history_recent ON session_history (tenant_id, last_active_at DESC);

-- Skills of Claude Code on a machine: the user's own and those of its projects.
CREATE TABLE local_skills (
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    path text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    description text,
    scope text NOT NULL CHECK (scope IN ('user', 'project')),
    project text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, path)
);
