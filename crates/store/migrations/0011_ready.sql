-- Items the owner cleared from "Ready for you" without another decision,
-- such as a draft an agent saved in an external tool.
CREATE TABLE ready_dismissals (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    kind text NOT NULL,
    item_id text NOT NULL,
    dismissed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, kind, item_id)
);

CREATE INDEX run_steps_recent_tools ON run_steps (tenant_id, kind, created_at DESC);
