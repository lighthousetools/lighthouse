//! HTTP API of the server.
//!
//! Only `/health`, `/openapi.json`, the sign-in and the OAuth callback are public.
//! Everything else needs a device token or the admin token, because runs and
//! reports contain private data. Setting the password needs the admin token.

mod approvals;
mod auth;

pub use approvals::{BatchDecision, BatchResult, Decision};
pub use auth::{
    AuthStatus, Caller, CredentialValue, LoginRequest, SetupRequest, TokenIssued, TokenRequest,
};

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};

pub use axum::Router;
use lighthouse_audit::{Approval, AuditLog, RunDetail, RunRecord, StepRecord};
use lighthouse_auth::{Auth, Device, DeviceKind};
use lighthouse_mcp::{McpError, OAuthFlows};
use lighthouse_vault::Vault;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;

/// Errors when starting a run.
#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    /// No agent has this ID.
    #[error("unknown agent `{0}`")]
    UnknownAgent(String),
    /// The run could not be registered.
    #[error("cannot start the run: {0}")]
    Failed(String),
}

/// Starts agents on request.
#[async_trait]
pub trait RunTrigger: Send + Sync {
    /// Starts a run in the background and returns its ID.
    async fn trigger(&self, agent_id: &str) -> Result<Uuid, TriggerError>;

    /// Continues a paused run in the background if no decision is open.
    async fn resume(&self, run: Uuid) -> Result<(), TriggerError>;
}

/// Shared state of the API.
#[derive(Clone)]
pub struct ApiState {
    /// The audit log.
    pub audit: AuditLog,
    /// Starts runs.
    pub trigger: Arc<dyn RunTrigger>,
    /// OAuth flows.
    pub oauth: Arc<OAuthFlows>,
    /// Sign-in and device tokens.
    pub auth: Auth,
    /// Encrypted credentials.
    pub vault: Vault,
    /// Admin token. Without one, the password can only be set by the CLI.
    pub admin_token: Option<SecretString>,
}

/// Answer of the health check.
#[derive(Serialize, ToSchema)]
pub struct Health {
    /// Always `ok`.
    pub status: &'static str,
    /// Server version.
    pub version: &'static str,
}

/// Answer when a run started.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct RunStarted {
    /// ID of the new run.
    pub id: Uuid,
}

/// Answer when an OAuth flow started.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ConnectStarted {
    /// URL to open in a browser.
    pub authorize_url: String,
}

/// Parameters of the run list.
#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

/// Parameters of the OAuth redirect.
#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "request failed");
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error".to_owned(),
    )
}

fn bearer_token(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn is_admin(state: &ApiState, provided: &str) -> bool {
    state.admin_token.as_ref().is_some_and(|expected| {
        bool::from(
            expected
                .expose_secret()
                .as_bytes()
                .ct_eq(provided.as_bytes()),
        )
    })
}

/// Reports that the server is up.
#[utoipa::path(get, path = "/health", responses((status = 200, body = Health)))]
async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Lists the latest runs.
#[utoipa::path(
    get,
    path = "/runs",
    params(("limit" = Option<i64>, Query, description = "Maximum number of runs")),
    responses((status = 200, body = Vec<RunRecord>)),
    security(("bearer" = []))
)]
async fn list_runs(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<RunRecord>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    Ok(Json(state.audit.list_runs(limit).await.map_err(internal)?))
}

/// Returns a run with all its steps.
#[utoipa::path(
    get,
    path = "/runs/{id}",
    params(("id" = Uuid, Path, description = "Run ID")),
    responses((status = 200, body = RunDetail), (status = 404, description = "Unknown run")),
    security(("bearer" = []))
)]
async fn get_run(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunDetail>, ApiError> {
    let detail = state.audit.run_detail(id).await.map_err(internal)?;
    detail
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown run".to_owned()))
}

/// Starts an agent.
#[utoipa::path(
    post,
    path = "/agents/{id}/run",
    params(("id" = String, Path, description = "Agent ID")),
    responses((status = 202, body = RunStarted), (status = 404, description = "Unknown agent")),
    security(("bearer" = []))
)]
async fn run_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<RunStarted>), ApiError> {
    let run = state
        .trigger
        .trigger(&id)
        .await
        .map_err(|error| match error {
            TriggerError::UnknownAgent(_) => ApiError(StatusCode::NOT_FOUND, error.to_string()),
            TriggerError::Failed(_) => internal(error),
        })?;
    Ok((StatusCode::ACCEPTED, Json(RunStarted { id: run })))
}

