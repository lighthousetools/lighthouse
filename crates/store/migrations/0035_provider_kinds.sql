-- A layer can change the models of a provider of a lower layer. It keeps that
-- provider's kind, which for one of the instance may also be `mock` or `demo`.
-- Only `anthropic` and `openai` can be chosen for a new provider.
ALTER TABLE settings_providers DROP CONSTRAINT settings_providers_kind_check;
ALTER TABLE settings_providers ADD CONSTRAINT settings_providers_kind_check
    CHECK (kind IN ('anthropic', 'openai', 'mock', 'demo'));
