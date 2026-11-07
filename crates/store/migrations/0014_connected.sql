-- A cache of mail, calendar, issues and pull requests from connectors. The
-- services stay the truth; every row can be fetched again.
CREATE TABLE synced_items (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    source text NOT NULL,
    domain text NOT NULL,
    kind text NOT NULL,
    external_id text NOT NULL,
    parent_id text,
    title text,
    summary text,
    author text,
    assignee text,
    state text,
    url text,
    flags text[] NOT NULL DEFAULT '{}',
    importance smallint NOT NULL DEFAULT 0,
    occurred_at timestamptz,
    starts_at timestamptz,
    ends_at timestamptz,
    data jsonb NOT NULL DEFAULT '{}',
    raw jsonb,
    synced_at timestamptz NOT NULL DEFAULT now(),
    search tsvector GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(title, '') || ' ' || coalesce(summary, '') || ' ' ||
                              coalesce(author, '') || ' ' || coalesce(assignee, ''))
    ) STORED,
    UNIQUE (tenant_id, source, kind, external_id)
);

CREATE INDEX synced_items_recent ON synced_items (tenant_id, domain, kind, occurred_at DESC);
CREATE INDEX synced_items_parent ON synced_items (tenant_id, source, parent_id);
CREATE INDEX synced_items_starts ON synced_items (tenant_id, starts_at) WHERE starts_at IS NOT NULL;
CREATE INDEX synced_items_flags ON synced_items USING gin (flags);
CREATE INDEX synced_items_search ON synced_items USING gin (search);

CREATE TABLE sync_state (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    source text NOT NULL,
    last_attempt_at timestamptz,
    last_success_at timestamptz,
    last_error text,
    items integer NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, source)
);