/// Starts the OAuth flow of a connector.
#[utoipa::path(
    post,
    path = "/connectors/{id}/connect",
    params(("id" = String, Path, description = "Connector name")),
    responses((status = 200, body = ConnectStarted), (status = 400, description = "Cannot start the flow")),
    security(("bearer" = []))
)]
async fn connect(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<ConnectStarted>, ApiError> {
    let authorize_url = state.oauth.start(&id).await.map_err(|error| match error {
        McpError::UnknownConnector(_) | McpError::OAuth(_) => {
            ApiError(StatusCode::BAD_REQUEST, error.to_string())
        }
        other => internal(other),
    })?;
    Ok(Json(ConnectStarted { authorize_url }))
}

async fn oauth_callback(
    State(state): State<ApiState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let failed =
        |message: &'static str| (StatusCode::BAD_REQUEST, Html(page(message))).into_response();
    if query.error.is_some() {
        return failed("The authorization was declined.");
    }
    let (Some(code), Some(csrf)) = (query.code, query.state) else {
        return failed("The redirect is missing parameters.");
    };
    match state.oauth.finish(&code, &csrf, query.iss.as_deref()).await {
        Ok(connector) => {
            tracing::info!(%connector, "connector authorized");
            Html(page("Connected. You can close this window.")).into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "OAuth callback failed");
            failed("The authorization could not be completed.")
        }
    }
}

fn page(message: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>Lighthouse</title><p>{message}</p>")
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
        let components = openapi.components.get_or_insert_with(Default::default);
        for name in ["bearer", "admin_token"] {
            components.add_security_scheme(
                name,
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        }
    }
}

/// OpenAPI description of the API.
#[derive(OpenApi)]
#[openapi(
    info(title = "Lighthouse API"),
    paths(
        health, list_runs, get_run, run_agent, connect,
        auth::status, auth::setup, auth::login, auth::issue_token, auth::logout,
        auth::devices, auth::revoke_device,
        auth::list_credentials, auth::put_credential, auth::delete_credential,
        approvals::list, approvals::decide, approvals::decide_batch,
    ),
    components(schemas(
        Health, RunRecord, StepRecord, RunDetail, RunStarted, ConnectStarted,
        AuthStatus, SetupRequest, LoginRequest, TokenRequest, TokenIssued, CredentialValue,
        Device, DeviceKind, Approval, Decision, BatchDecision, BatchResult,
    )),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

