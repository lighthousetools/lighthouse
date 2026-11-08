//! The signed-in user, their tenants and the administration of a tenant.

use crate::auth::{Caller, TokenIssued, acting_user, auth_error};
use crate::{ApiError, Instance, Scoped, SignupMode};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lighthouse_auth::{
    AcceptAs, AuthError, DeviceKind, Invitation, Member, Membership, Role, Tenant,
};
use lighthouse_core::TenantId;
use lighthouse_core::agent::Delivery;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The signed-in user in their current tenant.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Me {
    /// Display name.
    pub name: String,
    /// IANA time zone of the instance, such as `Europe/Berlin`. Dates such as
    /// "today" and the days of charts follow it.
    #[serde(default)]
    pub timezone: String,
    /// User ID. Absent for the admin token.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Email address. Absent for the admin token.
    #[serde(default)]
    pub email: Option<String>,
    /// Whether the email address is confirmed.
    #[serde(default)]
    pub email_verified: bool,
    /// The tenant of the token.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Name of that tenant.
    #[serde(default)]
    pub tenant_name: String,
    /// The role there.
    #[serde(default = "owner")]
    pub role: Role,
}

fn owner() -> Role {
    Role::Owner
}

/// A change to the signed-in user.
#[derive(Deserialize, ToSchema)]
pub struct MePatch {
    /// New display name.
    pub name: String,
}

/// Confirms a deletion by repeating a name.
#[derive(Deserialize, ToSchema)]
pub struct Confirmation {
    /// For an account: its email address. For a tenant: its name.
    pub confirm: String,
}

/// A new tenant.
#[derive(Deserialize, ToSchema)]
pub struct NewTenant {
    /// Display name.
    pub name: String,
    /// Name of the device that gets a token for the new tenant.
    #[serde(default = "default_device")]
    pub device_name: String,
    /// Client kind of that device.
    #[serde(default = "default_kind")]
    pub device_kind: DeviceKind,
}

/// A request for a token in another tenant.
#[derive(Deserialize, ToSchema)]
pub struct SwitchRequest {
    /// Name of the device.
    #[serde(default = "default_device")]
    pub device_name: String,
    /// Client kind.
    #[serde(default = "default_kind")]
    pub device_kind: DeviceKind,
}

fn default_device() -> String {
    "device".to_owned()
}

fn default_kind() -> DeviceKind {
    DeviceKind::Web
}

/// Accepting an invitation as a signed-in user.
#[derive(Deserialize, ToSchema)]
pub struct AcceptInvitation {
    /// The invitation token from the link.
    #[schema(value_type = String)]
    pub token: SecretString,
    /// Name of the device that gets a token for the joined tenant.
    #[serde(default = "default_device")]
    pub device_name: String,
    /// Client kind of that device.
    #[serde(default = "default_kind")]
    pub device_kind: DeviceKind,
}

/// A change to the tenant.
#[derive(Deserialize, ToSchema)]
pub struct TenantPatch {
    /// New display name.
    pub name: String,
}

/// The current tenant with the caller's role.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct TenantInfo {
    /// The tenant.
    #[serde(flatten)]
    pub tenant: Tenant,
    /// The caller's role.
    pub role: Role,
    /// Number of members.
    pub members: usize,
}

/// A change of a member's role.
#[derive(Deserialize, ToSchema)]
pub struct RoleChange {
    /// The new role.
    pub role: Role,
}

/// A new invitation.
#[derive(Deserialize, ToSchema)]
pub struct InviteRequest {
    /// Email address to invite.
    pub email: String,
    /// Role the new member gets. Defaults to member.
    #[serde(default = "member")]
    pub role: Role,
}

fn member() -> Role {
    Role::Member
}

/// A created invitation.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct InvitationCreated {
    /// The invitation.
    pub invitation: Invitation,
    /// Whether the invitation went out by mail.
    pub mailed: bool,
    /// The accept link, only when no mail was sent. Pass it on yourself.
    pub accept_url: Option<String>,
}

fn invite_link(instance: &Instance, token: &SecretString) -> String {
    format!(
        "{}/#/invite?token={}",
        instance.public_url,
        token.expose_secret()
    )
}

/// Returns the signed-in user and their tenant.
#[utoipa::path(
    get,
    operation_id = "me",
    path = "/me",
    responses((status = 200, body = Me)),
    security(("bearer" = []))
)]
pub(crate) async fn me(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
) -> Result<Json<Me>, ApiError> {
    let user = acting_user(&instance, &caller).await.ok();
    let user = match user {
        Some(id) => instance.accounts.user(id).await.map_err(auth_error)?,
        None => None,
    };
    let tenant = instance
        .accounts
        .tenant(caller.tenant)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown tenant".to_owned()))?;
    Ok(Json(Me {
        name: user
            .as_ref()
            .map_or_else(String::new, |user| user.name.clone()),
        timezone: state.zone.name().to_owned(),
        user_id: caller.user,
        email: caller
            .user
            .and(user.as_ref().map(|user| user.email.clone())),
        email_verified: user
            .as_ref()
            .is_some_and(|user| user.email_verified_at.is_some()),
        tenant_id: tenant.id,
        tenant_name: tenant.name,
        role: caller.role,
    }))
}

