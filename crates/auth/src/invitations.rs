//! Invitations to a tenant.
//!
//! An invitation holds a random token, of which the database keeps only the
//! hash. Whoever has the token can join the tenant with the invited email
//! address, either as a new user or by signing in to the matching account.

use crate::{Accounts, AuthError, Role, User, hash_token, new_token, normalize_email};
use chrono::{DateTime, Utc};
use lighthouse_core::TenantId;
use lighthouse_store::sqlx::{self, Row};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

const INVITATION_PREFIX: &str = "lhi_";
const INVITATION_DAYS: i32 = 7;

/// An open invitation.
#[derive(Clone, Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct Invitation {
    /// Invitation ID.
    pub id: Uuid,
    /// Invited email address.
    pub email: String,
    /// Role the new member gets.
    pub role: Role,
    /// Who invited.
    pub invited_by: Option<Uuid>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// A new invitation with its token. The token is shown only once.
#[derive(Debug)]
pub struct IssuedInvitation {
    /// The invitation.
    pub invitation: Invitation,
    /// The token for the accept link.
    pub token: SecretString,
}

/// Who accepts an invitation.
#[derive(Debug)]
pub enum AcceptAs<'a> {
    /// A signed-in user whose email address matches the invitation.
    Existing(Uuid),
    /// A new user with the invited email address.
    New {
        /// Display name.
        name: &'a str,
        /// Password.
        password: &'a SecretString,
    },
}

/// The result of accepting an invitation.
#[derive(Debug)]
pub struct Accepted {
    /// The member.
    pub user: User,
    /// The tenant joined.
    pub tenant: TenantId,
    /// The role there.
    pub role: Role,
}