/// Serves the API until shutdown is requested.
pub async fn serve(listener: tokio::net::TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// Builds the router.
pub fn router(state: ApiState) -> Router {
    let signed_in = Router::new()
        .route("/runs", get(list_runs))
        .route("/runs/{id}", get(get_run))
        .route("/agents/{id}/run", post(run_agent))
        .route("/connectors/{id}/connect", post(connect))
        .route("/auth/tokens", post(auth::issue_token))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/devices", get(auth::devices))
        .route("/auth/devices/{id}", delete(auth::revoke_device))
        .route("/approvals", get(approvals::list))
        .route("/approvals/decisions", post(approvals::decide_batch))
        .route("/approvals/{id}/decision", post(approvals::decide))
        .route("/credentials", get(auth::list_credentials))
        .route(
            "/credentials/{name}",
            put(auth::put_credential).delete(auth::delete_credential),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));
    let admin = Router::new().route("/auth/setup", post(auth::setup)).layer(
        middleware::from_fn_with_state(state.clone(), auth::require_admin),
    );
    Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/oauth/callback", get(oauth_callback))
        .route("/auth/status", get(auth::status))
        .route("/auth/login", post(auth::login))
        .merge(signed_in)
        .merge(admin)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use base64::Engine;
    use lighthouse_audit::NewApproval;
    use lighthouse_core::{RunId, TenantId};
    use lighthouse_store::testing::fresh_store;
    use lighthouse_vault::{MasterKey, Vault};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const TOKEN: &str = "test-admin-token";

    struct FakeTrigger(Uuid, Arc<Mutex<Vec<Uuid>>>);

    #[async_trait]
    impl RunTrigger for FakeTrigger {
        async fn trigger(&self, agent_id: &str) -> Result<Uuid, TriggerError> {
            if agent_id == "known" {
                Ok(self.0)
            } else {
                Err(TriggerError::UnknownAgent(agent_id.to_owned()))
            }
        }

        async fn resume(&self, run: Uuid) -> Result<(), TriggerError> {
            self.1.lock().unwrap().push(run);
            Ok(())
        }
    }

    async fn app(token: Option<&str>) -> (Router, AuditLog) {
        let (router, audit, _) = app_with_resumes(token).await;
        (router, audit)
    }

    async fn app_with_resumes(token: Option<&str>) -> (Router, AuditLog, Arc<Mutex<Vec<Uuid>>>) {
        let resumed = Arc::new(Mutex::new(Vec::new()));
        let store = fresh_store().await;
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let vault = Vault::new(
            store.clone(),
            TenantId::SINGLE,
            &MasterKey::from_base64(&key).unwrap(),
        );
        let audit = AuditLog::new(store.clone(), TenantId::SINGLE);
        let state = ApiState {
            audit: audit.clone(),
            trigger: Arc::new(FakeTrigger(Uuid::nil(), resumed.clone())),
            oauth: Arc::new(OAuthFlows::new(
                BTreeMap::new(),
                vault.clone(),
                "https://lighthouse.test",
            )),
            auth: Auth::new(store, TenantId::SINGLE),
            vault,
            admin_token: token.map(|token| SecretString::from(token.to_owned())),
        };
        (router(state), audit, resumed)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        send(app, method, uri, token, None).await
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        json: Option<Value>,
    ) -> (StatusCode, String) {
        let mut request = HttpRequest::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match json {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn health_and_openapi_are_public() {
        let (app, _) = app(None).await;
        let (status, body) = call(&app, "GET", "/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"ok\""));
        let (status, body) = call(&app, "GET", "/openapi.json", None).await;
        assert_eq!(status, StatusCode::OK);
        let spec: Value = serde_json::from_str(&body).unwrap();
        for path in [
            "/health",
            "/runs",
            "/runs/{id}",
            "/agents/{id}/run",
            "/connectors/{id}/connect",
            "/auth/login",
            "/credentials/{name}",
        ] {
            assert!(spec["paths"].get(path).is_some(), "{path}");
        }
    }

    #[tokio::test]
    async fn admin_endpoints_need_the_right_token() {
        let (app, _) = app(Some(TOKEN)).await;
        let endpoints = [
            ("GET", "/runs"),
            ("GET", "/runs/00000000-0000-0000-0000-000000000000"),
            ("POST", "/agents/known/run"),
            ("POST", "/connectors/demo/connect"),
        ];
        for (method, uri) in endpoints {
            for token in [None, Some("wrong")] {
                let (status, _) = call(&app, method, uri, token).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
            }
        }
    }

    #[tokio::test]
    async fn without_a_configured_token_admin_endpoints_stay_closed() {
        let (app, _) = app(None).await;
        let (status, _) = call(&app, "GET", "/runs", Some("")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn lists_and_shows_runs() {
        let (app, audit) = app(Some(TOKEN)).await;
        let run = RunId::new();
        audit.start_run(run, "brief", "manual").await.unwrap();
        let (status, body) = call(&app, "GET", "/runs?limit=1", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("brief"));
        let (status, body) = call(
            &app,
            "GET",
            &format!("/runs/{}", run.as_uuid()),
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"steps\":[]"));
        let unknown = format!("/runs/{}", Uuid::now_v7());
        assert_eq!(
            call(&app, "GET", &unknown, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn starts_known_agents_only() {
        let (app, _) = app(Some(TOKEN)).await;
        assert_eq!(
            call(&app, "POST", "/agents/known/run", Some(TOKEN)).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(&app, "POST", "/agents/other/run", Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn connect_reports_unknown_connectors() {
        let (app, _) = app(Some(TOKEN)).await;
        let (status, body) = call(&app, "POST", "/connectors/nobody/connect", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("nobody"));
    }

    #[tokio::test]
    async fn callback_rejects_bad_requests_without_echoing_input() {
        let (app, _) = app(None).await;
        for uri in [
            "/oauth/callback",
            "/oauth/callback?error=access_denied",
            "/oauth/callback?code=x&state=%3Cscript%3E",
        ] {
            let (status, body) = call(&app, "GET", uri, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!body.contains("<script"), "{body}");
        }
    }

    const PASSWORD: &str = "a long enough password";

    async fn signed_in(app: &Router) -> String {
        let (status, _) = send(
            app,
            "POST",
            "/auth/setup",
            Some(TOKEN),
            Some(json!({"password": PASSWORD})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = send(
            app,
            "POST",
            "/auth/login",
            None,
            Some(json!({"password": PASSWORD, "device_name": "test", "device_kind": "web"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_str::<TokenIssued>(&body).unwrap().token
    }

    #[tokio::test]
    async fn setup_needs_the_admin_token_even_on_a_fresh_instance() {
        let (app, _) = app(Some(TOKEN)).await;
        let (_, body) = call(&app, "GET", "/auth/status", None).await;
        assert!(body.contains("\"setup_required\":true"));
        let attempt = json!({"password": PASSWORD});
        let (status, _) = send(&app, "POST", "/auth/setup", None, Some(attempt)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let token = signed_in(&app).await;
        let attempt = json!({"password": "somebody elses password"});
        let (status, _) = send(&app, "POST", "/auth/setup", Some(&token), Some(attempt)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (_, body) = call(&app, "GET", "/auth/status", None).await;
        assert!(body.contains("\"setup_required\":false"));
    }

    #[tokio::test]
    async fn device_tokens_open_the_api_until_logout() {
        let (app, _) = app(Some(TOKEN)).await;
        let token = signed_in(&app).await;
        assert_eq!(
            call(&app, "GET", "/runs", Some(&token)).await.0,
            StatusCode::OK
        );
        let (_, body) = call(&app, "GET", "/auth/devices", Some(&token)).await;
        assert!(body.contains("\"name\":\"test\""));
        assert_eq!(
            call(&app, "POST", "/auth/logout", Some(&token)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "GET", "/runs", Some(&token)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_wrong_password_is_refused_and_later_rate_limited() {
        let (app, _) = app(Some(TOKEN)).await;
        signed_in(&app).await;
        let attempt = json!({"password": "wrong", "device_name": "x", "device_kind": "web"});
        for expected in [StatusCode::UNAUTHORIZED, StatusCode::TOO_MANY_REQUESTS] {
            let mut last = StatusCode::OK;
            for _ in 0..5 {
                last = send(&app, "POST", "/auth/login", None, Some(attempt.clone()))
                    .await
                    .0;
                if last == expected {
                    break;
                }
            }
            assert_eq!(last, expected);
        }
    }

    #[tokio::test]
    async fn devices_can_be_issued_and_revoked() {
        let (app, _) = app(Some(TOKEN)).await;
        let token = signed_in(&app).await;
        let request = json!({"name": "mac", "kind": "node"});
        let (status, body) = send(&app, "POST", "/auth/tokens", Some(&token), Some(request)).await;
        assert_eq!(status, StatusCode::OK);
        let issued: TokenIssued = serde_json::from_str(&body).unwrap();
        let uri = format!("/auth/devices/{}", issued.id);
        assert_eq!(
            call(&app, "DELETE", &uri, Some(&token)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "DELETE", &uri, Some(&token)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "GET", "/runs", Some(&issued.token)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn credentials_are_write_only() {
        let (app, _) = app(Some(TOKEN)).await;
        let token = signed_in(&app).await;
        let uri = "/credentials/mcp:linear";
        let (status, _) = send(
            &app,
            "PUT",
            uri,
            Some(&token),
            Some(json!({"value": "s3cret-value"})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = call(&app, "GET", "/credentials", Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("mcp:linear"));
        assert!(!body.contains("s3cret-value"));
        let (status, _) = send(&app, "PUT", uri, Some(&token), Some(json!({"value": ""}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            call(&app, "DELETE", uri, Some(&token)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "DELETE", uri, Some(&token)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    async fn proposals(audit: &AuditLog, count: usize) -> (Uuid, Vec<Uuid>) {
        let run = RunId::new();
        audit.start_run(run, "mailer", "manual").await.unwrap();
        let new: Vec<_> = (0..count)
            .map(|_| NewApproval {
                id: Uuid::now_v7(),
                tool: "mail.send".to_owned(),
                input: json!({"to": "a"}),
                quotes_external: false,
            })
            .collect();
        audit
            .pause_run(run, "mailer", &json!({}), &new)
            .await
            .unwrap();
        (run.as_uuid(), new.into_iter().map(|item| item.id).collect())
    }

    #[tokio::test]
    async fn a_decision_resumes_the_run_and_a_second_one_conflicts() {
        let (app, audit, resumed) = app_with_resumes(Some(TOKEN)).await;
        let (run, ids) = proposals(&audit, 1).await;
        let (status, body) = call(&app, "GET", "/approvals?status=pending", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("mail.send"));
        let uri = format!("/approvals/{}/decision", ids[0]);
        let decision = json!({"approve": true, "input": {"to": "b"}});
        let (status, body) = send(&app, "POST", &uri, Some(TOKEN), Some(decision.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"to\":\"b\""));
        assert_eq!(*resumed.lock().unwrap(), [run]);
        let (status, _) = send(&app, "POST", &uri, Some(TOKEN), Some(decision)).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_batch_reports_what_it_decided_and_resumes_each_run_once() {
        let (app, audit, resumed) = app_with_resumes(Some(TOKEN)).await;
        let (run, ids) = proposals(&audit, 2).await;
        let unknown = Uuid::now_v7();
        let batch = json!({"ids": [ids[0], ids[1], unknown], "approve": false});
        let (status, body) = send(
            &app,
            "POST",
            "/approvals/decisions",
            Some(TOKEN),
            Some(batch),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let result: BatchResult = serde_json::from_str(&body).unwrap();
        assert_eq!(result.decided, ids);
        assert_eq!(result.skipped, [unknown]);
        assert_eq!(*resumed.lock().unwrap(), [run]);
    }

    #[tokio::test]
    async fn approvals_need_a_sign_in() {
        let (app, _) = app(Some(TOKEN)).await;
        assert_eq!(
            call(&app, "GET", "/approvals", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
