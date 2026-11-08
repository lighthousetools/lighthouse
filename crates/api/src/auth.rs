//! Sign-in, sign-up, devices and credentials.

use crate::hooks::SignupDecision;
use crate::{ApiError, Instance, Scoped, SignupMode, bearer_token, internal};
use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use lighthouse_auth::{
    AcceptAs, AuthError, Credentials, Device, DeviceKind, IssuedToken, NewAccount, Role, User,
};
use lighthouse_core::TenantId;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

/// Header with which the admin token picks a tenant other than the default one.
pub const TENANT_HEADER: &str = "x-lighthouse-tenant";

/// Who is behind the current request.
///
/// A device token resolves to a user and the tenant it is bound to. The admin
/// token has no user; it acts as an owner in the default tenant or in the
/// tenant named by the `X-Lighthouse-Tenant` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The user, absent for the admin token.
    pub user: Option<Uuid>,
    /// The tenant of the request.
    pub tenant: TenantId,
    /// The role in that tenant.
    pub role: Role,
    /// The device, absent for the admin token.
    pub device: Option<Uuid>,
    /// Whether the device is a desktop agent.
    pub node: bool,
}

impl Caller {
    /// Fails unless the caller may manage members, settings and keys.
    pub(crate) fn require_manager(&self) -> Result<(), ApiError> {
        if self.role.can_manage() {
            Ok(())
        } else {
            Err(ApiError(
                StatusCode::FORBIDDEN,
                "this needs the owner or admin role".to_owned(),
            ))
        }
    }

    /// Fails unless the caller is an owner.
    pub(crate) fn require_owner(&self) -> Result<(), ApiError> {
        if self.role == Role::Owner {
            Ok(())
        } else {
            Err(ApiError(
                StatusCode::FORBIDDEN,
                "this needs the owner role".to_owned(),
            ))
        }
    }

    /// Fails for the admin token, which has no user.
    pub(crate) fn require_user(&self) -> Result<Uuid, ApiError> {
        self.user.ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "this needs the token of a user, not the admin token".to_owned(),
            )
        })
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Response> {
        parts
            .extensions
            .get::<Self>()
            .copied()
            .ok_or_else(unauthorized)
    }
}

/// The address of the client, for rate limits. `X-Forwarded-For` counts only
/// when the configuration trusts it.
pub(crate) struct ClientAddress(pub String);

impl FromRequestParts<Instance> for ClientAddress {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Instance) -> Result<Self, ApiError> {
        if state.trust_forwarded_for
            && let Some(forwarded) = parts
                .headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            return Ok(Self(forwarded.to_owned()));
        }
        let address = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(|| "unknown".to_owned(), |info| info.0.ip().to_string());
        Ok(Self(address))
    }
}

pub(crate) fn is_admin(instance: &Instance, provided: &str) -> bool {
    instance.admin_token.as_ref().is_some_and(|expected| {
        bool::from(
            expected
                .expose_secret()
                .as_bytes()
                .ct_eq(provided.as_bytes()),
        )
    })
}

fn admin_tenant(request: &Request) -> Result<TenantId, ApiError> {
    match request.headers().get(TENANT_HEADER) {
        None => Ok(TenantId::DEFAULT),
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            .map(TenantId::from_uuid)
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "invalid tenant header".to_owned())),
    }
}