fn invitation_from(row: &sqlx::postgres::PgRow) -> Invitation {
    Invitation {
        id: row.get("id"),
        email: row.get("email"),
        role: Role::parse(row.get("role")),
        invited_by: row.get("invited_by"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
    }
}

const COLUMNS: &str = "id, email, role, invited_by, created_at, expires_at";

impl Accounts {
    /// Invites an email address to a tenant. An open invitation for the same
    /// address is replaced.
    pub async fn invite(
        &self,
        tenant: TenantId,
        email: &str,
        role: Role,
        invited_by: Option<Uuid>,
    ) -> Result<IssuedInvitation, AuthError> {
        let email = normalize_email(email)?;
        let token = new_token(INVITATION_PREFIX);
        let mut transaction = self.pool().begin().await?;
        sqlx::query(
            "UPDATE invitations SET revoked_at = now() WHERE tenant_id = $1 \
             AND lower(email) = lower($2) AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(tenant.as_uuid())
        .bind(&email)
        .execute(&mut *transaction)
        .await?;
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO invitations (id, tenant_id, email, role, token_hash, invited_by, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7)) RETURNING {COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(tenant.as_uuid())
        .bind(&email)
        .bind(role.as_str())
        .bind(hash_token(token.expose_secret()))
        .bind(invited_by)
        .bind(INVITATION_DAYS)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(IssuedInvitation {
            invitation: invitation_from(&row),
            token,
        })
    }

    /// Lists the open invitations of a tenant.
    pub async fn invitations(&self, tenant: TenantId) -> Result<Vec<Invitation>, AuthError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM invitations WHERE tenant_id = $1 AND accepted_at IS NULL \
             AND revoked_at IS NULL AND expires_at > now() ORDER BY created_at DESC"
        )))
        .bind(tenant.as_uuid())
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(invitation_from).collect())
    }

    /// Revokes an open invitation. Returns whether it was open.
    pub async fn revoke_invitation(&self, tenant: TenantId, id: Uuid) -> Result<bool, AuthError> {
        let result = sqlx::query(
            "UPDATE invitations SET revoked_at = now() WHERE id = $1 AND tenant_id = $2 \
             AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(tenant.as_uuid())
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns the open invitation behind a token, with the tenant name.
    pub async fn invitation(&self, token: &str) -> Result<(Invitation, String), AuthError> {
        let row = sqlx::query(
            "SELECT i.id, i.email, i.role, i.invited_by, i.created_at, i.expires_at, t.name \
             FROM invitations i JOIN tenants t ON t.id = i.tenant_id \
             WHERE i.token_hash = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL \
             AND i.expires_at > now()",
        )
        .bind(hash_token(token))
        .fetch_optional(self.pool())
        .await?
        .ok_or(AuthError::InvalidToken)?;
        Ok((invitation_from(&row), row.get("name")))
    }

    /// Accepts an invitation and adds the membership.
    pub async fn accept_invitation(
        &self,
        token: &str,
        who: AcceptAs<'_>,
    ) -> Result<Accepted, AuthError> {
        let mut transaction = self.pool().begin().await?;
        let row: Option<(Uuid, Uuid, String, String)> = sqlx::query_as(
            "UPDATE invitations SET accepted_at = now() WHERE token_hash = $1 \
             AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() \
             RETURNING id, tenant_id, email, role",
        )
        .bind(hash_token(token))
        .fetch_optional(&mut *transaction)
        .await?;
        let (_, tenant, email, role) = row.ok_or(AuthError::InvalidToken)?;
        let user = match who {
            AcceptAs::Existing(user) => {
                let row = sqlx::query(
                    "SELECT id, email, name, email_verified_at, created_at FROM users \
                     WHERE id = $1 AND lower(email) = lower($2)",
                )
                .bind(user)
                .bind(&email)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AuthError::InvalidToken)?;
                crate::user_from(&row)
            }
            AcceptAs::New { name, password } => {
                Self::insert_user(&mut transaction, &email, name, password).await?
            }
        };
        sqlx::query(
            "INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant_id, user_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(user.id)
        .bind(&role)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let tenant = TenantId::from_uuid(tenant);
        let role = self
            .role(tenant, user.id)
            .await?
            .unwrap_or(Role::parse(&role));
        Ok(Accepted { user, tenant, role })
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{PASSWORD, accounts, secret, signup};
    use crate::{AcceptAs, AuthError, Role};
    use lighthouse_core::TenantId;
    use secrecy::ExposeSecret;

    #[tokio::test]
    async fn a_new_user_accepts_once_with_the_invited_email() {
        let (accounts, owner) = accounts().await;
        let issued = accounts
            .invite(
                TenantId::DEFAULT,
                "new@example.com",
                Role::Admin,
                Some(owner),
            )
            .await
            .unwrap();
        assert!(issued.token.expose_secret().starts_with("lhi_"));
        let open = accounts.invitations(TenantId::DEFAULT).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].invited_by, Some(owner));
        let (seen, tenant_name) = accounts
            .invitation(issued.token.expose_secret())
            .await
            .unwrap();
        assert_eq!(
            (seen.email.as_str(), tenant_name.as_str()),
            ("new@example.com", "default")
        );

        let password = secret(PASSWORD);
        let accepted = accounts
            .accept_invitation(
                issued.token.expose_secret(),
                AcceptAs::New {
                    name: "Robin",
                    password: &password,
                },
            )
            .await
            .unwrap();
        assert_eq!(accepted.user.email, "new@example.com");
        assert_eq!(accepted.tenant, TenantId::DEFAULT);
        assert_eq!(accepted.role, Role::Admin);
        assert!(
            accounts
                .invitations(TenantId::DEFAULT)
                .await
                .unwrap()
                .is_empty()
        );
        let again = accounts
            .accept_invitation(issued.token.expose_secret(), AcceptAs::Existing(owner))
            .await;
        assert!(matches!(again, Err(AuthError::InvalidToken)));
    }

    #[tokio::test]
    async fn existing_users_need_the_invited_email() {
        let (accounts, owner) = accounts().await;
        let (sam, _) = signup(&accounts, "sam@example.com").await;
        let issued = accounts
            .invite(
                TenantId::DEFAULT,
                "SAM@example.com",
                Role::Member,
                Some(owner),
            )
            .await
            .unwrap();
        let wrong = accounts
            .accept_invitation(issued.token.expose_secret(), AcceptAs::Existing(owner))
            .await;
        assert!(matches!(wrong, Err(AuthError::InvalidToken)));
        let accepted = accounts
            .accept_invitation(issued.token.expose_secret(), AcceptAs::Existing(sam.id))
            .await
            .unwrap();
        assert_eq!(accepted.role, Role::Member);
        assert_eq!(accounts.tenants_of(sam.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn invitations_can_be_replaced_and_revoked() {
        let (accounts, _) = accounts().await;
        let first = accounts
            .invite(TenantId::DEFAULT, "x@example.com", Role::Member, None)
            .await
            .unwrap();
        let second = accounts
            .invite(TenantId::DEFAULT, "x@example.com", Role::Admin, None)
            .await
            .unwrap();
        assert!(
            accounts
                .invitation(first.token.expose_secret())
                .await
                .is_err()
        );
        let open = accounts.invitations(TenantId::DEFAULT).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].role, Role::Admin);
        assert!(
            accounts
                .revoke_invitation(TenantId::DEFAULT, second.invitation.id)
                .await
                .unwrap()
        );
        assert!(
            !accounts
                .revoke_invitation(TenantId::DEFAULT, second.invitation.id)
                .await
                .unwrap()
        );
        assert!(
            accounts
                .invitation(second.token.expose_secret())
                .await
                .is_err()
        );
        assert!(matches!(
            accounts
                .invite(TenantId::DEFAULT, "bad", Role::Member, None)
                .await,
            Err(AuthError::InvalidEmail)
        ));
    }
}