/// Changes the display name of the signed-in user.
#[utoipa::path(
    patch,
    operation_id = "update_me",
    path = "/me",
    request_body = MePatch,
    responses((status = 204), (status = 400, description = "Invalid name")),
    security(("bearer" = []))
)]
pub(crate) async fn update_me(
    State(instance): State<Instance>,
    caller: Caller,
    Json(patch): Json<MePatch>,
) -> Result<StatusCode, ApiError> {
    let user = caller.require_user()?;
    instance
        .accounts
        .rename_user(user, &patch.name)
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes the account of the signed-in user. Tenants where they are the only
/// member are deleted with all their data. Repeat the email address in
/// `confirm`.
#[utoipa::path(
    delete,
    operation_id = "delete_me",
    path = "/me",
    request_body = Confirmation,
    responses(
        (status = 204),
        (status = 400, description = "The confirmation does not match"),
        (status = 409, description = "The user is the last owner of a tenant with other members")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_me(
    State(instance): State<Instance>,
    caller: Caller,
    Json(confirmation): Json<Confirmation>,
) -> Result<StatusCode, ApiError> {
    let user = caller.require_user()?;
    let account = instance
        .accounts
        .user(user)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown user".to_owned()))?;
    if !account
        .email
        .eq_ignore_ascii_case(confirmation.confirm.trim())
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "repeat the email address of the account in `confirm`".to_owned(),
        ));
    }
    let purged = instance
        .accounts
        .delete_user(user)
        .await
        .map_err(auth_error)?;
    for tenant in purged {
        instance.tenants.forget(tenant).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the tenants of the signed-in user. For the admin token, every tenant
/// of the instance, with the owner role.
#[utoipa::path(
    get,
    operation_id = "list_tenants",
    path = "/tenants",
    responses((status = 200, body = Vec<Membership>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<Json<Vec<Membership>>, ApiError> {
    if let Some(user) = caller.user {
        return Ok(Json(
            instance
                .accounts
                .tenants_of(user)
                .await
                .map_err(auth_error)?,
        ));
    }
    let mut all = Vec::new();
    for tenant in instance.accounts.all_tenants().await.map_err(auth_error)? {
        if let Some(tenant) = instance.accounts.tenant(tenant).await.map_err(auth_error)? {
            all.push(Membership {
                tenant_id: tenant.id,
                tenant_name: tenant.name,
                role: Role::Owner,
                joined_at: tenant.created_at,
            });
        }
    }
    Ok(Json(all))
}

/// Creates another tenant owned by the signed-in user, while sign-up is open,
/// and issues a token for it.
#[utoipa::path(
    post,
    operation_id = "create_tenant",
    path = "/tenants",
    request_body = NewTenant,
    responses((status = 201, body = TokenIssued), (status = 403, description = "Sign-up is not open")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    State(instance): State<Instance>,
    caller: Caller,
    Json(new): Json<NewTenant>,
) -> Result<(StatusCode, Json<TokenIssued>), ApiError> {
    let user = caller.require_user()?;
    if instance.signup != SignupMode::Open {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "new tenants need open sign-up".to_owned(),
        ));
    }
    let tenant = instance
        .accounts
        .create_tenant(user, &new.name)
        .await
        .map_err(auth_error)?;
    let issued = instance
        .accounts
        .issue(user, tenant, &new.device_name, new.device_kind)
        .await
        .map_err(auth_error)?;
    Ok((StatusCode::CREATED, Json(TokenIssued::of(&issued, user))))
}

/// Switches to another tenant of the user: issues a token bound to it. The
/// current token stays valid until it is signed out.
#[utoipa::path(
    post,
    operation_id = "switch_tenant",
    path = "/tenants/{id}/switch",
    params(("id" = Uuid, Path, description = "Tenant ID")),
    request_body = SwitchRequest,
    responses((status = 200, body = TokenIssued), (status = 403, description = "Not a member")),
    security(("bearer" = []))
)]
pub(crate) async fn switch(
    State(instance): State<Instance>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(request): Json<SwitchRequest>,
) -> Result<Json<TokenIssued>, ApiError> {
    let user = caller.require_user()?;
    let issued = instance
        .accounts
        .issue(
            user,
            TenantId::from_uuid(id),
            &request.device_name,
            request.device_kind,
        )
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued::of(&issued, user)))
}

