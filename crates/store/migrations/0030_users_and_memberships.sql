-- People sign in as users. A user belongs to one or more tenants through a
-- membership with a role. The single owner of earlier versions becomes the
-- first user when the server starts; see `Accounts::bootstrap_owner`.
CREATE TABLE users (
    id uuid PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL,
    password_hash text,
    email_verified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX users_email ON users (lower(email));

CREATE TABLE memberships (
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, user_id)
);

CREATE INDEX memberships_user ON memberships (user_id);

CREATE TABLE invitations (
    id uuid PRIMARY KEY,
    tenant_id uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    email text NOT NULL,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    token_hash bytea NOT NULL UNIQUE,
    invited_by uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    accepted_at timestamptz,
    revoked_at timestamptz
);

CREATE INDEX invitations_tenant ON invitations (tenant_id, created_at DESC);

-- One-time tokens of a user, such as email verification.
CREATE TABLE user_tokens (
    token_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    purpose text NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at timestamptz
);

-- Device tokens belong to a user and are bound to one tenant.
ALTER TABLE device_tokens ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
CREATE INDEX device_tokens_user ON device_tokens (user_id, created_at DESC);

-- Personal data within a tenant carries the user it belongs to.
ALTER TABLE nodes ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
ALTER TABLE talk_threads ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
ALTER TABLE synced_items ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
-- Runs keep the user they ran for even after that user is deleted, so they
-- never become visible to the rest of the tenant.
ALTER TABLE runs ADD COLUMN user_id uuid;
CREATE INDEX runs_user ON runs (tenant_id, user_id, started_at DESC);

-- The person agents work for in a tenant: its earliest owner.
CREATE FUNCTION tenant_primary_user(tenant uuid) RETURNS uuid AS $$
    SELECT user_id FROM memberships
    WHERE tenant_id = tenant AND role = 'owner'
    ORDER BY created_at, user_id
    LIMIT 1
$$ LANGUAGE sql STABLE;

-- Run steps stay append-only, except when a whole tenant is deleted. The
-- deletion names the tenant in `lighthouse.purging_tenant` for its transaction.
CREATE OR REPLACE FUNCTION reject_audit_change() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE'
       AND current_setting('lighthouse.purging_tenant', true) = OLD.tenant_id::text THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'run_steps is append-only';
END;
$$ LANGUAGE plpgsql;
