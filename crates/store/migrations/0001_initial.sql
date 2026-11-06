CREATE TABLE tenants (
    id uuid PRIMARY KEY,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO tenants (id, name) VALUES ('00000000-0000-0000-0000-000000000000', 'default');

CREATE TABLE credentials (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    nonce bytea NOT NULL,
    ciphertext bytea NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name)
);

CREATE TABLE runs (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    agent_id text NOT NULL,
    trigger text NOT NULL,
    status text NOT NULL,
    reason text,
    report text,
    input_tokens bigint NOT NULL DEFAULT 0,
    output_tokens bigint NOT NULL DEFAULT 0,
    cost_eur double precision NOT NULL DEFAULT 0,
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

CREATE INDEX runs_tenant_started ON runs (tenant_id, started_at DESC);

CREATE TABLE run_steps (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    run_id uuid NOT NULL REFERENCES runs (id),
    seq integer NOT NULL,
    kind text NOT NULL,
    name text NOT NULL,
    status text NOT NULL,
    detail text,
    input_tokens bigint NOT NULL DEFAULT 0,
    output_tokens bigint NOT NULL DEFAULT 0,
    cost_eur double precision NOT NULL DEFAULT 0,
    duration_ms bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (run_id, seq)
);

CREATE FUNCTION reject_audit_change() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'run_steps is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER run_steps_append_only
    BEFORE UPDATE OR DELETE ON run_steps
    FOR EACH ROW EXECUTE FUNCTION reject_audit_change();

CREATE TABLE agent_schedules (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    agent_id text NOT NULL,
    last_fire_at timestamptz NOT NULL,
    PRIMARY KEY (tenant_id, agent_id)
);