/// Accepts an invitation as the signed-in user, whose email address must match
/// it, and issues a token for the joined tenant.
#[utoipa::path(
    post,
    operation_id = "accept_invitation_signed_in",
    path = "/tenants/accept",
    request_body = AcceptInvitation,
    responses((status = 200, body = TokenIssued),
        (status = 400, description = "Invalid token or another email address"),
        (status = 403, description = "Invitations are off")),
    security(("bearer" = []))
)]
pub(crate) async fn accept(
    State(instance): State<Instance>,
    caller: Caller,
    Json(request): Json<AcceptInvitation>,
) -> Result<Json<TokenIssued>, ApiError> {
    let user = caller.require_user()?;
    if instance.signup == SignupMode::Closed {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "invitations are off on this instance".to_owned(),
        ));
    }
    let accepted = instance
        .accounts
        .accept_invitation(request.token.expose_secret(), AcceptAs::Existing(user))
        .await
        .map_err(auth_error)?;
    let issued = instance
        .accounts
        .issue(
            user,
            accepted.tenant,
            &request.device_name,
            request.device_kind,
        )
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued::of(&issued, user)))
}

/// Returns the current tenant.
#[utoipa::path(
    get,
    operation_id = "get_tenant",
    path = "/tenant",
    responses((status = 200, body = TenantInfo)),
    security(("bearer" = []))
)]
pub(crate) async fn current(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<Json<TenantInfo>, ApiError> {
    let tenant = instance
        .accounts
        .tenant(caller.tenant)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown tenant".to_owned()))?;
    let members = instance
        .accounts
        .members(caller.tenant)
        .await
        .map_err(auth_error)?
        .len();
    Ok(Json(TenantInfo {
        tenant,
        role: caller.role,
        members,
    }))
}

/// Renames the current tenant. Needs the owner or admin role.
#[utoipa::path(
    patch,
    operation_id = "update_tenant",
    path = "/tenant",
    request_body = TenantPatch,
    responses((status = 200, body = Tenant), (status = 403, description = "Needs the owner or admin role")),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    State(instance): State<Instance>,
    caller: Caller,
    Json(patch): Json<TenantPatch>,
) -> Result<Json<Tenant>, ApiError> {
    caller.require_manager()?;
    Ok(Json(
        instance
            .accounts
            .rename_tenant(caller.tenant, &patch.name)
            .await
            .map_err(auth_error)?,
    ))
}

/// Deletes the current tenant with all its data. Needs the owner role; repeat
/// the tenant name in `confirm`. The default tenant cannot be deleted.
#[utoipa::path(
    delete,
    operation_id = "delete_tenant",
    path = "/tenant",
    request_body = Confirmation,
    responses(
        (status = 204),
        (status = 400, description = "The confirmation does not match"),
        (status = 403, description = "Needs the owner role"),
        (status = 409, description = "The default tenant stays")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    State(instance): State<Instance>,
    caller: Caller,
    Json(confirmation): Json<Confirmation>,
) -> Result<StatusCode, ApiError> {
    caller.require_owner()?;
    let tenant = instance
        .accounts
        .tenant(caller.tenant)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown tenant".to_owned()))?;
    if tenant.name != confirmation.confirm.trim() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "repeat the name of the tenant in `confirm`".to_owned(),
        ));
    }
    instance
        .accounts
        .delete_tenant(caller.tenant)
        .await
        .map_err(auth_error)?;
    instance.tenants.forget(caller.tenant).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the members of the current tenant.