/// Resolves the caller and the state of their tenant, or refuses the request.
pub(crate) async fn require_auth(
    State(instance): State<Instance>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(provided) = bearer_token(&request).map(str::to_owned) else {
        return unauthorized();
    };
    let caller = if is_admin(&instance, &provided) {
        let tenant = match admin_tenant(&request) {
            Ok(tenant) => tenant,
            Err(error) => return error.into_response(),
        };
        Caller {
            user: None,
            tenant,
            role: Role::Owner,
            device: None,
            node: false,
        }
    } else {
        match instance.accounts.verify(&provided).await {
            Ok(Some(identity)) => Caller {
                user: Some(identity.user),
                tenant: identity.tenant,
                role: identity.role,
                device: Some(identity.device),
                node: identity.node,
            },
            Ok(None) => return unauthorized(),
            Err(error) => return internal(error).into_response(),
        }
    };
    let state = match instance.tenants.state(caller.tenant).await {
        Ok(Some(state)) => state,
        Ok(None) => {
            return ApiError(StatusCode::NOT_FOUND, "unknown tenant".to_owned()).into_response();
        }
        Err(error) => {
            tracing::error!(tenant = %caller.tenant.as_uuid(), %error, "cannot open the tenant");
            return ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "the tenant is not available".to_owned(),
            )
            .into_response();
        }
    };
    request.extensions_mut().insert(state.for_user(caller.user));
    request.extensions_mut().insert(caller);
    lighthouse_audit::acting_user::scope(caller.user, next.run(request)).await
}

/// Lets a request through with the admin token only.
pub(crate) async fn require_admin(
    State(instance): State<Instance>,
    request: Request,
    next: Next,
) -> Response {
    let valid = bearer_token(&request).is_some_and(|token| is_admin(&instance, token));
    if valid {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "admin token required".to_owned()).into_response()
    }
}

fn unauthorized() -> Response {
    ApiError(StatusCode::UNAUTHORIZED, "sign-in required".to_owned()).into_response()
}

pub(crate) fn auth_error(error: AuthError) -> ApiError {
    let status = match &error {
        AuthError::WeakPassword
        | AuthError::InvalidEmail
        | AuthError::Invalid(_)
        | AuthError::EmailRequired
        | AuthError::InvalidToken => StatusCode::BAD_REQUEST,
        AuthError::WrongPassword => StatusCode::UNAUTHORIZED,
        AuthError::LockedOut => StatusCode::TOO_MANY_REQUESTS,
        AuthError::EmailTaken
        | AuthError::LastOwner
        | AuthError::DefaultTenant
        | AuthError::AlreadyMember => StatusCode::CONFLICT,
        AuthError::NotMember => StatusCode::FORBIDDEN,
        AuthError::NotFound(_) => StatusCode::NOT_FOUND,
        AuthError::Database(_) | AuthError::Store(_) | AuthError::Hash => {
            return internal(error);
        }
    };
    ApiError(status, error.to_string())
}

/// State of the sign-in.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AuthStatus {
    /// Whether an administrator still has to set the first password with
    /// `POST /auth/setup`. Never true while sign-up is open.
    pub setup_required: bool,
    /// Who may create an account: `closed`, `invite` or `open`.
    pub signup: SignupMode,
    /// Whether `POST /auth/login` needs an email address because the instance
    /// has several users.
    pub email_required: bool,
}

/// A new password set by an administrator.
#[derive(Deserialize, ToSchema)]
pub struct SetupRequest {
    /// The password, at least 12 characters.
    #[schema(value_type = String)]
    pub password: SecretString,
}

/// A sign-in attempt.
#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Email address. May be left out while the instance has exactly one user.
    #[serde(default)]
    pub email: Option<String>,
    /// The password.
    #[schema(value_type = String)]
    pub password: SecretString,
    /// Name of this device.
    pub device_name: String,
    /// Client kind.
    pub device_kind: DeviceKind,
    /// Tenant to sign in to. Defaults to the user's oldest membership.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
}

/// A request for a new account with its own tenant.
#[derive(Deserialize, ToSchema)]
pub struct SignupRequest {
    /// Email address.
    pub email: String,
    /// Display name.
    pub name: String,
    /// The password, at least 12 characters.
    #[schema(value_type = String)]
    pub password: SecretString,
    /// Name of the new tenant. Defaults to the display name.
    #[serde(default)]
    pub tenant_name: Option<String>,
    /// Name of this device.
    pub device_name: String,
    /// Client kind.
    pub device_kind: DeviceKind,
}

