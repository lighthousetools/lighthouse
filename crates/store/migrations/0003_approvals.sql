CREATE TABLE approvals (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    run_id uuid NOT NULL REFERENCES runs (id),
    agent_id text NOT NULL,
    tool text NOT NULL,
    input jsonb NOT NULL,
    quotes_external boolean NOT NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    decided_at timestamptz
);

CREATE INDEX approvals_tenant_status ON approvals (tenant_id, status, created_at DESC);
CREATE INDEX approvals_run ON approvals (run_id);

CREATE TABLE run_checkpoints (
    run_id uuid PRIMARY KEY REFERENCES runs (id),
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    agent_id text NOT NULL,
    state jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
