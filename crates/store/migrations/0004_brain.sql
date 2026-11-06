CREATE EXTENSION IF NOT EXISTS vector;

-- Files on disk are the truth. These tables are a cache that can be rebuilt.
CREATE TABLE brain_entries (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    path text NOT NULL,
    kind text NOT NULL,
    title text NOT NULL,
    scope text NOT NULL,
    status text NOT NULL,
    body text NOT NULL,
    hash text NOT NULL,
    links text[] NOT NULL DEFAULT '{}',
    sources jsonb NOT NULL DEFAULT '[]',
    original text,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    UNIQUE (tenant_id, path)
);

CREATE INDEX brain_entries_recent ON brain_entries (tenant_id, updated_at DESC);

-- The column has no fixed dimension, so a change of embedding model needs no
-- migration. Searches compare only rows of the current model.
CREATE TABLE brain_chunks (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    entry_id uuid NOT NULL REFERENCES brain_entries (id) ON DELETE CASCADE,
    seq integer NOT NULL,
    text text NOT NULL,
    embedding vector,
    embedding_model text
);

CREATE INDEX brain_chunks_entry ON brain_chunks (entry_id);

-- A deletion leaves only the fact that it happened.
CREATE TABLE brain_deletions (
    entry_id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    deleted_at timestamptz NOT NULL DEFAULT now()
);
