-- Users who signed up themselves have to confirm their email address before
-- they sign in, if the instance requires it (`tenancy.require_verified_email`).
-- The configured owner and invited people are not affected.
ALTER TABLE users ADD COLUMN verification_required boolean NOT NULL DEFAULT false;
