//! Sign-in, devices and credentials.

use crate::{ApiError, ApiState, bearer_token, internal, is_admin};
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use lighthouse_auth::{AuthError, Device, DeviceKind};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The device behind the current request. Absent for the admin token.
#[derive(Clone, Copy, Debug)]
pub struct Caller(pub Option<Uuid>);

/// Lets a request through with a device token or the admin token.
pub(crate) async fn require_auth(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(provided) = bearer_token(&request).map(str::to_owned) else {
        return unauthorized();
    };
    if is_admin(&state, &provided) {
        request.extensions_mut().insert(Caller(None));
        return next.run(request).await;
    }
    match state.auth.verify(&provided).await {
        Ok(Some(device)) => {
            request.extensions_mut().insert(Caller(Some(device)));
            next.run(request).await
        }
        Ok(None) => unauthorized(),
        Err(error) => internal(error).into_response(),
    }
}

/// Lets a request through with the admin token only.
pub(crate) async fn require_admin(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let valid = bearer_token(&request).is_some_and(|token| is_admin(&state, token));
    if valid {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "admin token required".to_owned()).into_response()
    }
}

fn unauthorized() -> Response {
    ApiError(StatusCode::UNAUTHORIZED, "sign-in required".to_owned()).into_response()
}

fn auth_error(error: AuthError) -> ApiError {
    match error {
        AuthError::WeakPassword => ApiError(StatusCode::BAD_REQUEST, error.to_string()),
        AuthError::WrongPassword => ApiError(StatusCode::UNAUTHORIZED, error.to_string()),
        AuthError::LockedOut => ApiError(StatusCode::TOO_MANY_REQUESTS, error.to_string()),
        other => internal(other),
    }
}

/// State of the sign-in.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AuthStatus {
    /// Whether the owner still has to set a password.
    pub setup_required: bool,
}

/// A new password.
#[derive(Deserialize, ToSchema)]
pub struct SetupRequest {
    /// The password, at least 12 characters.
    #[schema(value_type = String)]
    pub password: SecretString,
}

/// A sign-in attempt.
#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    /// The password.
    #[schema(value_type = String)]
    pub password: SecretString,
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
}

/// A credential to store.
#[derive(Deserialize, ToSchema)]
pub struct CredentialValue {
    /// The secret value.
    #[schema(value_type = String)]
    pub value: SecretString,
}

/// Reports whether the instance is set up.
#[utoipa::path(get, operation_id = "auth_status",
    path = "/auth/status", responses((status = 200, body = AuthStatus)))]
pub(crate) async fn status(State(state): State<ApiState>) -> Result<Json<AuthStatus>, ApiError> {
    let set_up = state.auth.is_set_up().await.map_err(auth_error)?;
    Ok(Json(AuthStatus {
        setup_required: !set_up,
    }))
}

/// Sets the owner password. This signs out every device.
#[utoipa::path(
    post,
    path = "/auth/setup",
    request_body = SetupRequest,
    responses((status = 204), (status = 400, description = "Password too short")),
    security(("admin_token" = []))
)]
pub(crate) async fn setup(
    State(state): State<ApiState>,
    Json(request): Json<SetupRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .auth
        .set_password(&request.password)
        .await
        .map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Signs in a new device.
#[utoipa::path(
    post,
    path = "/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 200, body = TokenIssued),
        (status = 401, description = "Wrong password"),
        (status = 429, description = "Too many failed attempts")
    )
)]
pub(crate) async fn login(
    State(state): State<ApiState>,
    Json(request): Json<LoginRequest>,
) -> Result<Json<TokenIssued>, ApiError> {
    let issued = state
        .auth
        .login(&request.password, &request.device_name, request.device_kind)
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued {
        id: issued.id,
        token: issued.token.expose_secret().to_owned(),
    }))
}

/// Issues a token for a desktop agent or a script.
#[utoipa::path(
    post,
    path = "/auth/tokens",
    request_body = TokenRequest,
    responses((status = 200, body = TokenIssued)),
    security(("bearer" = []))
)]
pub(crate) async fn issue_token(
    State(state): State<ApiState>,
    Json(request): Json<TokenRequest>,
) -> Result<Json<TokenIssued>, ApiError> {
    let issued = state
        .auth
        .issue(&request.name, request.kind)
        .await
        .map_err(auth_error)?;
    Ok(Json(TokenIssued {
        id: issued.id,
        token: issued.token.expose_secret().to_owned(),
    }))
}

/// Signs out the current device.
#[utoipa::path(post, path = "/auth/logout", responses((status = 204)), security(("bearer" = [])))]
pub(crate) async fn logout(
    State(state): State<ApiState>,
    axum::Extension(caller): axum::Extension<Caller>,
) -> Result<StatusCode, ApiError> {
    if let Some(device) = caller.0 {
        state.auth.revoke(device).await.map_err(auth_error)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the signed-in devices.
#[utoipa::path(
    get,
    path = "/auth/devices",
    responses((status = 200, body = Vec<Device>)),
    security(("bearer" = []))
)]
pub(crate) async fn devices(State(state): State<ApiState>) -> Result<Json<Vec<Device>>, ApiError> {
    Ok(Json(state.auth.devices().await.map_err(auth_error)?))
}

/// Signs out a device.
#[utoipa::path(
    delete,
    path = "/auth/devices/{id}",
    params(("id" = Uuid, Path, description = "Device ID")),
    responses((status = 204), (status = 404, description = "Unknown device")),
    security(("bearer" = []))
)]
pub(crate) async fn revoke_device(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if state.auth.revoke(id).await.map_err(auth_error)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "unknown device".to_owned()))
    }
}

/// Lists the names of stored credentials. Values never leave the server.
#[utoipa::path(
    get,
    path = "/credentials",
    responses((status = 200, body = Vec<String>)),
    security(("bearer" = []))
)]
pub(crate) async fn list_credentials(
    State(state): State<ApiState>,
) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(state.vault.list().await.map_err(internal)?))
}

/// Stores a credential, for example `anthropic`, `mcp:linear` or `smtp`.
#[utoipa::path(
    put,
    path = "/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    request_body = CredentialValue,
    responses((status = 204), (status = 400, description = "Empty value")),
    security(("bearer" = []))
)]
pub(crate) async fn put_credential(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Json(request): Json<CredentialValue>,
) -> Result<StatusCode, ApiError> {
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

/// Deletes a credential.
#[utoipa::path(
    delete,
    path = "/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    responses((status = 204), (status = 404, description = "Unknown credential")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_credential(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.vault.delete(&name).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "unknown credential".to_owned(),
        ))
    }
}