#[utoipa::path(
    get,
    operation_id = "list_members",
    path = "/tenant/members",
    responses((status = 200, body = Vec<Member>)),
    security(("bearer" = []))
)]
pub(crate) async fn members(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<Json<Vec<Member>>, ApiError> {
    Ok(Json(
        instance
            .accounts
            .members(caller.tenant)
            .await
            .map_err(auth_error)?,
    ))
}

async fn role_of(instance: &Instance, tenant: TenantId, user: Uuid) -> Result<Role, ApiError> {
    instance
        .accounts
        .role(tenant, user)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown member".to_owned()))
}

fn owners_only() -> ApiError {
    ApiError(
        StatusCode::FORBIDDEN,
        "only owners change owners".to_owned(),
    )
}

/// Changes the role of a member. Needs the owner or admin role; only owners
/// grant or take away the owner role. The last owner keeps it.
#[utoipa::path(
    patch,
    path = "/tenant/members/{user}",
    params(("user" = Uuid, Path, description = "User ID")),
    request_body = RoleChange,
    responses((status = 204), (status = 403, description = "Not allowed"),
        (status = 404, description = "Unknown member"), (status = 409, description = "Last owner")),
    security(("bearer" = []))
)]
pub(crate) async fn change_role(
    State(instance): State<Instance>,
    caller: Caller,
    Path(user): Path<Uuid>,
    Json(change): Json<RoleChange>,
) -> Result<StatusCode, ApiError> {
    caller.require_manager()?;
    let current = role_of(&instance, caller.tenant, user).await?;
    if (current == Role::Owner || change.role == Role::Owner) && caller.role != Role::Owner {
        return Err(owners_only());
    }
    instance
        .accounts
        .set_role(caller.tenant, user, change.role)
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a member, or lets the caller leave. Removing others needs the owner
/// or admin role, and only owners remove owners. The last owner cannot leave.
#[utoipa::path(
    delete,
    path = "/tenant/members/{user}",
    params(("user" = Uuid, Path, description = "User ID")),
    responses((status = 204), (status = 403, description = "Not allowed"),
        (status = 404, description = "Unknown member"), (status = 409, description = "Last owner")),
    security(("bearer" = []))
)]
pub(crate) async fn remove_member(
    State(instance): State<Instance>,
    caller: Caller,
    Path(user): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let current = role_of(&instance, caller.tenant, user).await?;
    if caller.user != Some(user) {
        caller.require_manager()?;
        if current == Role::Owner && caller.role != Role::Owner {
            return Err(owners_only());
        }
    }
    instance
        .accounts
        .remove_member(caller.tenant, user)
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the open invitations of the current tenant. Needs the owner or admin role.
#[utoipa::path(
    get,
    operation_id = "list_invitations",
    path = "/tenant/invitations",
    responses((status = 200, body = Vec<Invitation>), (status = 403, description = "Not allowed")),
    security(("bearer" = []))
)]
pub(crate) async fn invitations(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<Json<Vec<Invitation>>, ApiError> {
    caller.require_manager()?;
    Ok(Json(
        instance
            .accounts
            .invitations(caller.tenant)
            .await
            .map_err(auth_error)?,
    ))
}

/// Invites someone to the current tenant. Needs the owner or admin role; only
/// owners invite owners. With mail configured, the invitation goes out by
/// mail; otherwise the answer carries the accept link to pass on.
#[utoipa::path(
    post,
    path = "/tenant/invitations",
    request_body = InviteRequest,
    responses((status = 201, body = InvitationCreated), (status = 400, description = "Invalid email"),
        (status = 403, description = "Not allowed or invitations are off"),
        (status = 409, description = "Already a member")),
    security(("bearer" = []))
)]
pub(crate) async fn invite(
    State(instance): State<Instance>,
    caller: Caller,
    Json(request): Json<InviteRequest>,
) -> Result<(StatusCode, Json<InvitationCreated>), ApiError> {
    caller.require_manager()?;
    if instance.signup == SignupMode::Closed {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "invitations are off on this instance; set tenancy.signup to invite or open".to_owned(),
        ));
    }
    if request.role == Role::Owner && caller.role != Role::Owner {
        return Err(owners_only());
    }
    if let Some(existing) = instance
        .accounts
        .user_by_email(&request.email)
        .await
        .map_err(auth_error)?
        && instance
            .accounts
            .role(caller.tenant, existing.id)
            .await
            .map_err(auth_error)?
            .is_some()
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this person is a member already".to_owned(),
        ));
    }
    let issued = instance
        .accounts
        .invite(caller.tenant, &request.email, request.role, caller.user)
        .await
        .map_err(auth_error)?;
    let link = invite_link(&instance, &issued.token);
    let tenant_name = instance
        .accounts
        .tenant(caller.tenant)
        .await
        .map_err(auth_error)?
        .map_or_else(String::new, |tenant| tenant.name);
    let mut mailed = false;
    if let Some(mailer) = &instance.mailer {
        let body = format!(
            "You are invited to join {tenant_name} on Lighthouse.\n\n\
             Accept the invitation here:\n\n{link}\n\nThe link is valid for 7 days."
        );
        let target = Delivery::Mail(issued.invitation.email.clone());
        match mailer
            .deliver(&target, &format!("Join {tenant_name} on Lighthouse"), &body)
            .await
        {
            Ok(()) => mailed = true,
            Err(error) => tracing::warn!(%error, "cannot mail the invitation"),
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(InvitationCreated {
            invitation: issued.invitation,
            mailed,
            accept_url: (!mailed).then_some(link),
        }),
    ))
}

