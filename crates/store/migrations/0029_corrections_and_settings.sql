-- Whether the owner changed a proposal before deciding, for an agent's corrections.
ALTER TABLE approvals ADD COLUMN edited boolean NOT NULL DEFAULT false;

-- Settings the owner changes in the apps, such as the monthly budget.
CREATE TABLE owner_settings (
    tenant_id uuid NOT NULL,
    key text NOT NULL,
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, key)
);
