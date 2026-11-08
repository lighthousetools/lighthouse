//! Users, tenants, memberships and device tokens.
//!
//! People sign in as users with an email address and a password. A user
//! belongs to one or more tenants through a membership with a role. Every app,
//! browser and desktop agent holds its own device token, which belongs to one
//! user and is bound to one tenant, so every request resolves to exactly one
//! user and one tenant. Tokens are random, so the database keeps only their
//! SHA-256 hash. Passwords are hashed with Argon2id.
//!
//! Instances of earlier versions had one owner and one password per tenant.
//! [`Accounts::bootstrap_owner`] turns that owner into the first user.

mod invitations;
pub mod limiter;
mod tenants;

pub use invitations::{AcceptAs, Accepted, Invitation, IssuedInvitation};
pub use tenants::{Member, Membership, Tenant};

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Argon2, PasswordHash};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use lighthouse_store::sqlx::{self, Row};
use limiter::RateLimiter;
use rand::Rng;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use utoipa::ToSchema;
use uuid::Uuid;

const TOKEN_PREFIX: &str = "lht_";
const TOKEN_BYTES: usize = 32;
/// Shortest accepted password.
pub const MIN_PASSWORD_LEN: usize = 12;
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
const MAX_SIGNUPS: u32 = 5;
const SIGNUP_WINDOW: Duration = Duration::from_secs(3600);
const VERIFY_EMAIL: &str = "verify_email";
const VERIFY_TTL_HOURS: i64 = 48;

/// Errors from accounts and sign-in.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The store failed, for example while deleting a tenant.
    #[error("store error: {0}")]
    Store(#[from] lighthouse_store::StoreError),
    /// The password is too short.
    #[error("the password needs at least {MIN_PASSWORD_LEN} characters")]
    WeakPassword,
    /// The email address or the password does not match.
    #[error("wrong email or password")]
    WrongPassword,
    /// The instance has several users, so the sign-in needs an email address.
    #[error("the sign-in needs an email address")]
    EmailRequired,
    /// Too many attempts.
    #[error("too many attempts, try again later")]
    LockedOut,
    /// Hashing failed.
    #[error("cannot hash the password")]
    Hash,
    /// The email address is not valid.
    #[error("the email address is not valid")]
    InvalidEmail,
    /// A field is empty or too long.
    #[error("{0}")]
    Invalid(String),
    /// Another user has this email address.
    #[error("this email address is taken")]
    EmailTaken,
    /// The user is not a member of the tenant.
    #[error("not a member of this tenant")]
    NotMember,
    /// The change would leave a tenant without an owner.
    #[error("a tenant needs at least one owner")]
    LastOwner,
    /// A token or invitation is unknown, used or expired.
    #[error("the link is invalid or expired")]
    InvalidToken,
    /// Something does not exist.
    #[error("unknown {0}")]
    NotFound(&'static str),
    /// The default tenant of the instance cannot be deleted.
    #[error("the default tenant cannot be deleted")]
    DefaultTenant,
    /// The person is a member of the tenant already.
    #[error("this person is a member already")]
    AlreadyMember,
}

/// What kind of client holds a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// A browser.
    Web,
    /// The Mac app.
    Mac,
    /// The iOS app.
    Ios,
    /// A desktop agent.
    Node,
    /// A personal API token.
    Api,
}

impl DeviceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Mac => "mac",
            Self::Ios => "ios",
            Self::Node => "node",
            Self::Api => "api",
        }
    }
}

/// The role of a member in a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Full control, including deleting the tenant.
    Owner,
    /// Manages members, settings and keys.
    Admin,
    /// Uses the tenant.
    Member,
}

impl Role {
    /// The name stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Parses a stored name. Unknown names count as member.
    pub fn parse(value: &str) -> Self {
        match value {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            _ => Self::Member,
        }
    }

