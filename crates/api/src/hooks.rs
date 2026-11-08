//! Extension points for programs that build their own server on these crates.
//!
//! The defaults allow everything and do nothing, which is how
//! `lighthouse-server` runs. Another binary can pass its own implementations,
//! for example to approve sign-ups by hand or to limit what a tenant may do.

use async_trait::async_trait;
use lighthouse_auth::{Invitation, Role, User};
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

/// Subject and body of a mail the server writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailText {
    /// Subject line.
    pub subject: String,
    /// Plain text body.
    pub text: String,
    /// HTML body, sent as an alternative to the text if present.
    pub html: Option<String>,
}

/// The texts of the mails the server sends about accounts. Every method has an
/// English default, so an implementation overrides only what it changes, for
/// example to translate the mails or to add its own name and footer.
#[async_trait]
pub trait MailTemplates: Send + Sync {
    /// The mail that confirms an email address. `link` opens the confirmation.
    async fn verification(&self, user: &User, link: &str) -> MailText {
        MailText {
            subject: "Confirm your email address".to_owned(),
            text: format!(
                "Hello {},\n\nplease confirm your email address for Lighthouse:\n\n{link}\n\n\
                 The link is valid for 48 hours.",
                user.name
            ),
            html: None,
        }
    }

    /// The mail that invites someone to a tenant. `link` accepts the invitation.
    async fn invitation(&self, invitation: &Invitation, tenant_name: &str, link: &str) -> MailText {
        let role = match invitation.role {
            Role::Owner => "an owner",
            Role::Admin => "an admin",
            Role::Member => "a member",
        };
        MailText {
            subject: format!("Join {tenant_name} on Lighthouse"),
            text: format!(
                "You are invited to join {tenant_name} on Lighthouse as {role}.\n\n\
                 Accept the invitation here:\n\n{link}\n\nThe link is valid for 7 days."
            ),
            html: None,
        }
    }
}

/// The English mail texts.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnglishMails;

impl MailTemplates for EnglishMails {}

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

    #[tokio::test]
    async fn english_mails_carry_the_link() {
        let now = chrono::Utc::now();
        let user = User {
            id: Uuid::nil(),
            email: "sam@example.com".to_owned(),
            name: "Sam".to_owned(),
            email_verified_at: None,
            created_at: now,
        };
        let mail = EnglishMails.verification(&user, "https://x/verify").await;
        assert!(mail.text.contains("Hello Sam") && mail.text.contains("https://x/verify"));
        for (role, named) in [
            (Role::Owner, "an owner"),
            (Role::Admin, "an admin"),
            (Role::Member, "a member"),
        ] {
            let invitation = Invitation {
                id: Uuid::nil(),
                email: "robin@example.com".to_owned(),
                role,
                invited_by: None,
                created_at: now,
                expires_at: now,
            };
            let mail = EnglishMails
                .invitation(&invitation, "Studio", "https://x/invite")
                .await;
            assert_eq!(mail.subject, "Join Studio on Lighthouse");
            assert!(mail.text.contains(named) && mail.text.contains("https://x/invite"));
        }
    }
}
