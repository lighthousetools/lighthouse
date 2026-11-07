-- Which full collections returned an item. An item no collection returns anymore is removed.
ALTER TABLE synced_items ADD COLUMN seen_by text[] NOT NULL DEFAULT '{}';
-- Hidden until then ("later"). Kept across refreshes.
ALTER TABLE synced_items ADD COLUMN snoozed_until timestamptz;

-- Saved filters of the connected lists, shown as splits.
CREATE TABLE saved_filters (
    id text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    domain text,
    filter jsonb NOT NULL DEFAULT '{}',
    position integer NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, id)
);
