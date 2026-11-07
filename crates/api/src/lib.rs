//! HTTP API of the server.
//!
//! Only `/health`, `/openapi.json`, the sign-in and the OAuth callback are public.
//! Everything else needs a device token or the admin token, because runs and
//! reports contain private data. Setting the password needs the admin token.

mod alerts;
mod approvals;
mod auth;
mod automations;
mod brain;
mod catalog;
mod documents;
mod memory;
mod ready;
mod rules;
mod sessions;
mod tasks;

pub use approvals::{BatchDecision, BatchResult, Decision};
pub use auth::{
    AuthStatus, Caller, CredentialValue, LoginRequest, SetupRequest, TokenIssued, TokenRequest,
};
pub use automations::{
    AutomationInfo, AutomationRuns, AutomationStepInfo, AutomationSwitch, AutomationTriggerInfo,
    Automations, ExternalEvent,
};
pub use brain::{AskAnswer, AskRequest, EntryDetail, ResolvedEntry};
pub use catalog::{
    AgentChangeError, AgentDraftRequest, AgentInfo, AgentSource, AgentTool, Catalog, ConnectorInfo,
    SkillInfo,
};
pub use ready::{ReadyFeed, ReadyItem};
pub use sessions::{NodeChange, PermissionDecision};
pub use tasks::{TaskOrder, TaskOrderResult};

use async_trait::async_trait;
use axum::Json;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};

pub use axum::Router;
use lighthouse_audit::{Approval, AuditLog, DailyCost, RunDetail, RunRecord, StepRecord};
use lighthouse_auth::{Auth, Device, DeviceKind};
use lighthouse_brain::{Brain, Entry, EntryPatch, EntrySummary, NewEntry, SearchHit, Source};
use lighthouse_desk::{
    Alert, AlertCounts, AlertView, Desk, DeskError, Fact, FactPatch, FactSource, NewFact, NewRule,
    NewTask, Origin, Rule, RulePatch, SourceLink, Task, TaskPatch, TaskStatus, TaskView,
};
use lighthouse_mcp::{McpError, OAuthFlows};
use lighthouse_notify::EventBus;
use lighthouse_sessions::{
    NewPermission, Node, NodeReport, NodeState, Permission, Session, SessionReport, Sessions,
};
use lighthouse_vault::Vault;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const EVENT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

/// Errors when starting a run.
#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    /// No agent has this ID.
    #[error("unknown agent `{0}`")]
    UnknownAgent(String),
    /// The run may not start, for example because its trigger does not allow it.
    #[error("{0}")]
    Refused(String),
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

/// Answers questions from the Second Brain.
#[async_trait]
pub trait Assistant: Send + Sync {
    /// Answers a question. The error text is safe to show to the owner.
    async fn ask(&self, question: &str) -> Result<AskAnswer, String>;

    /// Answers a question and sends the text as it arrives.
    async fn ask_stream(
        &self,
        question: &str,
        deltas: UnboundedSender<String>,
    ) -> Result<AskAnswer, String>;
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
    /// The Second Brain.
    pub brain: Brain,
    /// Answers questions.
    pub assistant: Arc<dyn Assistant>,
    /// Agents, connectors and skills.
    pub catalog: Arc<dyn Catalog>,
    /// Desktop agents and their sessions.
    pub sessions: Sessions,
    /// Name of the person this instance works for.
    pub owner_name: String,
    /// Admin token. Without one, the password can only be set by the CLI.
    pub admin_token: Option<SecretString>,
    /// Live events for the apps.
    pub events: EventBus,
    /// Tasks, rules, memory, alerts and briefing documents.
    pub desk: Desk,
    /// Time zone of the owner, for dates such as "today".
    pub zone: chrono_tz::Tz,
    /// Automations.
    pub automations: Arc<dyn Automations>,
}

/// Answer of the health check.
#[derive(Serialize, ToSchema)]
pub struct Health {
    /// Always `ok`.
    pub status: &'static str,
    /// Server version.
    pub version: &'static str,
}

