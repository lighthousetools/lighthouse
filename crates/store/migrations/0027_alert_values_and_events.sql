-- The current value of an alert's metric, and the timeline of what happened.
ALTER TABLE alerts ADD COLUMN value double precision, ADD COLUMN unit text;

CREATE TABLE alert_events (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL,
    alert_id uuid NOT NULL REFERENCES alerts (id) ON DELETE CASCADE,
    kind text NOT NULL,
    text text,
    value double precision,
    at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX alert_events_alert ON alert_events (tenant_id, alert_id, at);
