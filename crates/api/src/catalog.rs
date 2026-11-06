//! Agents, connectors, skills and costs.

use crate::{ApiError, ApiState, internal};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lighthouse_audit::{DailyCost, RunRecord};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const DEFAULT_COST_DAYS: i32 = 30;
const AGENT_RUNS: i64 = 20;

/// A tool of an agent.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentTool {
    /// Qualified name.
    pub name: String,
    /// Declared level: `read`, `draft` or `write`.
    pub level: String,
}

/// An agent.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentInfo {
    /// Agent ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Cron expression, if it runs on a schedule.
    pub schedule: Option<String>,
    /// Next scheduled run.
    pub next_run: Option<DateTime<Utc>>,
    /// Tools.
    pub tools: Vec<AgentTool>,
    /// Skills.
    pub skills: Vec<String>,
    /// Delivery targets.
    pub deliver: Vec<String>,
    /// Tools that keep it from starting, with the reason.
    pub blocked: Vec<String>,
    /// Whether the apps can change it.
    pub editable: bool,
    /// The agent file.
    pub source: String,
}

/// A connector.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ConnectorInfo {
    /// Connector name.
    pub name: String,
    /// `none`, `bearer`, `oauth` or `process`.
    pub auth: String,
    /// Whether the needed credential is stored.
    pub connected: bool,
    /// Tools that run directly because they only read.
    pub read_tools: Vec<String>,
    /// Reversible tools.
    pub draft_tools: Vec<String>,
}

/// A skill.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SkillInfo {
    /// Skill name.
    pub name: String,
    /// What it is for.
    pub description: String,
}

/// Why an agent cannot be changed.
#[derive(Debug, thiserror::Error)]
pub enum AgentChangeError {
    /// The file is invalid or the agent is read-only.
    #[error("{0}")]
    Rejected(String),
    /// The server failed.
    #[error("{0}")]
    Failed(String),
}

/// What the server knows about its agents and connections.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// Lists all agents.
    fn agents(&self) -> Vec<AgentInfo>;
    /// Returns an agent.
    fn agent(&self, id: &str) -> Option<AgentInfo>;
    /// Writes an agent file from a description. It is not saved.
    async fn draft_agent(&self, description: &str) -> Result<String, String>;
    /// Validates and stores an agent file.
    async fn save_agent(&self, source: &str) -> Result<AgentInfo, AgentChangeError>;
    /// Deletes an agent. Returns whether it existed.
    async fn delete_agent(&self, id: &str) -> Result<bool, AgentChangeError>;
    /// Lists the connectors.
    async fn connectors(&self) -> Result<Vec<ConnectorInfo>, String>;
    /// Lists the skills.
    fn skills(&self) -> Vec<SkillInfo>;
}

/// A description of a new agent.
#[derive(Deserialize, ToSchema)]
pub struct AgentDraftRequest {
    /// What the agent should do.
    pub description: String,
}

/// An agent file.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AgentSource {
    /// File content.
    pub source: String,
}

/// Parameters of the cost report.
#[derive(Deserialize)]
pub(crate) struct CostQuery {
    days: Option<i32>,
}

fn change_error(error: AgentChangeError) -> ApiError {
    match error {
        AgentChangeError::Rejected(reason) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason),
        AgentChangeError::Failed(reason) => internal(reason),
    }
}

fn unknown_agent() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "unknown agent".to_owned())
}

/// Lists the agents.
#[utoipa::path(
    get,
    path = "/agents",
    responses((status = 200, body = Vec<AgentInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn agents(State(state): State<ApiState>) -> Json<Vec<AgentInfo>> {
    Json(state.catalog.agents())
}

/// Reads an agent.
#[utoipa::path(
    get,
    path = "/agents/{id}",
    params(("id" = String, Path, description = "Agent ID")),
    responses((status = 200, body = AgentInfo), (status = 404, description = "Unknown agent")),
    security(("bearer" = []))
)]
pub(crate) async fn agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<AgentInfo>, ApiError> {
    state.catalog.agent(&id).map(Json).ok_or_else(unknown_agent)
}

/// Lists the latest runs of an agent.
#[utoipa::path(
    get,
    path = "/agents/{id}/runs",
    params(("id" = String, Path, description = "Agent ID")),
    responses((status = 200, body = Vec<RunRecord>)),
    security(("bearer" = []))
)]
pub(crate) async fn agent_runs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<RunRecord>>, ApiError> {
    let runs = state
        .audit
        .list_agent_runs(&id, AGENT_RUNS)
        .await
        .map_err(internal)?;
    Ok(Json(runs))
}

/// Writes an agent file from a description, without saving it.
#[utoipa::path(
    post,
    path = "/agents/draft",
    request_body = AgentDraftRequest,
    responses(
        (status = 200, body = AgentSource),
        (status = 502, description = "The model wrote no valid agent")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn draft_agent(
    State(state): State<ApiState>,
    Json(request): Json<AgentDraftRequest>,
) -> Result<Json<AgentSource>, ApiError> {
    if request.description.trim().is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the description is empty".to_owned(),
        ));
    }
    let source = state
        .catalog
        .draft_agent(&request.description)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(AgentSource { source }))
}

/// Creates or replaces an agent. The ID comes from the file.
#[utoipa::path(
    post,
    path = "/agents",
    request_body = AgentSource,
    responses(
        (status = 200, body = AgentInfo),
        (status = 422, description = "Invalid file or read-only agent")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn save_agent(
    State(state): State<ApiState>,
    Json(request): Json<AgentSource>,
) -> Result<Json<AgentInfo>, ApiError> {
    let agent = state
        .catalog
        .save_agent(&request.source)
        .await
        .map_err(change_error)?;
    Ok(Json(agent))
}

/// Deletes an agent created in the apps.
#[utoipa::path(
    delete,
    path = "/agents/{id}",
    params(("id" = String, Path, description = "Agent ID")),
    responses(
        (status = 204),
        (status = 404, description = "Unknown agent"),
        (status = 422, description = "Read-only agent")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_agent(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .catalog
        .delete_agent(&id)
        .await
        .map_err(change_error)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(unknown_agent())
    }
}

/// Lists the connectors and whether they are connected.
#[utoipa::path(
    get,
    path = "/connectors",
    responses((status = 200, body = Vec<ConnectorInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn connectors(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ConnectorInfo>>, ApiError> {
    Ok(Json(state.catalog.connectors().await.map_err(internal)?))
}

/// Lists the skills.
#[utoipa::path(
    get,
    path = "/skills",
    responses((status = 200, body = Vec<SkillInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn skills(State(state): State<ApiState>) -> Json<Vec<SkillInfo>> {
    Json(state.catalog.skills())
}

/// Sums the model costs per day and provider.
#[utoipa::path(
    get,
    path = "/costs",
    params(("days" = Option<i32>, Query, description = "Days to look back")),
    responses((status = 200, body = Vec<DailyCost>)),
    security(("bearer" = []))
)]
pub(crate) async fn costs(
    State(state): State<ApiState>,
    Query(query): Query<CostQuery>,
) -> Result<Json<Vec<DailyCost>>, ApiError> {
    let days = query.days.unwrap_or(DEFAULT_COST_DAYS).clamp(1, 366);
    Ok(Json(state.audit.daily_costs(days).await.map_err(internal)?))
}
