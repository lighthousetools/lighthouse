CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    title text NOT NULL,
    notes text,
    due_on date,
    priority smallint NOT NULL DEFAULT 0,
    status text NOT NULL,
    position double precision NOT NULL,
    source_kind text,
    source_ref text,
    source_label text,
    source_url text,
    origin text NOT NULL,
    agent_id text,
    run_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    done_at timestamptz
);

CREATE INDEX tasks_open ON tasks (tenant_id, status, position);
