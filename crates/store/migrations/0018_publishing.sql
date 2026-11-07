-- Posts the owner writes, plans and publishes, and the archive of published ones.
CREATE TABLE posts (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    org text,
    channel text NOT NULL DEFAULT 'linkedin',
    kind text,
    pillar text,
    topic text,
    title text,
    body text NOT NULL DEFAULT '',
    status text NOT NULL,
    scheduled_at timestamptz,
    published_at timestamptz,
    url text,
    external_id text,
    images uuid[] NOT NULL DEFAULT '{}',
    metrics jsonb NOT NULL DEFAULT '{}',
    origin text NOT NULL,
    publish_run uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX posts_external ON posts (tenant_id, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX posts_plan ON posts (tenant_id, status, scheduled_at);

-- Weekly slots of the posting plan, in the owner's time zone.
CREATE TABLE post_slots (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    weekday smallint NOT NULL,
    time text NOT NULL,
    pillar text,
    org text
);

CREATE TABLE post_pillars (
    id text NOT NULL,
    tenant_id uuid NOT NULL REFERENCES tenants (id),
    name text NOT NULL,
    description text,
    color text,
    topics text[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (tenant_id, id)
);

-- How the owner presents themselves: profile, voice and topics.
CREATE TABLE brand_profile (
    tenant_id uuid PRIMARY KEY REFERENCES tenants (id),
    headline text,
    bio text,
    audience text,
    voice text,
    topics text[] NOT NULL DEFAULT '{}',
    avoid text[] NOT NULL DEFAULT '{}',
    updated_at timestamptz NOT NULL DEFAULT now()
);