/// A request for a token without a password.
#[derive(Deserialize, ToSchema)]
pub struct TokenRequest {
    /// Name of the token.
    pub name: String,
    /// Client kind.
    pub kind: DeviceKind,
}

/// A device token. It is shown only once.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct TokenIssued {
    /// Device ID.
    pub id: Uuid,
    /// The bearer token.
    pub token: String,
    /// Tenant the token is bound to.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// User the token belongs to.
    #[serde(default)]
    pub user_id: Uuid,
}

impl TokenIssued {
    pub(crate) fn of(issued: &IssuedToken, user: Uuid) -> Self {
        Self {
            id: issued.id,
            token: issued.token.expose_secret().to_owned(),
            tenant_id: issued.tenant.as_uuid(),
            user_id: user,
        }
    }
}

/// A password change.
#[derive(Deserialize, ToSchema)]
pub struct PasswordChange {
    /// The current password.
    #[schema(value_type = String)]
    pub current: SecretString,
    /// The new password, at least 12 characters.
    #[schema(value_type = String)]
    pub new: SecretString,
}

/// A one-time token from a link, such as an email verification.
#[derive(Deserialize, ToSchema)]
pub struct LinkToken {
    /// The token.
    #[schema(value_type = String)]
    pub token: SecretString,
}

/// A credential to store.
#[derive(Deserialize, ToSchema)]
pub struct CredentialValue {
    /// The secret value.
    #[schema(value_type = String)]
    pub value: SecretString,
}

/// Reports how people sign in to this instance.
#[utoipa::path(get, operation_id = "auth_status",
    path = "/auth/status", responses((status = 200, body = AuthStatus)))]
pub(crate) async fn status(State(instance): State<Instance>) -> Result<Json<AuthStatus>, ApiError> {
    let set_up = instance.accounts.is_set_up().await.map_err(auth_error)?;
    let users = instance.accounts.user_count().await.map_err(auth_error)?;
    Ok(Json(AuthStatus {
        setup_required: !set_up && instance.signup != SignupMode::Open,
        signup: instance.signup,
        email_required: users > 1,
    }))
}

/// Sets the password of the first owner of the default tenant. This signs out
/// all their devices.
#[utoipa::path(
    post,
    path = "/auth/setup",
    request_body = SetupRequest,
    responses((status = 204), (status = 400, description = "Password too short"),
        (status = 404, description = "The default tenant has no owner")),
    security(("admin_token" = []))
)]
pub(crate) async fn setup(
    State(instance): State<Instance>,
    Json(request): Json<SetupRequest>,
) -> Result<StatusCode, ApiError> {
    let owner = instance
        .accounts
        .primary_user(TenantId::DEFAULT)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no owner to set up".to_owned()))?;
    instance
        .accounts
        .set_password(owner.id, &request.password)
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Signs in a new device and binds its token to one tenant.
#[utoipa::path(
    post,
    path = "/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 200, body = TokenIssued),
        (status = 400, description = "Email address required"),
        (status = 401, description = "Wrong email or password"),
        (status = 403, description = "Not a member of the requested tenant"),
        (status = 429, description = "Too many failed attempts")
    )
)]
pub(crate) async fn login(
    State(instance): State<Instance>,
    ClientAddress(client): ClientAddress,
    Json(request): Json<LoginRequest>,
) -> Result<Json<TokenIssued>, ApiError> {
    let user = instance
        .accounts
        .authenticate(Credentials {
            email: request
                .email
                .as_deref()
                .filter(|email| !email.trim().is_empty()),
            password: &request.password,
            client: &client,
        })
        .await
        .map_err(auth_error)?;
    let tenant = match request.tenant_id {
        Some(tenant) => TenantId::from_uuid(tenant),
        None => instance
            .accounts
            .tenants_of(user.id)
            .await
            .map_err(auth_error)?
            .first()
            .map(|membership| TenantId::from_uuid(membership.tenant_id))
            .ok_or_else(|| auth_error(AuthError::NotMember))?,
    };
    let issued = instance
        .accounts
        .issue(user.id, tenant, &request.device_name, request.device_kind)
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued::of(&issued, user.id)))
}

