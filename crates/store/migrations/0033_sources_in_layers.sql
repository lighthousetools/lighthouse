-- Connected sources added in the apps. A row without `user_id` is shared in the
-- tenant; a row with `user_id` is that member's, and its items belong to them.
CREATE TABLE settings_sources (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    name text NOT NULL,
    definition jsonb NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_sources_name ON settings_sources
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), name);
