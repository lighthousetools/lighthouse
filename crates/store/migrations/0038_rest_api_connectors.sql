-- A connector of the tenant or personal layer can be a REST API with an
-- OpenAPI description: `url` then points to the description, and `api` holds
-- the base URL, how the key is sent and the chosen operations.
ALTER TABLE settings_connectors ADD COLUMN api jsonb;