/// Sends the link that confirms an email address, if mail is configured.
pub(crate) async fn send_verification(instance: &Instance, user: &User) {
    if instance.mailer.is_none() {
        return;
    }
    let token = match instance.accounts.email_verification(user.id).await {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(%error, "cannot create an email verification");
            return;
        }
    };
    let link = format!(
        "{}/#/verify?token={}",
        instance.public_url,
        token.expose_secret()
    );
    let text = instance.mail_templates.verification(user, &link).await;
    instance.send_mail(&user.email, text).await;
}

/// Creates an account with its own tenant, while sign-up is open.
#[utoipa::path(
    post,
    path = "/auth/signup",
    request_body = SignupRequest,
    responses(
        (status = 201, body = TokenIssued),
        (status = 202, description = "Nothing created yet, for example while the sign-up waits for approval"),
        (status = 400, description = "Invalid email, name or password"),
        (status = 403, description = "Sign-up is not open"),
        (status = 409, description = "Email address taken"),
        (status = 429, description = "Too many sign-ups from this address")
    )
)]
pub(crate) async fn signup(
    State(instance): State<Instance>,
    ClientAddress(client): ClientAddress,
    Json(request): Json<SignupRequest>,
) -> Result<(StatusCode, Json<TokenIssued>), ApiError> {
    if instance.signup != SignupMode::Open {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "sign-up is not open on this instance".to_owned(),
        ));
    }
    match instance
        .lifecycle
        .on_signup(request.email.trim(), &client)
        .await
    {
        SignupDecision::Allow => {}
        SignupDecision::Reject(message) => return Err(ApiError(StatusCode::FORBIDDEN, message)),
        SignupDecision::Pending(message) => return Err(ApiError(StatusCode::ACCEPTED, message)),
    }
    let tenant_name = request
        .tenant_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&request.name);
    let (user, tenant) = instance
        .accounts
        .signup(
            NewAccount {
                email: &request.email,
                name: &request.name,
                password: &request.password,
                tenant_name,
            },
            &client,
        )
        .await
        .map_err(auth_error)?;
    instance.lifecycle.on_tenant_created(tenant, user.id).await;
    send_verification(&instance, &user).await;
    let issued = instance
        .accounts
        .issue(user.id, tenant, &request.device_name, request.device_kind)
        .await
        .map_err(auth_error)?;
    Ok((StatusCode::CREATED, Json(TokenIssued::of(&issued, user.id))))
}

/// Confirms an email address with the token from the verification mail.
#[utoipa::path(
    post,
    path = "/auth/verify",
    request_body = LinkToken,
    responses((status = 204), (status = 400, description = "Invalid or expired token"))
)]
pub(crate) async fn verify_email(
    State(instance): State<Instance>,
    Json(request): Json<LinkToken>,
) -> Result<StatusCode, ApiError> {
    instance
        .accounts
        .verify_email(request.token.expose_secret())
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a new verification mail to the signed-in user.
#[utoipa::path(
    post,
    path = "/auth/verify/resend",
    responses((status = 204), (status = 409, description = "No mail is configured")),
    security(("bearer" = []))
)]
pub(crate) async fn resend_verification(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<StatusCode, ApiError> {
    let user = caller.require_user()?;
    if instance.mailer.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this instance sends no mail".to_owned(),
        ));
    }
    let user = instance
        .accounts
        .user(user)
        .await
        .map_err(auth_error)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown user".to_owned()))?;
    send_verification(&instance, &user).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The user a token is issued for: the caller, or for the admin token the
/// first owner of the tenant.
pub(crate) async fn acting_user(instance: &Instance, caller: &Caller) -> Result<Uuid, ApiError> {
    if let Some(user) = caller.user {
        return Ok(user);
    }
    instance
        .accounts
        .primary_user(caller.tenant)
        .await
        .map_err(auth_error)?
        .map(|user| user.id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "the tenant has no owner".to_owned()))
}

