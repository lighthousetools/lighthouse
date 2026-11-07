-- The companies the owner runs. Connected items belong to the organization
-- whose sources include their source.
CREATE TABLE organizations (
    id text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    color text,
    sources text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, id)
);
