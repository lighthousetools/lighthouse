-- Settings in three layers: the instance (lighthouse.toml), the tenant and the
-- person. A row without `user_id` belongs to the tenant layer and applies to
-- every member; a row with `user_id` is a personal setting of that member in
-- that tenant. Keys and tokens stay in `credentials`, never here.

-- Personal vault entries: a credential with a user belongs to that person only.
ALTER TABLE credentials ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
ALTER TABLE credentials DROP CONSTRAINT credentials_pkey;
CREATE UNIQUE INDEX credentials_entry ON credentials
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), name);

CREATE TABLE settings_providers (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('anthropic', 'openai')),
    base_url text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_providers_name ON settings_providers
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), name);

CREATE TABLE settings_models (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    provider_id uuid NOT NULL REFERENCES settings_providers (id) ON DELETE CASCADE,
    task text NOT NULL,
    model text NOT NULL,
    input_eur_per_mtok double precision NOT NULL CHECK (input_eur_per_mtok >= 0),
    output_eur_per_mtok double precision NOT NULL CHECK (output_eur_per_mtok >= 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_id, task)
);

CREATE TABLE settings_routes (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    task text NOT NULL,
    provider text NOT NULL,
    fallback text,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_routes_task ON settings_routes
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), task);

-- MCP connectors added in the apps. Only streamable HTTP: a tenant or a person
-- never starts a process on the server.
CREATE TABLE settings_connectors (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    name text NOT NULL,
    url text NOT NULL,
    auth text NOT NULL CHECK (auth IN ('none', 'bearer', 'oauth')),
    client_id text,
    scopes text[] NOT NULL DEFAULT '{}',
    read_tools text[] NOT NULL DEFAULT '{}',
    draft_tools text[] NOT NULL DEFAULT '{}',
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_connectors_name ON settings_connectors
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), name);

-- Entries of a lower layer that a tenant or a person turned off for itself,
-- such as a connector of the instance.
CREATE TABLE settings_switches (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('connector', 'agent', 'automation', 'source')),
    name text NOT NULL,
    enabled boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_switches_entry ON settings_switches
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), kind, name);

-- Single settings such as notifications, the Talk agent or the time zone, as
-- JSON by key.
CREATE TABLE settings_values (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    key text NOT NULL,
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_values_key ON settings_values
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), key);

-- Who changed which setting when. Values are not recorded, so no secret and no
-- URL token lands here.
CREATE TABLE settings_changes (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid,
    actor uuid,
    area text NOT NULL,
    name text NOT NULL,
    action text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX settings_changes_tenant ON settings_changes (tenant_id, created_at DESC);

-- A counter per tenant that every change raises, so each server process sees
-- changes made through another one.
CREATE TABLE settings_revisions (
    tenant_id uuid PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    revision bigint NOT NULL DEFAULT 0
);
