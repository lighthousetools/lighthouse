-- Agents, automations and skills created in the apps. A row without `user_id`
-- belongs to the tenant, a row with `user_id` to that member. Built-in files of
-- the instance stay on disk and are read-only.
CREATE TABLE settings_files (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid REFERENCES users (id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('agent', 'automation', 'skill')),
    name text NOT NULL,
    source text NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX settings_files_name ON settings_files
    (tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), kind, name);

ALTER TABLE settings_switches DROP CONSTRAINT settings_switches_kind_check;
ALTER TABLE settings_switches ADD CONSTRAINT settings_switches_kind_check
    CHECK (kind IN ('connector', 'agent', 'automation', 'source', 'skill'));
