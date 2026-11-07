CREATE TABLE alerts (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    kind text NOT NULL,
    key text NOT NULL,
    severity text NOT NULL,
    title text NOT NULL,
    detail text,
    source_kind text,
    source_ref text,
    source_label text,
    source_url text,
    status text NOT NULL,
    count integer NOT NULL DEFAULT 1,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    acknowledged_at timestamptz,
    resolved_at timestamptz
);

-- One unresolved alert per problem. A repeat counts up instead of adding a row.
CREATE UNIQUE INDEX alerts_unresolved_key ON alerts (tenant_id, key) WHERE status <> 'resolved';
CREATE INDEX alerts_recent ON alerts (tenant_id, status, last_seen_at DESC);
