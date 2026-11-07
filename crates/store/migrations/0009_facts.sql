CREATE TABLE facts (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    text text NOT NULL,
    category text,
    status text NOT NULL,
    paused boolean NOT NULL DEFAULT false,
    confidence double precision NOT NULL,
    source_kind text NOT NULL,
    source_ref text,
    source_label text,
    agent_id text,
    run_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    confirmed_at timestamptz
);

CREATE INDEX facts_tenant ON facts (tenant_id, status, updated_at DESC);