/// Issues a token for a desktop agent or a script, for the caller's tenant.
#[utoipa::path(
    post,
    path = "/auth/tokens",
    request_body = TokenRequest,
    responses((status = 200, body = TokenIssued)),
    security(("bearer" = []))
)]
pub(crate) async fn issue_token(
    State(instance): State<Instance>,
    caller: Caller,
    Json(request): Json<TokenRequest>,
) -> Result<Json<TokenIssued>, ApiError> {
    let user = acting_user(&instance, &caller).await?;
    let issued = instance
        .accounts
        .issue(user, caller.tenant, &request.name, request.kind)
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued::of(&issued, user)))
}

/// Changes the password of the signed-in user. Every other device is signed out.
#[utoipa::path(
    post,
    path = "/auth/password",
    request_body = PasswordChange,
    responses((status = 204), (status = 400, description = "New password too short"),
        (status = 403, description = "Wrong current password")),
    security(("bearer" = []))
)]
pub(crate) async fn change_password(
    State(instance): State<Instance>,
    caller: Caller,
    Json(request): Json<PasswordChange>,
) -> Result<StatusCode, ApiError> {
    let user = caller.require_user()?;
    instance
        .accounts
        .change_password(user, &request.current, &request.new, caller.device)
        .await
        .map_err(|error| match error {
            // 401 would read as "token expired" and sign the client out.
            AuthError::WrongPassword => {
                ApiError(StatusCode::FORBIDDEN, "wrong current password".to_owned())
            }
            other => auth_error(other),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Signs out the current device.
#[utoipa::path(post, path = "/auth/logout", responses((status = 204)), security(("bearer" = [])))]
pub(crate) async fn logout(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<StatusCode, ApiError> {
    if let (Some(user), Some(device)) = (caller.user, caller.device) {
        instance
            .accounts
            .revoke(user, device)
            .await
            .map_err(auth_error)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the signed-in devices of the user, in every tenant.
#[utoipa::path(
    get,
    path = "/auth/devices",
    responses((status = 200, body = Vec<Device>)),
    security(("bearer" = []))
)]
pub(crate) async fn devices(
    State(instance): State<Instance>,
    caller: Caller,
) -> Result<Json<Vec<Device>>, ApiError> {
    let user = acting_user(&instance, &caller).await?;
    Ok(Json(
        instance.accounts.devices(user).await.map_err(auth_error)?,
    ))
}

/// Signs out a device of the user.
#[utoipa::path(
    delete,
    path = "/auth/devices/{id}",
    params(("id" = Uuid, Path, description = "Device ID")),
    responses((status = 204), (status = 404, description = "Unknown device")),
    security(("bearer" = []))
)]
pub(crate) async fn revoke_device(
    State(instance): State<Instance>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let user = acting_user(&instance, &caller).await?;
    if instance
        .accounts
        .revoke(user, id)
        .await
        .map_err(auth_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "unknown device".to_owned()))
    }
}

/// An invitation as the invited person sees it.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct InvitationInfo {
    /// Invited email address.
    pub email: String,
    /// Role the new member gets.
    pub role: Role,
    /// Name of the tenant.
    pub tenant_name: String,
    /// When the invitation expires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Whether an account with this email address exists. Then the person
    /// signs in and accepts with `POST /tenants/accept` instead.
    pub account_exists: bool,
}

/// Accepting an invitation with a new account.
#[derive(Deserialize, ToSchema)]
pub struct AcceptRequest {
    /// The invitation token from the link.
    #[schema(value_type = String)]
    pub token: SecretString,
    /// Display name.
    pub name: String,
    /// The password, at least 12 characters.
    #[schema(value_type = String)]
    pub password: SecretString,
    /// Name of this device.
    pub device_name: String,
    /// Client kind.
    pub device_kind: DeviceKind,
}

