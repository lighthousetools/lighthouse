CREATE TABLE owner_accounts (
    tenant_id uuid PRIMARY KEY REFERENCES tenants (id),
    password_hash text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE device_tokens (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    kind text NOT NULL,
    token_hash bytea NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at timestamptz
);

CREATE INDEX device_tokens_tenant ON device_tokens (tenant_id, created_at DESC);