/// The signed-in owner.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Me {
    /// Display name.
    pub name: String,
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

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("unknown {what}"))
}

fn desk_error(error: DeskError) -> ApiError {
    match error {
        DeskError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
        other => internal(other),
    }
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

/// Returns the signed-in owner.
#[utoipa::path(
    get,
    path = "/me",
    responses((status = 200, body = Me)),
    security(("bearer" = []))
)]
async fn me(State(state): State<ApiState>) -> Json<Me> {
    Json(Me {
        name: state.owner_name.clone(),
    })
}

/// Tells a client when to reload, as a stream of server-sent events.
///
/// A `changed` event means something the apps show has changed. Named events
/// such as `run.finished`, `run.waiting`, `alert.raised` or `tasks.changed`
/// say what changed and carry IDs. See `docs/api.md` for the list.
#[utoipa::path(
    get,
    path = "/events",
    responses((status = 200, description = "Server-sent events", content_type = "text/event-stream")),
    security(("bearer" = []))
)]
async fn events(State(state): State<ApiState>) -> impl IntoResponse {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    let mut bus = state.events.subscribe();
    tokio::spawn(async move {
        let mut last = None;
        let mut ticker = tokio::time::interval(EVENT_INTERVAL);
        let mut bus_open = true;
        loop {
            let event = tokio::select! {
                _ = ticker.tick() => {
                    let Ok(revision) = state.audit.revision().await else {
                        continue;
                    };
                    if last.as_ref() == Some(&revision) {
                        if sender.is_closed() {
                            return;
                        }
                        continue;
                    }
                    last = Some(revision);
                    Event::default().event("changed").data("{}")
                }
                received = bus.recv(), if bus_open => match received {
                    Ok(event) => Event::default().event(event.name).data(event.data.to_string()),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        Event::default().event("changed").data("{}")
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        bus_open = false;
                        continue;
                    }
                },
            };
            if sender.send(Ok(event)).await.is_err() {
                return;
            }
        }
    });
    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::default())
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
            TriggerError::Refused(_) => ApiError(StatusCode::CONFLICT, error.to_string()),
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
        health, me, events, list_runs, get_run, run_agent, connect,
        auth::status, auth::setup, auth::login, auth::issue_token, auth::logout,
        auth::devices, auth::revoke_device,
        auth::list_credentials, auth::put_credential, auth::delete_credential,
        approvals::list, approvals::decide, approvals::decide_batch,
        brain::search, brain::ask, brain::ask_stream, brain::resolve, brain::list, brain::create, brain::get, brain::update,
        brain::delete, brain::upload, brain::download,
        catalog::agents, catalog::agent, catalog::agent_runs, catalog::draft_agent,
        catalog::save_agent, catalog::delete_agent, catalog::connectors, catalog::skills,
        catalog::costs,
        sessions::report, sessions::request, sessions::await_decision, sessions::nodes,
        sessions::update_node, sessions::sessions, sessions::permissions, sessions::decide,
        tasks::list, tasks::create, tasks::get, tasks::update, tasks::delete, tasks::complete,
        tasks::reopen, tasks::accept, tasks::reorder,
        rules::list, rules::create, rules::get, rules::update, rules::delete,
        memory::list, memory::create, memory::get, memory::update, memory::confirm, memory::delete,
        alerts::list, alerts::counts, alerts::get, alerts::acknowledge, alerts::resolve,
        ready::feed, ready::dismiss_draft,
        documents::list, documents::latest_brief, documents::recaps, documents::recap,
        documents::of_run,
        automations::list, automations::get, automations::save, automations::switch,
        automations::delete, automations::runs, automations::run, automations::webhook,
        automations::emit,
    ),
    components(schemas(
        Health, Me, RunRecord, StepRecord, RunDetail, RunStarted, ConnectStarted,
        AuthStatus, SetupRequest, LoginRequest, TokenRequest, TokenIssued, CredentialValue,
        Device, DeviceKind, Approval, Decision, BatchDecision, BatchResult,
        Entry, EntryDetail, EntryPatch, EntrySummary, NewEntry, SearchHit, Source,
        AskRequest, AskAnswer, ResolvedEntry,
        AgentInfo, AgentTool, ConnectorInfo, SkillInfo, AgentDraftRequest, AgentSource, DailyCost,
        Node, NodeChange, NodeReport, NodeState, NewPermission, Permission, PermissionDecision,
        Session, SessionReport,
        Task, NewTask, TaskPatch, TaskStatus, TaskView, TaskOrder, TaskOrderResult, Origin, SourceLink,
        Rule, NewRule, RulePatch, Fact, FactSource, NewFact, FactPatch,
        Alert, AlertCounts, AlertView, lighthouse_desk::Severity,
        ReadyFeed, ReadyItem, lighthouse_audit::ToolCall,
        lighthouse_desk::BriefingDocument, lighthouse_desk::BriefingContent,
        lighthouse_desk::DocumentSection, lighthouse_desk::DocumentItem,
        lighthouse_desk::ItemSource, lighthouse_desk::ItemNumber,
        AutomationInfo, AutomationTriggerInfo, AutomationStepInfo, AutomationSwitch,
        AutomationRuns, ExternalEvent,
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

/// Serves the built web app from a directory for everything the API does not
/// answer. The app uses hash routes, so unknown paths stay 404.
pub fn with_web_app(router: Router, directory: &std::path::Path) -> Router {
    router.fallback_service(tower_http::services::ServeDir::new(directory))
}

/// Builds the router.
pub fn router(state: ApiState) -> Router {
    let signed_in = Router::new()
        .route("/me", get(me))
        .route("/events", get(events))
        .route("/runs", get(list_runs))
        .route("/runs/{id}", get(get_run))
        .route("/agents", get(catalog::agents).post(catalog::save_agent))
        .route("/agents/draft", post(catalog::draft_agent))
        .route(
            "/agents/{id}",
            get(catalog::agent).delete(catalog::delete_agent),
        )
        .route("/agents/{id}/runs", get(catalog::agent_runs))
        .route("/agents/{id}/run", post(run_agent))
        .route("/node/report", post(sessions::report))
        .route("/node/permissions", post(sessions::request))
        .route("/node/permissions/{id}", get(sessions::await_decision))
        .route("/nodes", get(sessions::nodes))
        .route("/nodes/{id}", patch(sessions::update_node))
        .route("/sessions", get(sessions::sessions))
        .route("/permissions", get(sessions::permissions))
        .route("/permissions/{id}/decision", post(sessions::decide))
        .route("/connectors", get(catalog::connectors))
        .route("/skills", get(catalog::skills))
        .route("/costs", get(catalog::costs))
        .route("/connectors/{id}/connect", post(connect))
        .route("/auth/tokens", post(auth::issue_token))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/devices", get(auth::devices))
        .route("/auth/devices/{id}", delete(auth::revoke_device))
        .route("/approvals", get(approvals::list))
        .route("/approvals/decisions", post(approvals::decide_batch))
        .route("/approvals/{id}/decision", post(approvals::decide))
        .route("/brain/search", get(brain::search))
        .route("/brain/ask", post(brain::ask))
        .route("/brain/ask/stream", post(brain::ask_stream))
        .route("/brain/resolve", get(brain::resolve))
        .route("/brain/entries", get(brain::list).post(brain::create))
        .route(
            "/brain/entries/{id}",
            get(brain::get).patch(brain::update).delete(brain::delete),
        )
        .route(
            "/brain/files",
            post(brain::upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/brain/files/{id}", get(brain::download))
        .route("/tasks", get(tasks::list).post(tasks::create))
        .route("/tasks/order", post(tasks::reorder))
        .route(
            "/tasks/{id}",
            get(tasks::get).patch(tasks::update).delete(tasks::delete),
        )
        .route("/tasks/{id}/complete", post(tasks::complete))
        .route("/tasks/{id}/reopen", post(tasks::reopen))
        .route("/tasks/{id}/accept", post(tasks::accept))
        .route("/rules", get(rules::list).post(rules::create))
        .route(
            "/rules/{id}",
            get(rules::get).patch(rules::update).delete(rules::delete),
        )
        .route("/memory", get(memory::list).post(memory::create))
        .route(
            "/memory/{id}",
            get(memory::get)
                .patch(memory::update)
                .delete(memory::delete),
        )
        .route("/memory/{id}/confirm", post(memory::confirm))
        .route("/runs/{id}/document", get(documents::of_run))
        .route("/documents", get(documents::list))
        .route("/briefs/latest", get(documents::latest_brief))
        .route("/recaps", get(documents::recaps))
        .route("/recaps/{week}", get(documents::recap))
        .route(
            "/automations",
            get(automations::list).post(automations::save),
        )
        .route("/automations/events", post(automations::emit))
        .route(
            "/automations/{id}",
            get(automations::get)
                .patch(automations::switch)
                .delete(automations::delete),
        )
        .route("/automations/{id}/runs", get(automations::runs))
        .route("/automations/{id}/run", post(automations::run))
        .route("/automations/{id}/webhook", post(automations::webhook))
        .route("/ready", get(ready::feed))
        .route("/ready/drafts/{id}/dismiss", post(ready::dismiss_draft))
        .route("/alerts", get(alerts::list))
        .route("/alerts/counts", get(alerts::counts))
        .route("/alerts/{id}", get(alerts::get))
        .route("/alerts/{id}/acknowledge", post(alerts::acknowledge))
        .route("/alerts/{id}/resolve", post(alerts::resolve))
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
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    pub(crate) const TOKEN: &str = "test-admin-token";

    struct FakeAssistant;

    #[async_trait]
    impl Assistant for FakeAssistant {
        async fn ask(&self, question: &str) -> Result<AskAnswer, String> {
            if question == "fail" {
                return Err("no model".to_owned());
            }
            Ok(AskAnswer {
                run_id: Uuid::nil(),
                answer: format!("You asked {question}"),
                sources: Vec::new(),
            })
        }

        async fn ask_stream(
            &self,
            question: &str,
            deltas: UnboundedSender<String>,
        ) -> Result<AskAnswer, String> {
            deltas.send("You ".to_owned()).unwrap();
            deltas.send("asked".to_owned()).unwrap();
            self.ask(question).await
        }
    }

    fn agent_info(id: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            schedule: None,
            next_run: None,
            tools: Vec::new(),
            skills: Vec::new(),
            deliver: Vec::new(),
            blocked: Vec::new(),
            editable: true,
            source: "file".to_owned(),
        }
    }

    struct FakeCatalog;

    #[async_trait]
    impl Catalog for FakeCatalog {
        fn agents(&self) -> Vec<AgentInfo> {
            vec![agent_info("brief")]
        }

        fn agent(&self, id: &str) -> Option<AgentInfo> {
            (id == "brief").then(|| agent_info(id))
        }

        async fn draft_agent(&self, description: &str) -> Result<String, String> {
            if description == "fail" {
                Err("no model".to_owned())
            } else {
                Ok(format!("file for {description}"))
            }
        }

        async fn save_agent(&self, source: &str) -> Result<AgentInfo, AgentChangeError> {
            if source == "bad" {
                Err(AgentChangeError::Rejected("invalid".to_owned()))
            } else {
                Ok(agent_info("saved"))
            }
        }

        async fn delete_agent(&self, id: &str) -> Result<bool, AgentChangeError> {
            match id {
                "fixed" => Err(AgentChangeError::Rejected("read-only".to_owned())),
                _ => Ok(id == "brief"),
            }
        }

        async fn connectors(&self) -> Result<Vec<ConnectorInfo>, String> {
            Ok(vec![ConnectorInfo {
                name: "linear".to_owned(),
                auth: "oauth".to_owned(),
                connected: false,
                read_tools: Vec::new(),
                draft_tools: vec!["save_draft".to_owned()],
            }])
        }

        fn skills(&self) -> Vec<SkillInfo> {
            vec![SkillInfo {
                name: "brief".to_owned(),
                description: "Briefs".to_owned(),
            }]
        }
    }

    fn automation_info(id: &str, enabled: bool) -> AutomationInfo {
        AutomationInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            enabled,
            trigger: AutomationTriggerInfo {
                kind: "webhook".to_owned(),
                cron: None,
                event: None,
                filter: BTreeMap::new(),
            },
            steps: Vec::new(),
            next_run: None,
            editable: id != "fixed",
            source: String::new(),
        }
    }

    struct FakeAutomations;

    #[async_trait]
    impl Automations for FakeAutomations {
        async fn list(&self) -> Result<Vec<AutomationInfo>, String> {
            Ok(vec![automation_info("follow-up", true)])
        }

        async fn get(&self, id: &str) -> Result<Option<AutomationInfo>, String> {
            Ok((id == "follow-up").then(|| automation_info(id, true)))
        }

        async fn save(&self, source: &str) -> Result<AutomationInfo, AgentChangeError> {
            match source {
                "bad" => Err(AgentChangeError::Rejected("invalid".to_owned())),
                _ => Ok(automation_info("saved", true)),
            }
        }

        async fn delete(&self, id: &str) -> Result<bool, AgentChangeError> {
            match id {
                "fixed" => Err(AgentChangeError::Rejected("read-only".to_owned())),
                _ => Ok(id == "follow-up"),
            }
        }

        async fn set_enabled(
            &self,
            id: &str,
            enabled: bool,
        ) -> Result<Option<AutomationInfo>, AgentChangeError> {
            Ok((id == "follow-up").then(|| automation_info(id, enabled)))
        }

        async fn run(&self, id: &str, _payload: Value) -> Result<Uuid, TriggerError> {
            match id {
                "follow-up" => Ok(Uuid::nil()),
                _ => Err(TriggerError::UnknownAgent(id.to_owned())),
            }
        }

        async fn webhook(&self, id: &str, _payload: Value) -> Result<Option<Uuid>, TriggerError> {
            match id {
                "follow-up" => Ok(Some(Uuid::nil())),
                "manual" => Err(TriggerError::Refused("not a webhook".to_owned())),
                _ => Err(TriggerError::UnknownAgent(id.to_owned())),
            }
        }

        async fn emit(&self, _name: &str, _payload: Value) -> Result<Vec<Uuid>, String> {
            Ok(vec![Uuid::nil()])
        }
    }

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

    fn brain_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lighthouse-api-test-{}", Uuid::now_v7()))
    }

    async fn app(token: Option<&str>) -> (Router, AuditLog) {
        let (router, audit, _) = app_with_resumes(token).await;
        (router, audit)
    }

    async fn app_with_resumes(token: Option<&str>) -> (Router, AuditLog, Arc<Mutex<Vec<Uuid>>>) {
        let harness = harness(token).await;
        (harness.app, harness.audit, harness.resumed)
    }

    /// A router with real storage and fakes for everything that calls out.
    pub(crate) struct Harness {
        pub(crate) app: Router,
        pub(crate) audit: AuditLog,
        pub(crate) desk: Desk,
        pub(crate) sessions: Sessions,
        pub(crate) events: EventBus,
        pub(crate) resumed: Arc<Mutex<Vec<Uuid>>>,
    }

    pub(crate) async fn harness(token: Option<&str>) -> Harness {
        let resumed = Arc::new(Mutex::new(Vec::new()));
        let store = fresh_store().await;
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let vault = Vault::new(
            store.clone(),
            TenantId::SINGLE,
            &MasterKey::from_base64(&key).unwrap(),
        );
        let audit = AuditLog::new(store.clone(), TenantId::SINGLE);
        let desk = Desk::new(store.clone(), TenantId::SINGLE);
        let sessions = Sessions::new(store.clone(), TenantId::SINGLE);
        let events = EventBus::new();
        let state = ApiState {
            audit: audit.clone(),
            trigger: Arc::new(FakeTrigger(Uuid::nil(), resumed.clone())),
            oauth: Arc::new(OAuthFlows::new(
                BTreeMap::new(),
                vault.clone(),
                "https://lighthouse.test",
            )),
            auth: Auth::new(store.clone(), TenantId::SINGLE),
            vault,
            brain: Brain::new(store.clone(), TenantId::SINGLE, brain_root()).unwrap(),
            sessions: sessions.clone(),
            owner_name: "Alex".to_owned(),
            assistant: Arc::new(FakeAssistant),
            catalog: Arc::new(FakeCatalog),
            admin_token: token.map(|token| SecretString::from(token.to_owned())),
            events: events.clone(),
            desk: desk.clone(),
            zone: chrono_tz::Europe::Berlin,
            automations: Arc::new(FakeAutomations),
        };
        Harness {
            app: router(state),
            audit,
            desk,
            sessions,
            events,
            resumed,
        }
    }

    pub(crate) async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        send(app, method, uri, token, None).await
    }

    pub(crate) async fn send(
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
    async fn the_web_app_is_served_next_to_the_api() {
        let dir = brain_root();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<title>Lighthouse</title>").unwrap();
        let (app, _) = app(Some(TOKEN)).await;
        let app = with_web_app(app, &dir);
        let (status, body) = call(&app, "GET", "/", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Lighthouse"));
        assert_eq!(
            call(&app, "GET", "/runs", None).await.0,
            StatusCode::UNAUTHORIZED,
            "the API stays protected"
        );
    }

    #[test]
    fn every_operation_has_its_own_id() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let mut ids = Vec::new();
        for item in spec["paths"].as_object().unwrap().values() {
            for operation in item.as_object().unwrap().values() {
                if let Some(id) = operation["operationId"].as_str() {
                    ids.push(id.to_owned());
                }
            }
        }
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "generated clients need unique IDs");
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
        let (_, body) = call(&app, "GET", "/me", Some(&token)).await;
        assert!(body.contains("Alex"));
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
                reason: None,
                consulted: Vec::new(),
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

    #[tokio::test]
    async fn the_brain_stores_finds_changes_and_deletes() {
        let (app, _) = app(Some(TOKEN)).await;
        let new = json!({"type": "note", "title": "Rule", "body": "Due in January"});
        let (status, body) = send(&app, "POST", "/brain/entries", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let entry: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(entry["body"], "Due in January");
        assert_eq!(entry["sources"][0]["kind"], "app");
        let uri = format!("/brain/entries/{}", entry["id"].as_str().unwrap());

        let (_, body) = call(&app, "GET", "/brain/search?q=january", Some(TOKEN)).await;
        assert!(body.contains("Rule"), "{body}");
        let (_, body) = call(&app, "GET", "/brain/entries?type=note", Some(TOKEN)).await;
        assert!(body.contains("Rule"), "{body}");
        let (_, body) = call(&app, "GET", &uri, Some(TOKEN)).await;
        assert!(body.contains("Due in January"));

        let patch = json!({"status": "archived"});
        let (status, body) = send(&app, "PATCH", &uri, Some(TOKEN), Some(patch)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("archived"));
        assert_eq!(
            call(&app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, "GET", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn invalid_entries_are_refused() {
        let (app, _) = app(Some(TOKEN)).await;
        let new = json!({"type": "secret", "title": "x", "body": "y"});
        let (status, _) = send(&app, "POST", "/brain/entries", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn uploads_return_their_original() {
        let (app, _) = app(Some(TOKEN)).await;
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/brain/files?name=memo.txt")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("plain content"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let entry: Value = serde_json::from_slice(&body).unwrap();
        let uri = format!("/brain/files/{}", entry["id"].as_str().unwrap());
        let (status, body) = call(&app, "GET", &uri, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "plain content");
        let unknown = format!("/brain/files/{}", Uuid::now_v7());
        assert_eq!(
            call(&app, "GET", &unknown, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn questions_reach_the_assistant() {
        let (app, _) = app(Some(TOKEN)).await;
        let ask = |question: &str| json!({"question": question});
        let (status, body) = send(&app, "POST", "/brain/ask", Some(TOKEN), Some(ask("why"))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("You asked why"));
        let (status, _) = send(&app, "POST", "/brain/ask", Some(TOKEN), Some(ask(" "))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _) = send(&app, "POST", "/brain/ask", Some(TOKEN), Some(ask("fail"))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn the_brain_needs_a_sign_in() {
        let (app, _) = app(Some(TOKEN)).await;
        for uri in ["/brain/search?q=x", "/brain/entries"] {
            assert_eq!(
                call(&app, "GET", uri, None).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[tokio::test]
    async fn agents_can_be_listed_drafted_saved_and_deleted() {
        let (app, _) = app(Some(TOKEN)).await;
        let (_, body) = call(&app, "GET", "/agents", Some(TOKEN)).await;
        assert!(body.contains("brief"));
        assert_eq!(
            call(&app, "GET", "/agents/brief", Some(TOKEN)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, "GET", "/agents/nobody", Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = call(&app, "GET", "/agents/brief/runs", Some(TOKEN)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));

        let draft = |text: &str| json!({"description": text});
        let (_, body) = send(
            &app,
            "POST",
            "/agents/draft",
            Some(TOKEN),
            Some(draft("watch")),
        )
        .await;
        assert!(body.contains("file for watch"));
        let (status, _) = send(&app, "POST", "/agents/draft", Some(TOKEN), Some(draft(" "))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _) = send(
            &app,
            "POST",
            "/agents/draft",
            Some(TOKEN),
            Some(draft("fail")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let file = |source: &str| json!({"source": source});
        let (status, _) = send(&app, "POST", "/agents", Some(TOKEN), Some(file("ok"))).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(&app, "POST", "/agents", Some(TOKEN), Some(file("bad"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        for (uri, expected) in [
            ("/agents/brief", StatusCode::NO_CONTENT),
            ("/agents/nobody", StatusCode::NOT_FOUND),
            ("/agents/fixed", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            assert_eq!(call(&app, "DELETE", uri, Some(TOKEN)).await.0, expected);
        }
    }

    #[tokio::test]
    async fn connectors_skills_and_costs_are_listed() {
        let (app, audit) = app(Some(TOKEN)).await;
        let run = RunId::new();
        audit.start_run(run, "brief", "manual").await.unwrap();
        let step = lighthouse_audit::NewStep {
            kind: "model".to_owned(),
            name: "m".to_owned(),
            status: "ok".to_owned(),
            detail: Some("provider=anthropic fallback=false".to_owned()),
            cost_eur: 0.5,
            ..Default::default()
        };
        audit.record_step(run, &step).await.unwrap();
        let (_, body) = call(&app, "GET", "/costs?days=7", Some(TOKEN)).await;
        let costs: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(costs[0]["provider"], "anthropic");
        assert_eq!(costs[0]["cost_eur"], 0.5);
        let (_, body) = call(&app, "GET", "/connectors", Some(TOKEN)).await;
        assert!(body.contains("linear"));
        let (_, body) = call(&app, "GET", "/skills", Some(TOKEN)).await;
        assert!(body.contains("Briefs"));
    }

    #[tokio::test]
    async fn a_node_reports_asks_and_receives_the_decision() {
        let (app, _) = app(Some(TOKEN)).await;
        let owner = signed_in(&app).await;
        let (_, body) = send(
            &app,
            "POST",
            "/auth/tokens",
            Some(&owner),
            Some(json!({"name": "mac", "kind": "node"})),
        )
        .await;
        let node: TokenIssued = serde_json::from_str(&body).unwrap();
        let report = json!({"name": "mac", "sessions": [{
            "id": "s1", "project": "billing", "branch": null, "status": "running",
            "summary": null,
        }]});
        let (status, body) = send(
            &app,
            "POST",
            "/node/report",
            Some(&node.token),
            Some(report),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"paused\":false"));
        let (_, body) = call(&app, "GET", "/sessions", Some(&owner)).await;
        assert!(body.contains("billing"));

        let ask = json!({"session_id": "s1", "project": "billing", "tool": "Bash",
                         "input": {"command": "ls"}});
        let (status, body) = send(
            &app,
            "POST",
            "/node/permissions",
            Some(&node.token),
            Some(ask),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let asked: Value = serde_json::from_str(&body).unwrap();
        let id = asked["id"].as_str().unwrap();

        let waiting = format!("/node/permissions/{id}?wait=0");
        let (_, body) = call(&app, "GET", &waiting, Some(&node.token)).await;
        assert!(body.contains("pending"));
        let (_, body) = call(&app, "GET", "/permissions?status=pending", Some(&owner)).await;
        assert!(body.contains("Bash"));

        let uri = format!("/permissions/{id}/decision");
        let (status, _) = send(
            &app,
            "POST",
            &uri,
            Some(&owner),
            Some(json!({"allow": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            &app,
            "POST",
            &uri,
            Some(&owner),
            Some(json!({"allow": false})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (_, body) = call(&app, "GET", &waiting, Some(&node.token)).await;
        assert!(body.contains("allowed"));

        let (_, body) = call(&app, "GET", "/nodes", Some(&owner)).await;
        let nodes: Value = serde_json::from_str(&body).unwrap();
        let uri = format!("/nodes/{}", nodes[0]["id"].as_str().unwrap());
        let (status, _) = send(
            &app,
            "PATCH",
            &uri,
            Some(&owner),
            Some(json!({"paused": true})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn node_endpoints_need_a_device_token() {
        let (app, _) = app(Some(TOKEN)).await;
        let report = json!({"name": "mac", "sessions": []});
        let (status, _) = send(&app, "POST", "/node/report", Some(TOKEN), Some(report)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let uri = format!("/nodes/{}", Uuid::now_v7());
        let (status, _) = send(
            &app,
            "PATCH",
            &uri,
            Some(TOKEN),
            Some(json!({"paused": true})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn answers_stream_their_text_and_end_with_the_result() {
        let (app, _) = app(Some(TOKEN)).await;
        let question = json!({"question": "why"});
        let (status, body) = send(
            &app,
            "POST",
            "/brain/ask/stream",
            Some(TOKEN),
            Some(question),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("event: delta"), "{body}");
        assert!(body.contains("event: done"), "{body}");
        let question = json!({"question": "fail"});
        let (_, body) = send(
            &app,
            "POST",
            "/brain/ask/stream",
            Some(TOKEN),
            Some(question),
        )
        .await;
        assert!(body.contains("event: failed"), "{body}");
    }

    #[tokio::test]
    async fn the_event_stream_announces_the_first_state() {
        let (app, _) = app(Some(TOKEN)).await;
        let request = HttpRequest::builder()
            .uri("/events")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let bytes = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("event: changed"));
    }

    #[tokio::test]
    async fn the_event_stream_passes_named_events_on() {
        let harness = harness(Some(TOKEN)).await;
        let request = HttpRequest::builder()
            .uri("/events")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = harness.app.clone().oneshot(request).await.unwrap();
        let mut body = response.into_body().into_data_stream();
        let mut seen = String::new();
        for _ in 0..5 {
            harness.events.emit("tasks.changed", json!({"id": null}));
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
            if seen.contains("event: tasks.changed") {
                return;
            }
        }
        panic!("no named event in {seen}");
    }

    #[tokio::test]
    async fn wiki_links_resolve() {
        let (app, _) = app(Some(TOKEN)).await;
        let new = json!({"type": "note", "title": "Tax logic", "body": "Rules"});
        send(&app, "POST", "/brain/entries", Some(TOKEN), Some(new)).await;
        let new = json!({"type": "note", "title": "Meeting", "body": "See [[Tax logic]]"});
        send(&app, "POST", "/brain/entries", Some(TOKEN), Some(new)).await;
        let (status, body) =
            call(&app, "GET", "/brain/resolve?title=tax%20logic", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        let resolved: ResolvedEntry = serde_json::from_str(&body).unwrap();
        let uri = format!("/brain/entries/{}", resolved.id);
        let (_, body) = call(&app, "GET", &uri, Some(TOKEN)).await;
        assert!(body.contains("Meeting"), "{body}");
        assert_eq!(
            call(&app, "GET", "/brain/resolve?title=nobody", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