fn invitations_closed(instance: &Instance) -> Result<(), ApiError> {
    if instance.signup == SignupMode::Closed {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "invitations are off on this instance".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// Shows the invitation behind a token.
#[utoipa::path(
    post,
    operation_id = "inspect_invitation",
    path = "/auth/invitation",
    request_body = LinkToken,
    responses((status = 200, body = InvitationInfo), (status = 400, description = "Invalid or expired token"))
)]
pub(crate) async fn invitation(
    State(instance): State<Instance>,
    Json(request): Json<LinkToken>,
) -> Result<Json<InvitationInfo>, ApiError> {
    invitations_closed(&instance)?;
    let (invitation, tenant_name) = instance
        .accounts
        .invitation(request.token.expose_secret())
        .await
        .map_err(auth_error)?;
    let account_exists = instance
        .accounts
        .user_by_email(&invitation.email)
        .await
        .map_err(auth_error)?
        .is_some();
    Ok(Json(InvitationInfo {
        email: invitation.email,
        role: invitation.role,
        tenant_name,
        expires_at: invitation.expires_at,
        account_exists,
    }))
}

/// Accepts an invitation with a new account and signs in its first device.
#[utoipa::path(
    post,
    operation_id = "accept_invitation",
    path = "/auth/accept",
    request_body = AcceptRequest,
    responses(
        (status = 201, body = TokenIssued),
        (status = 400, description = "Invalid token, name or password"),
        (status = 403, description = "Invitations are off"),
        (status = 409, description = "An account with this email address exists")
    )
)]
pub(crate) async fn accept(
    State(instance): State<Instance>,
    Json(request): Json<AcceptRequest>,
) -> Result<(StatusCode, Json<TokenIssued>), ApiError> {
    invitations_closed(&instance)?;
    let accepted = instance
        .accounts
        .accept_invitation(
            request.token.expose_secret(),
            AcceptAs::New {
                name: &request.name,
                password: &request.password,
            },
        )
        .await
        .map_err(auth_error)?;
    instance
        .lifecycle
        .on_invitation_accepted(&accepted.user, accepted.tenant, accepted.role)
        .await;
    send_verification(&instance, &accepted.user).await;
    let issued = instance
        .accounts
        .issue(
            accepted.user.id,
            accepted.tenant,
            &request.device_name,
            request.device_kind,
        )
        .await
        .map_err(auth_error)?;
    Ok((
        StatusCode::CREATED,
        Json(TokenIssued::of(&issued, accepted.user.id)),
    ))
}

/// Lists the names of stored credentials. Values never leave the server.
#[utoipa::path(
    get,
    path = "/credentials",
    responses((status = 200, body = Vec<String>)),
    security(("bearer" = []))
)]
pub(crate) async fn list_credentials(Scoped(state): Scoped) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(state.vault.list().await.map_err(internal)?))
}

/// Stores a credential of the tenant, for example a model provider key such as
/// `anthropic`, `mcp:linear` or `slack:#weekly`. Needs the owner or admin role.
#[utoipa::path(
    put,
    path = "/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    request_body = CredentialValue,
    responses((status = 204), (status = 400, description = "Empty value"),
        (status = 403, description = "Needs the owner or admin role")),
    security(("bearer" = []))
)]
pub(crate) async fn put_credential(
    Scoped(state): Scoped,
    caller: Caller,
    Path(name): Path<String>,
    Json(request): Json<CredentialValue>,
) -> Result<StatusCode, ApiError> {
    caller.require_manager()?;
    if request.value.expose_secret().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the value is empty".to_owned(),
        ));
    }
    state
        .vault
        .put(&name, &request.value)
        .await
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a credential. Needs the owner or admin role.
#[utoipa::path(
    delete,
    path = "/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    responses((status = 204), (status = 403, description = "Needs the owner or admin role"),
        (status = 404, description = "Unknown credential")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_credential(
    Scoped(state): Scoped,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require_manager()?;
    if state.vault.delete(&name).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "unknown credential".to_owned(),
        ))
    }
}