/// Revokes an open invitation. Needs the owner or admin role.
#[utoipa::path(
    delete,
    path = "/tenant/invitations/{id}",
    params(("id" = Uuid, Path, description = "Invitation ID")),
    responses((status = 204), (status = 403, description = "Not allowed"),
        (status = 404, description = "Unknown invitation")),
    security(("bearer" = []))
)]
pub(crate) async fn revoke_invitation(
    State(instance): State<Instance>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    caller.require_manager()?;
    if instance
        .accounts
        .revoke_invitation(caller.tenant, id)
        .await
        .map_err(auth_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(auth_error(AuthError::NotFound("invitation")))
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{OWNER_EMAIL, TOKEN, build_harness_with, call, send, signed_in};
    use crate::{SignupMode, TokenIssued};
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use lighthouse_core::agent::Delivery;
    use lighthouse_notify::{Notifier, NotifyError};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    const PASSWORD: &str = "a long enough password";

    #[derive(Default)]
    struct Outbox(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl Notifier for Outbox {
        async fn deliver(
            &self,
            target: &Delivery,
            _subject: &str,
            markdown: &str,
        ) -> Result<(), NotifyError> {
            let Delivery::Mail(address) = target else {
                panic!("account mails go by mail");
            };
            self.0
                .lock()
                .unwrap()
                .push((address.clone(), markdown.to_owned()));
            Ok(())
        }
    }

    fn link_token(body: &str) -> String {
        body.split("token=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    fn json_of(body: &str) -> Value {
        serde_json::from_str(body).unwrap()
    }

    async fn sign_up(app: &crate::Router, email: &str, tenant: &str) -> TokenIssued {
        let (status, body) = send(
            app,
            "POST",
            "/auth/signup",
            None,
            Some(json!({"email": email, "name": "Sam", "password": PASSWORD,
                        "tenant_name": tenant, "device_name": "web", "device_kind": "web"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn signup_is_closed_by_default_and_open_signups_get_their_own_tenant() {
        let closed = build_harness_with(Some(TOKEN), None, SignupMode::Closed, None).await;
        let request = json!({"email": "sam@example.com", "name": "Sam", "password": PASSWORD,
                             "device_name": "web", "device_kind": "web"});
        let (status, _) = send(&closed.app, "POST", "/auth/signup", None, Some(request)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let outbox = Arc::new(Outbox::default());
        let open =
            build_harness_with(Some(TOKEN), None, SignupMode::Open, Some(outbox.clone())).await;
        let (_, body) = call(&open.app, "GET", "/auth/status", None).await;
        assert_eq!(json_of(&body)["signup"], "open");
        assert_eq!(json_of(&body)["setup_required"], false);
        let sam = sign_up(&open.app, "sam@example.com", "Sam's studio").await;
        let (_, body) = call(&open.app, "GET", "/me", Some(&sam.token)).await;
        let me = json_of(&body);
        assert_eq!(me["email"], "sam@example.com");
        assert_eq!(me["tenant_name"], "Sam's studio");
        assert_eq!(me["role"], "owner");
        assert_eq!(me["email_verified"], false);

        let mail = outbox.0.lock().unwrap().pop().unwrap();
        assert_eq!(mail.0, "sam@example.com");
        let token = link_token(&mail.1);
        let (status, _) = send(
            &open.app,
            "POST",
            "/auth/verify",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&open.app, "GET", "/me", Some(&sam.token)).await;
        assert_eq!(json_of(&body)["email_verified"], true);
        assert_eq!(
            call(&open.app, "POST", "/auth/verify/resend", Some(&sam.token))
                .await
                .0,
            StatusCode::NO_CONTENT
        );

        let (_, body) = call(&open.app, "GET", "/auth/status", None).await;
        assert_eq!(json_of(&body)["email_required"], true);
        let attempt = json!({"password": PASSWORD, "device_name": "x", "device_kind": "web"});
        let (status, _) = send(&open.app, "POST", "/auth/login", None, Some(attempt)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "several users need an email"
        );
        let attempt = json!({"email": "SAM@example.com", "password": PASSWORD,
                             "device_name": "x", "device_kind": "web"});
        let (status, body) = send(&open.app, "POST", "/auth/login", None, Some(attempt)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["tenant_id"], json!(sam.tenant_id));
    }

    #[tokio::test]
    async fn tenants_do_not_see_each_others_data() {
        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Open, None).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let sam = sign_up(app, "sam@example.com", "Studio").await;
        for (token, title) in [(&owner, "Owner task"), (&sam.token, "Studio task")] {
            let (status, _) = send(
                app,
                "POST",
                "/tasks",
                Some(token),
                Some(json!({"title": title})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
        }
        let (_, owner_tasks) = call(app, "GET", "/tasks?view=all", Some(&owner)).await;
        let (_, sam_tasks) = call(app, "GET", "/tasks?view=all", Some(&sam.token)).await;
        assert!(owner_tasks.contains("Owner task") && !owner_tasks.contains("Studio task"));
        assert!(sam_tasks.contains("Studio task") && !sam_tasks.contains("Owner task"));
        let id = json_of(&sam_tasks).as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            call(app, "GET", &format!("/tasks/{id}"), Some(&owner))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", &format!("/tasks/{id}"), Some(&owner))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = send(app, "GET", "/tasks?view=all", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains("Studio task"),
            "the admin token works in the default tenant"
        );
        let admin_in_studio = admin_get(app, "/tasks?view=all", sam.tenant_id).await;
        assert!(admin_in_studio.contains("Studio task"));
    }

    async fn admin_call(app: &crate::Router, uri: &str, tenant: &str) -> (StatusCode, String) {
        use axum::body::{Body, to_bytes};
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header(crate::TENANT_HEADER, tenant)
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn admin_get(app: &crate::Router, uri: &str, tenant: uuid::Uuid) -> String {
        let (status, body) = admin_call(app, uri, &tenant.to_string()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            admin_call(app, uri, "nope").await.0,
            StatusCode::BAD_REQUEST
        );
        let unknown = uuid::Uuid::now_v7().to_string();
        assert_eq!(
            admin_call(app, uri, &unknown).await.0,
            StatusCode::NOT_FOUND
        );
        body
    }

    async fn invite(
        app: &crate::Router,
        token: &str,
        email: &str,
        role: &str,
    ) -> (StatusCode, Value) {
        let (status, body) = send(
            app,
            "POST",
            "/tenant/invitations",
            Some(token),
            Some(json!({"email": email, "role": role})),
        )
        .await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn accept_new(app: &crate::Router, url: &str) -> TokenIssued {
        let token = link_token(url);
        let (status, body) = send(
            app,
            "POST",
            "/auth/invitation",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["account_exists"], false);
        let (status, body) = send(
            app,
            "POST",
            "/auth/accept",
            None,
            Some(
                json!({"token": token, "name": "Robin", "password": PASSWORD,
                        "device_name": "web", "device_kind": "web"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn invitations_roles_and_leaving() {
        let closed = build_harness_with(Some(TOKEN), None, SignupMode::Closed, None).await;
        let owner = signed_in(&closed.app).await;
        assert_eq!(
            invite(&closed.app, &owner, "robin@example.com", "member")
                .await
                .0,
            StatusCode::FORBIDDEN,
            "closed instances invite nobody"
        );

        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Invite, None).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let (status, created) = invite(app, &owner, "robin@example.com", "member").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["mailed"], false);
        let robin = accept_new(app, created["accept_url"].as_str().unwrap()).await;
        assert_eq!(
            invite(app, &owner, "robin@example.com", "member").await.0,
            StatusCode::CONFLICT
        );

        // A member uses the tenant but manages nothing.
        assert_eq!(
            invite(app, &robin.token, "x@example.com", "member").await.0,
            StatusCode::FORBIDDEN
        );
        let key = json!({"value": "secret-key"});
        let (status, _) = send(
            app,
            "PUT",
            "/credentials/anthropic",
            Some(&robin.token),
            Some(key.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(
            app,
            "PUT",
            "/credentials/anthropic",
            Some(&owner),
            Some(key),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            call(app, "DELETE", "/credentials/anthropic", Some(&robin.token))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, _) = send(
            app,
            "PATCH",
            "/tenant",
            Some(&robin.token),
            Some(json!({"name": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            call(app, "GET", "/tenant/invitations", Some(&robin.token))
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        let (_, body) = call(app, "GET", "/tenant/members", Some(&robin.token)).await;
        let members = json_of(&body);
        assert_eq!(members.as_array().unwrap().len(), 2);
        let owner_id = members[0]["user_id"].as_str().unwrap().to_owned();
        let robin_id = robin.user_id.to_string();

        // An admin manages members but not owners.
        let promote = |role: &str| json!({"role": role});
        let uri = format!("/tenant/members/{robin_id}");
        assert_eq!(
            send(
                app,
                "PATCH",
                &uri,
                Some(&robin.token),
                Some(promote("admin"))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(app, "PATCH", &uri, Some(&owner), Some(promote("admin")))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let owner_uri = format!("/tenant/members/{owner_id}");
        assert_eq!(
            send(
                app,
                "PATCH",
                &owner_uri,
                Some(&robin.token),
                Some(promote("member"))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                app,
                "PATCH",
                &uri,
                Some(&robin.token),
                Some(promote("owner"))
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(app, "DELETE", &owner_uri, Some(&robin.token)).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, created) = invite(app, &robin.token, "kim@example.com", "member").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            invite(app, &robin.token, "kim@example.com", "owner")
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (_, open) = call(app, "GET", "/tenant/invitations", Some(&robin.token)).await;
        assert!(open.contains("kim@example.com"));
        let id = created["invitation"]["id"].as_str().unwrap();
        let revoke = format!("/tenant/invitations/{id}");
        assert_eq!(
            call(app, "DELETE", &revoke, Some(&robin.token)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", &revoke, Some(&robin.token)).await.0,
            StatusCode::NOT_FOUND
        );

        // The last owner stays; others may leave.
        assert_eq!(
            send(
                app,
                "PATCH",
                &owner_uri,
                Some(&owner),
                Some(promote("member"))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(app, "DELETE", &owner_uri, Some(&owner)).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(&robin.token)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "GET", "/me", Some(&robin.token)).await.0,
            StatusCode::UNAUTHORIZED
        );
        let unknown = format!("/tenant/members/{}", uuid::Uuid::now_v7());
        assert_eq!(
            call(app, "DELETE", &unknown, Some(&owner)).await.0,
            StatusCode::NOT_FOUND
        );
        let (_, body) = call(app, "GET", "/tenant", Some(&owner)).await;
        assert_eq!(json_of(&body)["members"], 1);
        let (status, body) = send(
            app,
            "PATCH",
            "/tenant",
            Some(&owner),
            Some(json!({"name": "Home"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_of(&body)["name"], "Home");
    }

    #[tokio::test]
    async fn invitations_go_out_by_mail_when_mail_is_configured() {
        let outbox = Arc::new(Outbox::default());
        let harness =
            build_harness_with(Some(TOKEN), None, SignupMode::Invite, Some(outbox.clone())).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let (status, created) = invite(app, &owner, "robin@example.com", "admin").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["mailed"], true);
        assert!(created["accept_url"].is_null());
        let mail = outbox.0.lock().unwrap().pop().unwrap();
        assert_eq!(mail.0, "robin@example.com");
        let robin = accept_new(app, &mail.1).await;
        let (_, body) = call(app, "GET", "/me", Some(&robin.token)).await;
        assert_eq!(json_of(&body)["role"], "admin");
    }

    #[tokio::test]
    async fn a_user_switches_between_tenants() {
        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Open, None).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let sam = sign_up(app, "sam@example.com", "Studio").await;
        let (_, created) = invite(app, &owner, "sam@example.com", "member").await;
        let token = link_token(created["accept_url"].as_str().unwrap());
        let (_, body) = send(
            app,
            "POST",
            "/auth/invitation",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(json_of(&body)["account_exists"], true);
        let (status, body) = send(
            app,
            "POST",
            "/tenants/accept",
            Some(&sam.token),
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let joined: TokenIssued = serde_json::from_str(&body).unwrap();
        let (_, body) = call(app, "GET", "/tenants", Some(&sam.token)).await;
        assert_eq!(json_of(&body).as_array().unwrap().len(), 2);
        let (_, body) = call(app, "GET", "/me", Some(&joined.token)).await;
        assert_eq!(json_of(&body)["tenant_name"], "default");
        assert_eq!(json_of(&body)["role"], "member");

        let switch = format!("/tenants/{}/switch", sam.tenant_id);
        let (status, body) = send(
            app,
            "POST",
            &switch,
            Some(&joined.token),
            Some(json!({"device_name": "web"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let back: TokenIssued = serde_json::from_str(&body).unwrap();
        assert_eq!(back.tenant_id, sam.tenant_id);
        let (status, _) = send(app, "POST", &switch, Some(&owner), Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "only members switch");

        let (status, body) = send(
            app,
            "POST",
            "/tenants",
            Some(&sam.token),
            Some(json!({"name": "Side"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let side: TokenIssued = serde_json::from_str(&body).unwrap();
        let (_, body) = call(app, "GET", "/tenant", Some(&side.token)).await;
        assert_eq!(json_of(&body)["name"], "Side");
        let (_, body) = call(app, "GET", "/tenants", Some(TOKEN)).await;
        assert_eq!(
            json_of(&body).as_array().unwrap().len(),
            3,
            "the admin token sees every tenant"
        );
        let (_, body) = call(app, "GET", "/auth/devices", Some(&sam.token)).await;
        assert!(json_of(&body).as_array().unwrap().len() >= 4);
    }

    #[tokio::test]
    async fn owners_delete_tenants_and_users_delete_their_account() {
        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Open, None).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let sam = sign_up(app, "sam@example.com", "Studio").await;
        let wrong = json!({"confirm": "Not the name"});
        assert_eq!(
            send(app, "DELETE", "/tenant", Some(&sam.token), Some(wrong))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let default = json!({"confirm": "default"});
        assert_eq!(
            send(app, "DELETE", "/tenant", Some(&owner), Some(default))
                .await
                .0,
            StatusCode::CONFLICT
        );
        let confirm = json!({"confirm": "Studio"});
        assert_eq!(
            send(app, "DELETE", "/tenant", Some(&sam.token), Some(confirm))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            *harness.tenants.forgotten.lock().unwrap(),
            [lighthouse_core::TenantId::from_uuid(sam.tenant_id)]
        );
        assert_eq!(
            call(app, "GET", "/me", Some(&sam.token)).await.0,
            StatusCode::UNAUTHORIZED
        );

        let other = sign_up(app, "kim@example.com", "Kim").await;
        let (status, _) = send(
            app,
            "PATCH",
            "/me",
            Some(&other.token),
            Some(json!({"name": "Kimberly"})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(app, "GET", "/me", Some(&other.token)).await;
        assert_eq!(json_of(&body)["name"], "Kimberly");
        let (status, _) = send(
            app,
            "DELETE",
            "/me",
            Some(&other.token),
            Some(json!({"confirm": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            app,
            "DELETE",
            "/me",
            Some(&other.token),
            Some(json!({"confirm": "KIM@example.com"})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(harness.tenants.forgotten.lock().unwrap().len(), 2);
        assert!(
            harness
                .accounts
                .user_by_email("kim@example.com")
                .await
                .unwrap()
                .is_none()
        );
        let (status, _) = send(
            app,
            "DELETE",
            "/me",
            Some(&owner),
            Some(json!({"confirm": OWNER_EMAIL})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "the default tenant keeps its owner"
        );
        let (status, _) = send(
            app,
            "DELETE",
            "/me",
            Some(TOKEN),
            Some(json!({"confirm": OWNER_EMAIL})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the admin token has no account"
        );
    }

    #[tokio::test]
    async fn the_password_changes_and_signs_out_other_devices() {
        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Closed, None).await;
        let app = &harness.app;
        let first = signed_in(app).await;
        let attempt = json!({"email": OWNER_EMAIL, "password": PASSWORD, "device_name": "two", "device_kind": "mac"});
        let (_, body) = send(app, "POST", "/auth/login", None, Some(attempt)).await;
        let second: TokenIssued = serde_json::from_str(&body).unwrap();
        let change = json!({"current": "wrong password!", "new": "a brand new password"});
        assert_eq!(
            send(app, "POST", "/auth/password", Some(&first), Some(change))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let change = json!({"current": PASSWORD, "new": "short"});
        assert_eq!(
            send(app, "POST", "/auth/password", Some(&first), Some(change))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let change = json!({"current": PASSWORD, "new": "a brand new password"});
        assert_eq!(
            send(app, "POST", "/auth/password", Some(&first), Some(change))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "GET", "/me", Some(&first)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "GET", "/me", Some(&second.token)).await.0,
            StatusCode::UNAUTHORIZED
        );
        let change = json!({"current": PASSWORD, "new": "a brand new password"});
        assert_eq!(
            send(app, "POST", "/auth/password", Some(TOKEN), Some(change))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let (_, body) = call(app, "GET", "/me", Some(TOKEN)).await;
        assert_eq!(json_of(&body)["name"], "Alex");
        assert!(json_of(&body)["email"].is_null());
        assert_eq!(
            call(app, "POST", "/auth/verify/resend", Some(&first))
                .await
                .0,
            StatusCode::CONFLICT,
            "no mail without SMTP"
        );
        let invalid = json!({"token": "lhv_nothing"});
        assert_eq!(
            send(app, "POST", "/auth/verify", None, Some(invalid))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn personal_data_stays_with_its_user_and_shared_data_is_shared() {
        let harness = build_harness_with(Some(TOKEN), None, SignupMode::Invite, None).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let (_, created) = invite(app, &owner, "robin@example.com", "member").await;
        let robin = accept_new(app, created["accept_url"].as_str().unwrap()).await;
        let (status, body) = send(
            app,
            "POST",
            "/talk/threads",
            Some(&owner),
            Some(json!({"title": "Private"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let thread = json_of(&body)["id"].as_str().unwrap().to_owned();
        let (status, _) = send(
            app,
            "POST",
            "/tasks",
            Some(&owner),
            Some(json!({"title": "Shared task"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (_, threads) = call(app, "GET", "/talk/threads", Some(&robin.token)).await;
        assert!(!threads.contains("Private"));
        assert_eq!(
            call(
                app,
                "GET",
                &format!("/talk/threads/{thread}"),
                Some(&robin.token)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (_, threads) = call(app, "GET", "/talk/threads", Some(&owner)).await;
        assert!(threads.contains("Private"));
        let (_, tasks) = call(app, "GET", "/tasks?view=all", Some(&robin.token)).await;
        assert!(tasks.contains("Shared task"));
    }
}
