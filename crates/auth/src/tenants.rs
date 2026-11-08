//! Tenants, their members and roles.

use crate::{Accounts, AuthError, Role, check_name};
use chrono::{DateTime, Utc};
use lighthouse_core::TenantId;
use lighthouse_store::sqlx::{self, Row};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

/// A tenant: the space a team or a person works in.
#[derive(Clone, Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct Tenant {
    /// Tenant ID.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// A tenant a user belongs to.
#[derive(Clone, Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct Membership {
    /// Tenant ID.
    pub tenant_id: Uuid,
    /// Name of the tenant.
    pub tenant_name: String,
    /// The role of the user there.
    pub role: Role,
    /// When the user joined.
    pub joined_at: DateTime<Utc>,
}

/// A member of a tenant.
#[derive(Clone, Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct Member {
    /// User ID.
    pub user_id: Uuid,
    /// Email address.
    pub email: String,
    /// Display name.
    pub name: String,
    /// Role in the tenant.
    pub role: Role,
    /// When the user joined.
    pub joined_at: DateTime<Utc>,
}

impl Accounts {
    /// Returns a tenant.
    pub async fn tenant(&self, tenant: TenantId) -> Result<Option<Tenant>, AuthError> {
        let row = sqlx::query("SELECT id, name, created_at FROM tenants WHERE id = $1")
            .bind(tenant.as_uuid())
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(|row| Tenant {
            id: row.get(0),
            name: row.get(1),
            created_at: row.get(2),
        }))
    }

    /// Lists every tenant of the instance, for background work.
    pub async fn all_tenants(&self) -> Result<Vec<TenantId>, AuthError> {
        let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM tenants ORDER BY created_at, id")
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id,)| TenantId::from_uuid(id))
            .collect())
    }

    /// Creates a tenant owned by an existing user.
    pub async fn create_tenant(&self, owner: Uuid, name: &str) -> Result<TenantId, AuthError> {
        let name = check_name(name, "tenant name")?;
        let tenant = TenantId::generate();
        let mut transaction = self.pool().begin().await?;
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
            .bind(tenant.as_uuid())
            .bind(name)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(tenant.as_uuid())
            .bind(owner)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(tenant)
    }

    /// Renames a tenant.
    pub async fn rename_tenant(&self, tenant: TenantId, name: &str) -> Result<Tenant, AuthError> {
        let name = check_name(name, "tenant name")?;
        let row = sqlx::query(
            "UPDATE tenants SET name = $2 WHERE id = $1 RETURNING id, name, created_at",
        )
        .bind(tenant.as_uuid())
        .bind(name)
        .fetch_optional(self.pool())
        .await?
        .ok_or(AuthError::NotFound("tenant"))?;
        Ok(Tenant {
            id: row.get(0),
            name: row.get(1),
            created_at: row.get(2),
        })
    }

    /// Lists the tenants of a user, oldest membership first.
    pub async fn tenants_of(&self, user: Uuid) -> Result<Vec<Membership>, AuthError> {
        let rows = sqlx::query(
            "SELECT t.id, t.name, m.role, m.created_at FROM memberships m \
             JOIN tenants t ON t.id = m.tenant_id WHERE m.user_id = $1 \
             ORDER BY m.created_at, t.id",
        )
        .bind(user)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Membership {
                tenant_id: row.get(0),
                tenant_name: row.get(1),
                role: Role::parse(row.get(2)),
                joined_at: row.get(3),
            })
            .collect())
    }

    /// Returns the role of a user in a tenant.
    pub async fn role(&self, tenant: TenantId, user: Uuid) -> Result<Option<Role>, AuthError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT role FROM memberships WHERE tenant_id = $1 AND user_id = $2")
                .bind(tenant.as_uuid())
                .bind(user)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(role,)| Role::parse(&role)))
    }

    /// Lists the members of a tenant.
    pub async fn members(&self, tenant: TenantId) -> Result<Vec<Member>, AuthError> {
        let rows = sqlx::query(
            "SELECT u.id, u.email, u.name, m.role, m.created_at FROM memberships m \
             JOIN users u ON u.id = m.user_id WHERE m.tenant_id = $1 \
             ORDER BY m.created_at, u.id",
        )
        .bind(tenant.as_uuid())
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Member {
                user_id: row.get(0),
                email: row.get(1),
                name: row.get(2),
                role: Role::parse(row.get(3)),
                joined_at: row.get(4),
            })
            .collect())
    }

    async fn lock_memberships(
        transaction: &mut sqlx::PgConnection,
        tenant: TenantId,
    ) -> Result<Vec<(Uuid, Role)>, AuthError> {
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT user_id, role FROM memberships WHERE tenant_id = $1 FOR UPDATE")
                .bind(tenant.as_uuid())
                .fetch_all(&mut *transaction)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(user, role)| (user, Role::parse(&role)))
            .collect())
    }

    fn leaves_an_owner(members: &[(Uuid, Role)], user: Uuid) -> bool {
        members
            .iter()
            .any(|(other, role)| *other != user && *role == Role::Owner)
    }

    /// Changes the role of a member. The last owner cannot be demoted.
    pub async fn set_role(
        &self,
        tenant: TenantId,
        user: Uuid,
        role: Role,
    ) -> Result<(), AuthError> {
        let mut transaction = self.pool().begin().await?;
        let members = Self::lock_memberships(&mut transaction, tenant).await?;
        let current = members
            .iter()
            .find(|(member, _)| *member == user)
            .map(|(_, role)| *role)
            .ok_or(AuthError::NotMember)?;
        if current == Role::Owner && role != Role::Owner && !Self::leaves_an_owner(&members, user) {
            return Err(AuthError::LastOwner);
        }
        sqlx::query("UPDATE memberships SET role = $3 WHERE tenant_id = $1 AND user_id = $2")
            .bind(tenant.as_uuid())
            .bind(user)
            .bind(role.as_str())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Removes a member and signs out their devices in the tenant. The last
    /// owner cannot leave.
    pub async fn remove_member(&self, tenant: TenantId, user: Uuid) -> Result<(), AuthError> {
        let mut transaction = self.pool().begin().await?;
        let members = Self::lock_memberships(&mut transaction, tenant).await?;
        let role = members
            .iter()
            .find(|(member, _)| *member == user)
            .map(|(_, role)| *role)
            .ok_or(AuthError::NotMember)?;
        if role == Role::Owner && !Self::leaves_an_owner(&members, user) {
            return Err(AuthError::LastOwner);
        }
        sqlx::query("DELETE FROM memberships WHERE tenant_id = $1 AND user_id = $2")
            .bind(tenant.as_uuid())
            .bind(user)
            .execute(&mut *transaction)
            .await?;
        // Their personal data in this tenant leaves with them. Runs stay in
        // the audit log, visible to nobody but the admin token.
        for statement in [
            "UPDATE device_tokens SET revoked_at = now() \
             WHERE tenant_id = $1 AND user_id = $2 AND revoked_at IS NULL",
            "DELETE FROM talk_threads WHERE tenant_id = $1 AND user_id = $2",
            "DELETE FROM nodes WHERE tenant_id = $1 AND user_id = $2",
            "DELETE FROM synced_items WHERE tenant_id = $1 AND user_id = $2",
        ] {
            sqlx::query(statement)
                .bind(tenant.as_uuid())
                .bind(user)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Deletes a tenant with all its data. The default tenant stays.
    pub async fn delete_tenant(&self, tenant: TenantId) -> Result<(), AuthError> {
        if tenant == TenantId::DEFAULT {
            return Err(AuthError::DefaultTenant);
        }
        if self.tenant(tenant).await?.is_none() {
            return Err(AuthError::NotFound("tenant"));
        }
        self.store().purge_tenant(tenant).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{accounts, signup};
    use crate::{AuthError, DeviceKind, Role};
    use lighthouse_core::TenantId;
    use lighthouse_store::sqlx;
    use secrecy::ExposeSecret;

    #[tokio::test]
    async fn the_last_owner_cannot_be_demoted_or_removed() {
        let (accounts, owner) = accounts().await;
        let tenant = TenantId::DEFAULT;
        assert!(matches!(
            accounts.set_role(tenant, owner, Role::Admin).await,
            Err(AuthError::LastOwner)
        ));
        assert!(matches!(
            accounts.remove_member(tenant, owner).await,
            Err(AuthError::LastOwner)
        ));
        let (sam, _) = signup(&accounts, "sam@example.com").await;
        assert!(matches!(
            accounts.set_role(tenant, sam.id, Role::Admin).await,
            Err(AuthError::NotMember)
        ));
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, 'member')")
            .bind(tenant.as_uuid())
            .bind(sam.id)
            .execute(accounts.pool())
            .await
            .unwrap();
        accounts
            .set_role(tenant, sam.id, Role::Owner)
            .await
            .unwrap();
        accounts
            .set_role(tenant, owner, Role::Member)
            .await
            .unwrap();
        assert_eq!(
            accounts.role(tenant, owner).await.unwrap(),
            Some(Role::Member)
        );
        assert!(matches!(
            accounts.remove_member(tenant, sam.id).await,
            Err(AuthError::LastOwner)
        ));
        let members = accounts.members(tenant).await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[1].email, "sam@example.com");
        assert_eq!(
            accounts.primary_user(tenant).await.unwrap().unwrap().id,
            sam.id
        );
    }

    #[tokio::test]
    async fn removing_a_member_signs_out_their_devices_there() {
        let (accounts, owner) = accounts().await;
        let (sam, own) = signup(&accounts, "sam@example.com").await;
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, 'member')")
            .bind(TenantId::DEFAULT.as_uuid())
            .bind(sam.id)
            .execute(accounts.pool())
            .await
            .unwrap();
        let here = accounts
            .issue(sam.id, TenantId::DEFAULT, "web", DeviceKind::Web)
            .await
            .unwrap();
        let there = accounts
            .issue(sam.id, own, "web", DeviceKind::Web)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO talk_threads (id, tenant_id, title, user_id) VALUES ($1, $2, 'mine', $3)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(TenantId::DEFAULT.as_uuid())
        .bind(sam.id)
        .execute(accounts.pool())
        .await
        .unwrap();
        accounts
            .remove_member(TenantId::DEFAULT, sam.id)
            .await
            .unwrap();
        assert!(
            accounts
                .verify(here.token.expose_secret())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            accounts
                .verify(there.token.expose_secret())
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            accounts.role(TenantId::DEFAULT, sam.id).await.unwrap(),
            None
        );
        let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM talk_threads")
            .fetch_one(accounts.pool())
            .await
            .unwrap();
        assert_eq!(left, 0, "their conversations leave with them");
        assert_eq!(
            accounts.members(TenantId::DEFAULT).await.unwrap()[0].user_id,
            owner
        );
    }

    #[tokio::test]
    async fn tenants_can_be_renamed_listed_and_deleted() {
        let (accounts, _) = accounts().await;
        let (sam, tenant) = signup(&accounts, "sam@example.com").await;
        let second = accounts
            .create_tenant(sam.id, "Side project")
            .await
            .unwrap();
        assert_eq!(accounts.tenants_of(sam.id).await.unwrap().len(), 2);
        accounts.delete_tenant(second).await.unwrap();
        assert_eq!(
            accounts
                .rename_tenant(tenant, " Studio ")
                .await
                .unwrap()
                .name,
            "Studio"
        );
        assert!(matches!(
            accounts.rename_tenant(tenant, "").await,
            Err(AuthError::Invalid(_))
        ));
        assert_eq!(
            accounts.all_tenants().await.unwrap(),
            vec![TenantId::DEFAULT, tenant]
        );
        assert!(matches!(
            accounts.delete_tenant(TenantId::DEFAULT).await,
            Err(AuthError::DefaultTenant)
        ));
        accounts.delete_tenant(tenant).await.unwrap();
        assert!(matches!(
            accounts.delete_tenant(tenant).await,
            Err(AuthError::NotFound(_))
        ));
        assert!(matches!(
            accounts.rename_tenant(tenant, "x").await,
            Err(AuthError::NotFound(_))
        ));
    }
}