    /// Whether the role may manage members, settings and keys.
    pub fn can_manage(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// A person who can sign in.
#[derive(Clone, Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct User {
    /// User ID.
    pub id: Uuid,
    /// Email address.
    pub email: String,
    /// Display name.
    pub name: String,
    /// When the email address was confirmed.
    pub email_verified_at: Option<DateTime<Utc>>,
    /// When the account was created.
    pub created_at: DateTime<Utc>,
}

/// A device that can sign in.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Device {
    /// Device ID.
    pub id: Uuid,
    /// Name chosen at sign-in.
    pub name: String,
    /// Client kind.
    pub kind: String,
    /// Tenant the token is bound to.
    pub tenant_id: Uuid,
    /// When the device signed in.
    pub created_at: DateTime<Utc>,
    /// Last request, to the minute.
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A new device with its token. The token is shown only once.
#[derive(Debug)]
pub struct IssuedToken {
    /// Device ID.
    pub id: Uuid,
    /// The token.
    pub token: SecretString,
    /// Tenant the token is bound to.
    pub tenant: TenantId,
}

/// Who is behind a valid device token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The user.
    pub user: Uuid,
    /// The tenant the token is bound to.
    pub tenant: TenantId,
    /// The role of the user in that tenant.
    pub role: Role,
    /// The device.
    pub device: Uuid,
    /// Whether the device is a desktop agent.
    pub node: bool,
}

/// A sign-in attempt.
#[derive(Debug)]
pub struct Credentials<'a> {
    /// Email address. Optional while the instance has exactly one user.
    pub email: Option<&'a str>,
    /// Password.
    pub password: &'a SecretString,
    /// Address of the client, for rate limiting.
    pub client: &'a str,
}

/// A new account with its own tenant.
#[derive(Debug)]
pub struct NewAccount<'a> {
    /// Email address.
    pub email: &'a str,
    /// Display name.
    pub name: &'a str,
    /// Password.
    pub password: &'a SecretString,
    /// Name of the new tenant.
    pub tenant_name: &'a str,
}

/// Users, memberships and device tokens of the whole instance.
#[derive(Clone, Debug)]
pub struct Accounts {
    store: Store,
    failures: Arc<RateLimiter>,
    signups: Arc<RateLimiter>,
}

pub(crate) fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub(crate) fn new_token(prefix: &str) -> SecretString {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    SecretString::from(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub(crate) fn hash_password(password: &SecretString) -> Result<String, AuthError> {
    if password.expose_secret().chars().count() < MIN_PASSWORD_LEN {
        return Err(AuthError::WeakPassword);
    }
    Ok(Argon2::default()
        .hash_password(password.expose_secret().as_bytes())
        .map_err(|_| AuthError::Hash)?
        .to_string())
}

fn password_matches(hash: Option<&str>, password: &SecretString) -> bool {
    hash.is_some_and(|hash| {
        PasswordHash::new(hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.expose_secret().as_bytes(), &parsed)
                .is_ok()
        })
    })
}

/// Normalizes and checks an email address. The check is deliberately loose;
/// the verification mail proves the address.
pub fn normalize_email(email: &str) -> Result<String, AuthError> {
    let email = email.trim();
    let valid = email.len() <= 254
        && !email.contains(char::is_whitespace)
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
    if valid {
        Ok(email.to_owned())
    } else {
        Err(AuthError::InvalidEmail)
    }
}

pub(crate) fn check_name(name: &str, what: &str) -> Result<String, AuthError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AuthError::Invalid(format!(
            "the {what} must have 1 to 200 characters"
        )));
    }
    Ok(name.to_owned())
}

pub(crate) fn user_from(row: &sqlx::postgres::PgRow) -> User {
    User {
        id: row.get("id"),
        email: row.get("email"),
        name: row.get("name"),
        email_verified_at: row.get("email_verified_at"),
        created_at: row.get("created_at"),
    }
}

const USER_COLUMNS: &str = "id, email, name, email_verified_at, created_at";

impl Accounts {
    /// Creates the accounts of an instance.
    pub fn new(store: Store) -> Self {
        Self {
            store,
            failures: Arc::new(RateLimiter::new(MAX_FAILURES, LOCKOUT)),
            signups: Arc::new(RateLimiter::new(MAX_SIGNUPS, SIGNUP_WINDOW)),
        }
    }

    pub(crate) fn pool(&self) -> &sqlx::PgPool {
        self.store.pool()
    }

