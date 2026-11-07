-- Every refresh attempt of a source, for charts of alerts and freshness.
CREATE TABLE sync_history (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    source text NOT NULL,
    ok boolean NOT NULL,
    items integer NOT NULL DEFAULT 0,
    error text,
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX sync_history_source ON sync_history (tenant_id, source, at DESC);
