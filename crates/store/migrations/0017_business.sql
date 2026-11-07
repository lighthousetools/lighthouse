-- Amounts of deals, balances, transactions, invoices and holdings.
ALTER TABLE synced_items ADD COLUMN amount double precision;
ALTER TABLE synced_items ADD COLUMN currency text;
CREATE INDEX synced_items_kind ON synced_items (tenant_id, kind, source);