    /// The store the accounts live in.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Turns the owner of an earlier single-user version into the first user.
    ///
    /// Runs only while there is no user. The user gets the configured email
    /// address and name, the stored password and every device token of the
    /// default tenant, and becomes its owner. Personal data of the default
    /// tenant (desktop agents, conversations, mail and calendar, runs) is
    /// assigned to this user. Returns the new user, if one was created.
    pub async fn bootstrap_owner(
        &self,
        email: &str,
        name: &str,
    ) -> Result<Option<Uuid>, AuthError> {
        let email = normalize_email(email)?;
        let name = check_name(name, "name")?;
        let tenant = TenantId::DEFAULT.as_uuid();
        let mut transaction = self.pool().begin().await?;
        sqlx::query("LOCK TABLE users IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *transaction)
            .await?;
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&mut *transaction)
            .await?;
        if count > 0 {
            return Ok(None);
        }
        let hash: Option<(String,)> =
            sqlx::query_as("SELECT password_hash FROM owner_accounts WHERE tenant_id = $1")
                .bind(tenant)
                .fetch_optional(&mut *transaction)
                .await?;
        let user = Uuid::now_v7();
        sqlx::query("INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, $3, $4)")
            .bind(user)
            .bind(&email)
            .bind(&name)
            .bind(hash.map(|(hash,)| hash))
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(tenant)
            .bind(user)
            .execute(&mut *transaction)
            .await?;
        for statement in [
            "UPDATE device_tokens SET user_id = $1 WHERE tenant_id = $2 AND user_id IS NULL",
            "UPDATE nodes SET user_id = $1 WHERE tenant_id = $2 AND user_id IS NULL",
            "UPDATE talk_threads SET user_id = $1 WHERE tenant_id = $2 AND user_id IS NULL",
            "UPDATE runs SET user_id = $1 WHERE tenant_id = $2 AND user_id IS NULL",
            "UPDATE synced_items SET user_id = $1 WHERE tenant_id = $2 AND user_id IS NULL \
             AND domain IN ('mail', 'calendar')",
        ] {
            sqlx::query(statement)
                .bind(user)
                .bind(tenant)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        tracing::info!("created the first user from the configured owner");
        Ok(Some(user))
    }

    /// Number of users.
    pub async fn user_count(&self) -> Result<i64, AuthError> {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }

    /// Whether some user can sign in with a password.
    pub async fn is_set_up(&self) -> Result<bool, AuthError> {
        let (exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM users WHERE password_hash IS NOT NULL)")
                .fetch_one(self.pool())
                .await?;
        Ok(exists)
    }

    /// Returns a user.
    pub async fn user(&self, id: Uuid) -> Result<Option<User>, AuthError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(user_from))
    }

    /// Returns the user with an email address, ignoring case.
    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>, AuthError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users WHERE lower(email) = lower($1)"
        )))
        .bind(email.trim())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(user_from))
    }

    /// The owner agents work for in a tenant: its earliest owner.
    pub async fn primary_user(&self, tenant: TenantId) -> Result<Option<User>, AuthError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {USER_COLUMNS} FROM users WHERE id = tenant_primary_user($1)"
        )))
        .bind(tenant.as_uuid())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(user_from))
    }

    /// Changes the display name of a user.
    pub async fn rename_user(&self, user: Uuid, name: &str) -> Result<(), AuthError> {
        let name = check_name(name, "name")?;
        sqlx::query("UPDATE users SET name = $2, updated_at = now() WHERE id = $1")
            .bind(user)
            .bind(name)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Creates a user without a tenant.
    pub(crate) async fn insert_user(
        transaction: &mut sqlx::PgConnection,
        email: &str,
        name: &str,
        password: &SecretString,
    ) -> Result<User, AuthError> {
        let email = normalize_email(email)?;
        let name = check_name(name, "name")?;
        let hash = hash_password(password)?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING RETURNING {USER_COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(&email)
        .bind(&name)
        .bind(hash)
        .fetch_optional(&mut *transaction)
        .await?;
        row.as_ref().map(user_from).ok_or(AuthError::EmailTaken)
    }

    /// Creates a user with a new tenant they own. Limited per client address.
    pub async fn signup(
        &self,
        account: NewAccount<'_>,
        client: &str,
    ) -> Result<(User, TenantId), AuthError> {
        let key = format!("signup:{client}");
        if !self.signups.allowed(&key) {
            return Err(AuthError::LockedOut);
        }
        self.signups.hit(&key);
        let tenant_name = check_name(account.tenant_name, "tenant name")?;
        let mut transaction = self.pool().begin().await?;
        let user = Self::insert_user(
            &mut transaction,
            account.email,
            account.name,
            account.password,
        )
        .await?;
        let tenant = TenantId::from_uuid(Uuid::now_v7());
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
            .bind(tenant.as_uuid())
            .bind(tenant_name)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(tenant.as_uuid())
            .bind(user.id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok((user, tenant))
    }

    /// Checks email and password. Without an email address, the sign-in works
    /// only while the instance has exactly one user. Failures are limited per
    /// client address and per email address.
    pub async fn authenticate(&self, credentials: Credentials<'_>) -> Result<User, AuthError> {
        let ip_key = format!("ip:{}", credentials.client);
        let email_key = credentials
            .email
            .map(|email| format!("email:{}", email.trim().to_lowercase()));
        if !self.failures.allowed(&ip_key)
            || email_key
                .as_deref()
                .is_some_and(|key| !self.failures.allowed(key))
        {
            return Err(AuthError::LockedOut);
        }
        let row =
            match credentials.email {
                Some(email) => sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT {USER_COLUMNS}, password_hash FROM users WHERE lower(email) = lower($1)"
                )))
                .bind(email.trim())
                .fetch_optional(self.pool())
                .await?,
                None => {
                    let mut rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "SELECT {USER_COLUMNS}, password_hash FROM users LIMIT 2"
                    )))
                    .fetch_all(self.pool())
                    .await?;
                    if rows.len() > 1 {
                        return Err(AuthError::EmailRequired);
                    }
                    rows.pop()
                }
            };
        let hash: Option<String> = row.as_ref().and_then(|row| row.get("password_hash"));
        if password_matches(hash.as_deref(), credentials.password)
            && let Some(row) = row
        {
            self.failures.reset(&ip_key);
            if let Some(key) = &email_key {
                self.failures.reset(key);
            }
            return Ok(user_from(&row));
        }
        self.failures.hit(&ip_key);
        if let Some(key) = &email_key {
            self.failures.hit(key);
        }
        tracing::warn!("failed sign-in");
        Err(AuthError::WrongPassword)
    }

    /// Sets the password of a user and signs out all their devices.
    pub async fn set_password(&self, user: Uuid, password: &SecretString) -> Result<(), AuthError> {
        self.replace_password(user, password, None).await
    }

    /// Changes the password after checking the current one. Every device but
    /// `keep` is signed out.
    pub async fn change_password(
        &self,
        user: Uuid,
        current: &SecretString,
        new: &SecretString,
        keep: Option<Uuid>,
    ) -> Result<(), AuthError> {
        let (hash,): (Option<String>,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
                .bind(user)
                .fetch_optional(self.pool())
                .await?
                .ok_or(AuthError::NotFound("user"))?;
        if !password_matches(hash.as_deref(), current) {
            return Err(AuthError::WrongPassword);
        }
        self.replace_password(user, new, keep).await
    }

    async fn replace_password(
        &self,
        user: Uuid,
        password: &SecretString,
        keep: Option<Uuid>,
    ) -> Result<(), AuthError> {
        let hash = hash_password(password)?;
        let mut transaction = self.pool().begin().await?;
        let updated =
            sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
                .bind(user)
                .bind(hash)
                .execute(&mut *transaction)
                .await?;
        if updated.rows_affected() == 0 {
            return Err(AuthError::NotFound("user"));
        }
        sqlx::query(
            "UPDATE device_tokens SET revoked_at = now() \
             WHERE user_id = $1 AND revoked_at IS NULL AND id IS DISTINCT FROM $2",
        )
        .bind(user)
        .bind(keep)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Issues a token of a user for a tenant they belong to.
    pub async fn issue(
        &self,
        user: Uuid,
        tenant: TenantId,
        name: &str,
        kind: DeviceKind,
    ) -> Result<IssuedToken, AuthError> {
        let name = check_name(name, "device name")?;
        let id = Uuid::now_v7();
        let token = new_token(TOKEN_PREFIX);
        let inserted = sqlx::query(
            "INSERT INTO device_tokens (id, tenant_id, user_id, name, kind, token_hash) \
             SELECT $1, $2, $3, $4, $5, $6 FROM memberships \
             WHERE tenant_id = $2 AND user_id = $3",
        )
        .bind(id)
        .bind(tenant.as_uuid())
        .bind(user)
        .bind(name)
        .bind(kind.as_str())
        .bind(hash_token(token.expose_secret()))
        .execute(self.pool())
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(AuthError::NotMember);
        }
        Ok(IssuedToken { id, token, tenant })
    }

    /// Returns who is behind a token, if it is valid and its user still
    /// belongs to its tenant.
    pub async fn verify(&self, token: &str) -> Result<Option<Identity>, AuthError> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT d.id, d.kind, d.user_id, d.tenant_id, m.role FROM device_tokens d \
             JOIN memberships m ON m.tenant_id = d.tenant_id AND m.user_id = d.user_id \
             WHERE d.token_hash = $1 AND d.revoked_at IS NULL",
        )
        .bind(hash_token(token))
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let device: Uuid = row.get(0);
        let kind: String = row.get(1);
        sqlx::query(
            "UPDATE device_tokens SET last_used_at = now() WHERE id = $1 \
             AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(device)
        .execute(self.pool())
        .await?;
        Ok(Some(Identity {
            user: row.get(2),
            tenant: TenantId::from_uuid(row.get(3)),
            role: Role::parse(row.get(4)),
            device,
            node: kind == "node",
        }))
    }

    /// Lists the signed-in devices of a user, in every tenant.
    pub async fn devices(&self, user: Uuid) -> Result<Vec<Device>, AuthError> {
        let rows = sqlx::query(
            "SELECT id, name, kind, tenant_id, created_at, last_used_at FROM device_tokens \
             WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
        )
        .bind(user)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Device {
                id: row.get(0),
                name: row.get(1),
                kind: row.get(2),
                tenant_id: row.get(3),
                created_at: row.get(4),
                last_used_at: row.get(5),
            })
            .collect())
    }

    /// Signs out a device of a user. Returns whether it was signed in.
    pub async fn revoke(&self, user: Uuid, device: Uuid) -> Result<bool, AuthError> {
        let result = sqlx::query(
            "UPDATE device_tokens SET revoked_at = now() \
             WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
        )
        .bind(device)
        .bind(user)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Creates a token that confirms the email address of a user.
    pub async fn email_verification(&self, user: Uuid) -> Result<SecretString, AuthError> {
        let token = new_token("lhv_");
        sqlx::query(
            "INSERT INTO user_tokens (token_hash, user_id, purpose, expires_at) \
             VALUES ($1, $2, $3, now() + make_interval(hours => $4::int))",
        )
        .bind(hash_token(token.expose_secret()))
        .bind(user)
        .bind(VERIFY_EMAIL)
        .bind(i32::try_from(VERIFY_TTL_HOURS).unwrap_or(48))
        .execute(self.pool())
        .await?;
        Ok(token)
    }

    /// Confirms an email address with a token from [`Self::email_verification`].
    pub async fn verify_email(&self, token: &str) -> Result<Uuid, AuthError> {
        let mut transaction = self.pool().begin().await?;
        let row: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE user_tokens SET used_at = now() \
             WHERE token_hash = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > now() \
             RETURNING user_id",
        )
        .bind(hash_token(token))
        .bind(VERIFY_EMAIL)
        .fetch_optional(&mut *transaction)
        .await?;
        let (user,) = row.ok_or(AuthError::InvalidToken)?;
        sqlx::query(
            "UPDATE users SET email_verified_at = coalesce(email_verified_at, now()) WHERE id = $1",
        )
        .bind(user)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(user)
    }

    /// Deletes a user. Tenants where the user is the only member are deleted
    /// with all their data and returned. Fails if the user is the last owner
    /// of a tenant that has other members, or the only member of the default
    /// tenant.
    pub async fn delete_user(&self, user: Uuid) -> Result<Vec<TenantId>, AuthError> {
        let rows: Vec<(Uuid, bool, bool)> = sqlx::query_as(
            "SELECT m.tenant_id, \
                    NOT EXISTS (SELECT 1 FROM memberships o WHERE o.tenant_id = m.tenant_id \
                                AND o.user_id <> m.user_id), \
                    m.role = 'owner' AND NOT EXISTS (SELECT 1 FROM memberships o \
                                WHERE o.tenant_id = m.tenant_id AND o.user_id <> m.user_id \
                                AND o.role = 'owner') \
             FROM memberships m WHERE m.user_id = $1",
        )
        .bind(user)
        .fetch_all(self.pool())
        .await?;
        let mut alone = Vec::new();
        for (tenant, only_member, last_owner) in rows {
            let tenant = TenantId::from_uuid(tenant);
            if only_member {
                if tenant == TenantId::DEFAULT {
                    return Err(AuthError::DefaultTenant);
                }
                alone.push(tenant);
            } else if last_owner {
                return Err(AuthError::LastOwner);
            }
        }
        for tenant in &alone {
            self.store.purge_tenant(*tenant).await?;
        }
        let deleted = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user)
            .execute(self.pool())
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(AuthError::NotFound("user"));
        }
        Ok(alone)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lighthouse_store::testing::fresh_store;

    pub(crate) const PASSWORD: &str = "correct horse battery";

    pub(crate) fn secret(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    pub(crate) fn login<'a>(email: Option<&'a str>, password: &'a SecretString) -> Credentials<'a> {
        Credentials {
            email,
            password,
            client: "127.0.0.1",
        }
    }

    /// Accounts with the bootstrapped owner `owner@example.com`.
    pub(crate) async fn accounts() -> (Accounts, Uuid) {
        let accounts = Accounts::new(fresh_store().await);
        let user = accounts
            .bootstrap_owner("owner@example.com", "Alex")
            .await
            .unwrap()
            .unwrap();
        accounts
            .set_password(user, &secret(PASSWORD))
            .await
            .unwrap();
        (accounts, user)
    }

    pub(crate) async fn signup(accounts: &Accounts, email: &str) -> (User, TenantId) {
        let password = secret(PASSWORD);
        accounts
            .signup(
                NewAccount {
                    email,
                    name: "Sam",
                    password: &password,
                    tenant_name: "Sam's team",
                },
                email,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn bootstrap_keeps_the_old_password_tokens_and_personal_data() {
        let store = fresh_store().await;
        let nil = TenantId::DEFAULT.as_uuid();
        let old_hash = hash_password(&secret(PASSWORD)).unwrap();
        sqlx::query("INSERT INTO owner_accounts (tenant_id, password_hash) VALUES ($1, $2)")
            .bind(nil)
            .bind(old_hash)
            .execute(store.pool())
            .await
            .unwrap();
        let old_token = "lht_legacy-token";
        sqlx::query(
            "INSERT INTO device_tokens (id, tenant_id, name, kind, token_hash) \
             VALUES ($1, $2, 'mac', 'mac', $3)",
        )
        .bind(Uuid::now_v7())
        .bind(nil)
        .bind(hash_token(old_token))
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO talk_threads (id, tenant_id, title) VALUES ($1, $2, 'old')")
            .bind(Uuid::now_v7())
            .bind(nil)
            .execute(store.pool())
            .await
            .unwrap();
        let accounts = Accounts::new(store.clone());
        assert!(!accounts.is_set_up().await.unwrap());
        let user = accounts
            .bootstrap_owner(" Owner@Example.com ", "Alex")
            .await
            .unwrap()
            .unwrap();
        assert!(accounts.is_set_up().await.unwrap());
        assert_eq!(
            accounts
                .bootstrap_owner("other@example.com", "B")
                .await
                .unwrap(),
            None
        );

        let identity = accounts.verify(old_token).await.unwrap().unwrap();
        assert_eq!(identity.user, user);
        assert_eq!(identity.tenant, TenantId::DEFAULT);
        assert_eq!(identity.role, Role::Owner);
        assert!(!identity.node);

        let password = secret(PASSWORD);
        let signed_in = accounts.authenticate(login(None, &password)).await.unwrap();
        assert_eq!(signed_in.id, user);
        assert_eq!(signed_in.email, "Owner@Example.com");
        let by_email = accounts
            .authenticate(login(Some("owner@EXAMPLE.com"), &password))
            .await
            .unwrap();
        assert_eq!(by_email.id, user);
        let (owner,): (Option<Uuid>,) = sqlx::query_as("SELECT user_id FROM talk_threads LIMIT 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(owner, Some(user));
        assert_eq!(
            accounts
                .primary_user(TenantId::DEFAULT)
                .await
                .unwrap()
                .unwrap()
                .id,
            user
        );
    }

    #[tokio::test]
    async fn a_fresh_instance_needs_setup_and_rejects_every_login() {
        let accounts = Accounts::new(fresh_store().await);
        accounts
            .bootstrap_owner("owner@example.com", "Alex")
            .await
            .unwrap();
        assert!(!accounts.is_set_up().await.unwrap());
        let password = secret(PASSWORD);
        let result = accounts.authenticate(login(None, &password)).await;
        assert!(matches!(result, Err(AuthError::WrongPassword)));
    }

    #[tokio::test]
    async fn short_passwords_and_bad_emails_are_rejected() {
        let (accounts, user) = accounts().await;
        let result = accounts.set_password(user, &secret("short")).await;
        assert!(matches!(result, Err(AuthError::WeakPassword)));
        for email in ["", "no-at", "a@b", "a b@example.com", "@example.com"] {
            assert!(normalize_email(email).is_err(), "{email}");
        }
        let password = secret(PASSWORD);
        let result = accounts
            .signup(
                NewAccount {
                    email: "x@example.com",
                    name: " ",
                    password: &password,
                    tenant_name: "t",
                },
                "1",
            )
            .await;
        assert!(matches!(result, Err(AuthError::Invalid(_))));
    }

    #[tokio::test]
    async fn tokens_verify_until_revoked_and_only_by_their_user() {
        let (accounts, user) = accounts().await;
        let issued = accounts
            .issue(user, TenantId::DEFAULT, "phone", DeviceKind::Ios)
            .await
            .unwrap();
        let token = issued.token.expose_secret();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(
            accounts.verify(token).await.unwrap().unwrap().device,
            issued.id
        );

        let devices = accounts.devices(user).await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "phone");
        assert_eq!(devices[0].kind, "ios");
        assert_eq!(devices[0].tenant_id, TenantId::DEFAULT.as_uuid());
        assert!(devices[0].last_used_at.is_some());

        let (other, _) = signup(&accounts, "sam@example.com").await;
        assert!(!accounts.revoke(other.id, issued.id).await.unwrap());
        assert!(accounts.revoke(user, issued.id).await.unwrap());
        assert!(!accounts.revoke(user, issued.id).await.unwrap());
        assert_eq!(accounts.verify(token).await.unwrap(), None);
        assert!(accounts.devices(user).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tokens_need_a_membership() {
        let (accounts, user) = accounts().await;
        let (_, tenant) = signup(&accounts, "sam@example.com").await;
        let result = accounts.issue(user, tenant, "x", DeviceKind::Web).await;
        assert!(matches!(result, Err(AuthError::NotMember)));
    }

    #[tokio::test]
    async fn the_database_never_holds_the_token() {
        let (accounts, user) = accounts().await;
        let issued = accounts
            .issue(user, TenantId::DEFAULT, "cli", DeviceKind::Api)
            .await
            .unwrap();
        let stored: (Vec<u8>,) = sqlx::query_as("SELECT token_hash FROM device_tokens")
            .fetch_one(accounts.pool())
            .await
            .unwrap();
        assert_ne!(stored.0, issued.token.expose_secret().as_bytes());
        assert_eq!(stored.0.len(), 32);
    }

    #[tokio::test]
    async fn unknown_tokens_do_not_verify() {
        let (accounts, _) = accounts().await;
        assert_eq!(accounts.verify("nonsense").await.unwrap(), None);
        assert_eq!(accounts.verify("lht_unknown").await.unwrap(), None);
    }

    #[tokio::test]
    async fn repeated_failures_lock_the_login_even_for_the_right_password() {
        let (accounts, _) = accounts().await;
        let wrong = secret("wrong password!");
        for _ in 0..MAX_FAILURES {
            let result = accounts.authenticate(login(None, &wrong)).await;
            assert!(matches!(result, Err(AuthError::WrongPassword)));
        }
        let right = secret(PASSWORD);
        let result = accounts.authenticate(login(None, &right)).await;
        assert!(matches!(result, Err(AuthError::LockedOut)));
    }

    #[tokio::test]
    async fn failures_lock_an_email_address_from_any_client() {
        let (accounts, _) = accounts().await;
        let wrong = secret("wrong password!");
        for index in 0..MAX_FAILURES {
            let client = format!("10.0.0.{index}");
            let attempt = Credentials {
                email: Some("owner@example.com"),
                password: &wrong,
                client: &client,
            };
            let _ = accounts.authenticate(attempt).await;
        }
        let right = secret(PASSWORD);
        let attempt = Credentials {
            email: Some("OWNER@example.com"),
            password: &right,
            client: "10.9.9.9",
        };
        assert!(matches!(
            accounts.authenticate(attempt).await,
            Err(AuthError::LockedOut)
        ));
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let (accounts, _) = accounts().await;
        let wrong = secret("wrong password!");
        let right = secret(PASSWORD);
        for _ in 0..MAX_FAILURES - 1 {
            let _ = accounts.authenticate(login(None, &wrong)).await;
        }
        accounts.authenticate(login(None, &right)).await.unwrap();
        let result = accounts.authenticate(login(None, &wrong)).await;
        assert!(matches!(result, Err(AuthError::WrongPassword)));
    }

    #[tokio::test]
    async fn several_users_need_an_email_to_sign_in() {
        let (accounts, _) = accounts().await;
        let (sam, _) = signup(&accounts, "sam@example.com").await;
        let password = secret(PASSWORD);
        assert!(matches!(
            accounts.authenticate(login(None, &password)).await,
            Err(AuthError::EmailRequired)
        ));
        let user = accounts
            .authenticate(login(Some("sam@example.com"), &password))
            .await
            .unwrap();
        assert_eq!(user.id, sam.id);
        assert!(matches!(
            accounts
                .authenticate(login(Some("nobody@example.com"), &password))
                .await,
            Err(AuthError::WrongPassword)
        ));
    }

    #[tokio::test]
    async fn signup_creates_an_owned_tenant_and_rejects_taken_emails() {
        let (accounts, _) = accounts().await;
        let (sam, tenant) = signup(&accounts, "sam@example.com").await;
        let tenants = accounts.tenants_of(sam.id).await.unwrap();
        assert_eq!(tenants.len(), 1);
        assert_eq!(tenants[0].tenant_id, tenant.as_uuid());
        assert_eq!(tenants[0].role, Role::Owner);
        let password = secret(PASSWORD);
        let again = accounts
            .signup(
                NewAccount {
                    email: "SAM@example.com",
                    name: "Sam",
                    password: &password,
                    tenant_name: "t",
                },
                "other",
            )
            .await;
        assert!(matches!(again, Err(AuthError::EmailTaken)));
    }

    #[tokio::test]
    async fn signups_are_limited_per_client() {
        let accounts = Accounts::new(fresh_store().await);
        let password = secret(PASSWORD);
        for index in 0..=MAX_SIGNUPS {
            let email = format!("u{index}@example.com");
            let result = accounts
                .signup(
                    NewAccount {
                        email: &email,
                        name: "U",
                        password: &password,
                        tenant_name: "T",
                    },
                    "203.0.113.1",
                )
                .await;
            if index < MAX_SIGNUPS {
                result.unwrap();
            } else {
                assert!(matches!(result, Err(AuthError::LockedOut)));
            }
        }
    }

    #[tokio::test]
    async fn changing_the_password_signs_out_every_other_device() {
        let (accounts, user) = accounts().await;
        let kept = accounts
            .issue(user, TenantId::DEFAULT, "this", DeviceKind::Web)
            .await
            .unwrap();
        let old = accounts
            .issue(user, TenantId::DEFAULT, "old", DeviceKind::Web)
            .await
            .unwrap();
        let new = secret("another long password");
        let wrong = accounts
            .change_password(user, &secret("not the password"), &new, Some(kept.id))
            .await;
        assert!(matches!(wrong, Err(AuthError::WrongPassword)));
        accounts
            .change_password(user, &secret(PASSWORD), &new, Some(kept.id))
            .await
            .unwrap();
        assert!(
            accounts
                .verify(kept.token.expose_secret())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            accounts
                .verify(old.token.expose_secret())
                .await
                .unwrap()
                .is_none()
        );
        let old_password = secret(PASSWORD);
        assert!(matches!(
            accounts.authenticate(login(None, &old_password)).await,
            Err(AuthError::WrongPassword)
        ));
        accounts.set_password(user, &new).await.unwrap();
        assert!(
            accounts
                .verify(kept.token.expose_secret())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn email_verification_tokens_work_once() {
        let (accounts, user) = accounts().await;
        assert!(
            accounts
                .user(user)
                .await
                .unwrap()
                .unwrap()
                .email_verified_at
                .is_none()
        );
        let token = accounts.email_verification(user).await.unwrap();
        assert_eq!(
            accounts.verify_email(token.expose_secret()).await.unwrap(),
            user
        );
        assert!(
            accounts
                .user(user)
                .await
                .unwrap()
                .unwrap()
                .email_verified_at
                .is_some()
        );
        assert!(matches!(
            accounts.verify_email(token.expose_secret()).await,
            Err(AuthError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn deleting_a_user_deletes_their_solo_tenants() {
        let (accounts, owner) = accounts().await;
        let (sam, tenant) = signup(&accounts, "sam@example.com").await;
        accounts.rename_user(sam.id, "Samantha").await.unwrap();
        assert_eq!(
            accounts.user(sam.id).await.unwrap().unwrap().name,
            "Samantha"
        );
        assert_eq!(accounts.delete_user(sam.id).await.unwrap(), vec![tenant]);
        assert!(accounts.user(sam.id).await.unwrap().is_none());
        assert!(accounts.tenant(tenant).await.unwrap().is_none());
        assert!(matches!(
            accounts.delete_user(owner).await,
            Err(AuthError::DefaultTenant)
        ));
        assert!(matches!(
            accounts.delete_user(sam.id).await,
            Err(AuthError::NotFound(_))
        ));
    }

    #[test]
    fn roles_round_trip_and_know_who_manages() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(Role::parse(role.as_str()), role);
        }
        assert_eq!(Role::parse("unknown"), Role::Member);
        assert!(Role::Admin.can_manage());
        assert!(!Role::Member.can_manage());
    }
}
