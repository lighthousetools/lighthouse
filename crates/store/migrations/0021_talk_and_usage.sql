-- Conversations with the assistant.
CREATE TABLE talk_threads (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    title text NOT NULL,
    pinned boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE talk_messages (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    thread_id uuid NOT NULL REFERENCES talk_threads (id) ON DELETE CASCADE,
    role text NOT NULL,
    text text NOT NULL,
    status text NOT NULL,
    run_id uuid,
    sources jsonb NOT NULL DEFAULT '[]',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX talk_messages_thread ON talk_messages (thread_id, created_at);
CREATE INDEX talk_messages_run ON talk_messages (run_id) WHERE run_id IS NOT NULL;

-- Costs and tokens of Claude Code sessions, from their OpenTelemetry metrics.
CREATE TABLE session_usage (
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    node_id uuid NOT NULL REFERENCES nodes (id) ON DELETE CASCADE,
    session_id text NOT NULL,
    cost_usd double precision NOT NULL DEFAULT 0,
    input_tokens double precision NOT NULL DEFAULT 0,
    output_tokens double precision NOT NULL DEFAULT 0,
    cache_read_tokens double precision NOT NULL DEFAULT 0,
    cache_creation_tokens double precision NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, session_id)
);
