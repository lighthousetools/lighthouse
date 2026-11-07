-- Structured results of briefing runs, next to the Markdown report of the run.
CREATE TABLE run_documents (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    run_id uuid NOT NULL UNIQUE REFERENCES runs (id),
    agent_id text NOT NULL,
    kind text NOT NULL,
    period text NOT NULL,
    title text NOT NULL,
    content jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX run_documents_kind ON run_documents (tenant_id, kind, period DESC, created_at DESC);
