//! Extension points for programs that build their own server on these crates.
//!
//! The defaults allow everything and do nothing, which is how
//! `lighthouse-server` runs. Another binary can pass its own implementations,
//! for example to approve sign-ups by hand or to limit what a tenant may do.

use async_trait::async_trait;
use lighthouse_core::TenantId;
use uuid::Uuid;

/// What happens to a sign-up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignupDecision {
    /// The account and its tenant are created.
    Allow,
    /// The sign-up is refused with a message for the person.
    Reject(String),
    /// Nothing is created yet; the message tells the person what happens next,
    /// for example that someone has to approve the sign-up first.
    Pending(String),
}

/// Hooks around accounts and tenants. Every method has a neutral default.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    /// Decides on a sign-up before anything is created.
    async fn on_signup(&self, _email: &str, _client: &str) -> SignupDecision {
        SignupDecision::Allow
    }

    /// Runs after a tenant was created, by sign-up or by `POST /tenants`.
    async fn on_tenant_created(&self, _tenant: TenantId, _owner: Uuid) {}

    /// Runs after a tenant and all its data were deleted.
    async fn on_tenant_deleted(&self, _tenant: TenantId) {}
}

/// Hooks that do nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl Lifecycle for NoHooks {}

/// Something a tenant or user wants to do that may be limited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// A user creates another tenant.
    CreateTenant {
        /// The user.
        user: Uuid,
    },
    /// An owner or admin invites someone; `members` is the current count.
    InviteMember {
        /// Members of the tenant now.
        members: usize,
    },
    /// An agent starts a run, by hand, on a schedule or from an automation.
    RunAgent {
        /// The agent.
        agent_id: String,
    },
}

/// Limits of a tenant. The default allows everything.
#[async_trait]
pub trait Entitlements: Send + Sync {
    /// Returns `Err` with a message for the person if the action is not allowed.
    async fn check(&self, _tenant: TenantId, _action: &Action) -> Result<(), String> {
        Ok(())
    }
}

/// Entitlements that allow everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl Entitlements for AllowAll {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_defaults_allow_everything() {
        assert_eq!(
            NoHooks.on_signup("a@example.com", "1").await,
            SignupDecision::Allow
        );
        NoHooks
            .on_tenant_created(TenantId::DEFAULT, Uuid::nil())
            .await;
        NoHooks.on_tenant_deleted(TenantId::DEFAULT).await;
        let action = Action::RunAgent {
            agent_id: "brief".to_owned(),
        };
        assert!(AllowAll.check(TenantId::DEFAULT, &action).await.is_ok());
    }
}
