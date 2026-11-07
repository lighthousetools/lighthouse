-- Automations live in files. The apps can switch one on or off without editing it.
CREATE TABLE automation_settings (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    automation_id text NOT NULL,
    enabled boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, automation_id)
);
