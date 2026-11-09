//! Agents, connectors, skills and costs.

use crate::{ApiError, ApiState, Scoped, internal};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lighthouse_audit::{DailyCost, RunRecord};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

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
    /// The model its runs use, from the routing of the `text` task. Display only.
    pub model: Option<ModelRoute>,
    /// How far it acts on its own: `observe` (reads only), `prepare` (also saves
    /// drafts) or `propose` (also proposes actions that wait for approval).
    pub autonomy: String,
    /// The cost limit of one run, in EUR.
    pub max_cost_eur: f64,
    /// Proposals of it the owner denied or changed before approving.
    #[serde(default)]
    pub corrections: i64,
    /// Where it comes from: `instance`, `tenant` or `personal`.
    #[serde(default = "instance")]
    pub layer: String,
    /// Whether it may run for the caller.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    #[serde(default)]
    pub disabled_by: Option<String>,
}

fn instance() -> String {
    "instance".to_owned()
}

const fn enabled() -> bool {
    true
}

/// Which provider and model serve a task, from the configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelRoute {
    /// The task, such as `text`.
    pub task: String,
    /// The provider.
    pub provider: String,
    /// Its model for the task, if it names one.
    pub model: Option<String>,
    /// The fallback provider, if any.
    pub fallback: Option<String>,
    /// The fallback's model for the task.
    pub fallback_model: Option<String>,
}

/// The autonomy of an agent with these tools.
pub fn autonomy(tools: &[AgentTool]) -> &'static str {
    let has = |level: &str| tools.iter().any(|tool| tool.level == level);
    if has("write") {
        "propose"
    } else if has("draft") {
        "prepare"
    } else {
        "observe"
    }
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

/// A tool a connector offers, with its level now and the level its server suggests.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConnectorTool {
    /// Unqualified name.
    pub name: String,
    /// What it does, as its server says.
    pub description: String,
    /// `read`, `draft` or `write` under the current lists.
    pub level: String,
    /// `read` when the server marks it read-only, else `write`. Only a suggestion: servers describe their own tools.
    pub suggested: String,
    /// The server marks it as destructive.
    pub destructive: bool,
}

/// A skill.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SkillInfo {
    /// Skill name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Where it comes from: `instance`, `tenant` or `personal`.
    #[serde(default = "instance")]
    pub layer: String,
    /// Whether it is in use for the caller.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    #[serde(default)]
    pub disabled_by: Option<String>,
    /// The file, for one created in the apps.
    #[serde(default)]
    pub source: Option<String>,
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
    /// Validates and stores an agent file: for a member with an owner, else for
    /// the tenant.
    async fn save_agent(
        &self,
        source: &str,
        owner: Option<Uuid>,
    ) -> Result<AgentInfo, AgentChangeError>;
    /// Deletes an agent of a layer. Returns whether it existed.
    async fn delete_agent(&self, id: &str, owner: Option<Uuid>) -> Result<bool, AgentChangeError>;
    /// Switches an `agent`, `automation` or `skill` on or off for a layer.
    /// Returns whether it exists.
    async fn switch(
        &self,
        _kind: &str,
        _id: &str,
        _owner: Option<Uuid>,
        _enabled: bool,
    ) -> Result<bool, AgentChangeError> {
        Ok(false)
    }
    /// Validates and stores a skill file.
    async fn save_skill(
        &self,
        _name: &str,
        _source: &str,
        _owner: Option<Uuid>,
    ) -> Result<(), AgentChangeError> {
        Err(AgentChangeError::Failed("no skills here".to_owned()))
    }
    /// Deletes a skill of a layer. Returns whether it existed.
    async fn delete_skill(
        &self,
        _name: &str,
        _owner: Option<Uuid>,
    ) -> Result<bool, AgentChangeError> {
        Ok(false)
    }
    /// Lists the connectors.
    async fn connectors(&self) -> Result<Vec<ConnectorInfo>, String>;
    /// Lists the tools a connector offers the caller, by asking its server.
    async fn connector_tools(&self, connector: &str) -> Result<Vec<ConnectorTool>, String> {
        Err(format!("cannot list the tools of {connector}"))
    }
    /// Lists the skills.
    fn skills(&self) -> Vec<SkillInfo>;
    /// The model routes of all tasks, from the configuration.
    fn models(&self) -> Vec<ModelRoute> {
        Vec::new()
    }
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

/// Which layer a change of an agent or automation is for.
#[derive(Deserialize)]
pub(crate) struct LayerQuery {
    layer: Option<String>,
}

/// Whose a new file becomes: the named layer, else the tenant's for owners and
/// admins and their own for members.
pub(crate) fn save_owner(
    query: &LayerQuery,
    caller: &crate::Caller,
) -> Result<Option<Uuid>, ApiError> {
    let personal = match query.layer.as_deref() {
        Some("me") => true,
        Some("tenant") => false,
        Some(_) => return Err(ApiError(StatusCode::NOT_FOUND, "unknown layer".to_owned())),
        None => !caller.role.can_manage(),
    };
    named_owner(personal, caller)
}

/// Whose file a deletion means: the named layer, else the layer it comes from.
pub(crate) fn delete_owner(
    query: &LayerQuery,
    caller: &crate::Caller,
    layer: Option<&str>,
) -> Result<Option<Uuid>, ApiError> {
    let personal = match query.layer.as_deref() {
        Some("me") => true,
        Some("tenant") => false,
        Some(_) => return Err(ApiError(StatusCode::NOT_FOUND, "unknown layer".to_owned())),
        None => layer == Some("personal"),
    };
    named_owner(personal, caller)
}

fn named_owner(personal: bool, caller: &crate::Caller) -> Result<Option<Uuid>, ApiError> {
    if !personal {
        caller.require_manager()?;
        return Ok(None);
    }
    caller.user.map(Some).ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "the admin token has no personal layer".to_owned(),
        )
    })
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
pub(crate) async fn agents(Scoped(state): Scoped) -> Json<Vec<AgentInfo>> {
    let corrections = corrections(&state).await;
    Json(
        state
            .catalog
            .agents()
            .into_iter()
            .map(|agent| with_corrections(agent, &corrections))
            .collect(),
    )
}

async fn corrections(state: &ApiState) -> std::collections::HashMap<String, i64> {
    state.audit.corrections().await.unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot count the corrections of agents");
        std::collections::HashMap::new()
    })
}

fn with_corrections(
    mut agent: AgentInfo,
    counts: &std::collections::HashMap<String, i64>,
) -> AgentInfo {
    agent.corrections = counts.get(&agent.id).copied().unwrap_or_default();
    agent
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
    Scoped(state): Scoped,
    Path(id): Path<String>,
) -> Result<Json<AgentInfo>, ApiError> {
    let agent = state.catalog.agent(&id).ok_or_else(unknown_agent)?;
    Ok(Json(with_corrections(agent, &corrections(&state).await)))
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
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
    params(("layer" = Option<String>, Query, description = "`tenant` or `me`. By default the tenant for owners and admins, their own for members")),
    request_body = AgentSource,
    responses(
        (status = 200, body = AgentInfo),
        (status = 422, description = "Invalid file or read-only agent")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn save_agent(
    Scoped(state): Scoped,
    caller: crate::Caller,
    Query(query): Query<LayerQuery>,
    Json(request): Json<AgentSource>,
) -> Result<Json<AgentInfo>, ApiError> {
    let owner = save_owner(&query, &caller)?;
    let agent = state
        .catalog
        .save_agent(&request.source, owner)
        .await
        .map_err(change_error)?;
    Ok(Json(agent))
}

/// Deletes an agent created in the apps.
#[utoipa::path(
    delete,
    path = "/agents/{id}",
    params(("id" = String, Path, description = "Agent ID"),
        ("layer" = Option<String>, Query, description = "`tenant` or `me`. By default the layer it comes from")),
    responses(
        (status = 204),
        (status = 404, description = "Unknown agent"),
        (status = 422, description = "Read-only agent")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete_agent(
    Scoped(state): Scoped,
    caller: crate::Caller,
    Query(query): Query<LayerQuery>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let layer = state.catalog.agent(&id).map(|agent| agent.layer);
    let owner = delete_owner(&query, &caller, layer.as_deref())?;
    if state
        .catalog
        .delete_agent(&id, owner)
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
    Scoped(state): Scoped,
) -> Result<Json<Vec<ConnectorInfo>>, ApiError> {
    Ok(Json(state.catalog.connectors().await.map_err(internal)?))
}

/// Lists the tools a connector offers, with their level now and a suggestion from its server.
#[utoipa::path(
    get,
    path = "/connectors/{id}/tools",
    params(("id" = String, Path, description = "Connector name")),
    responses((status = 200, body = Vec<ConnectorTool>), (status = 502, description = "The server could not be asked")),
    security(("bearer" = []))
)]
pub(crate) async fn connector_tools(
    Scoped(state): Scoped,
    Path(id): Path<String>,
) -> Result<Json<Vec<ConnectorTool>>, ApiError> {
    state
        .catalog
        .connector_tools(&id)
        .await
        .map(Json)
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))
}

/// Lists the skills.
#[utoipa::path(
    get,
    path = "/skills",
    responses((status = 200, body = Vec<SkillInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn skills(Scoped(state): Scoped) -> Json<Vec<SkillInfo>> {
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
    Scoped(state): Scoped,
    Query(query): Query<CostQuery>,
) -> Result<Json<Vec<DailyCost>>, ApiError> {
    let days = query.days.unwrap_or(DEFAULT_COST_DAYS).clamp(1, 366);
    Ok(Json(state.audit.daily_costs(days).await.map_err(internal)?))
}

/// Sums the cost of runs per user of the tenant. Needs the owner or admin role.
#[utoipa::path(
    get,
    path = "/costs/users",
    params(("days" = Option<i32>, Query, description = "Days to look back")),
    responses((status = 200, body = Vec<lighthouse_audit::UserCost>),
        (status = 403, description = "Needs the owner or admin role")),
    security(("bearer" = []))
)]
pub(crate) async fn user_costs(
    Scoped(state): Scoped,
    caller: crate::Caller,
    Query(query): Query<CostQuery>,
) -> Result<Json<Vec<lighthouse_audit::UserCost>>, ApiError> {
    caller.require_manager()?;
    let days = query.days.unwrap_or(DEFAULT_COST_DAYS).clamp(1, 366);
    Ok(Json(
        state.audit.costs_by_user(days).await.map_err(internal)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness};
    use lighthouse_audit::NewApproval;
    use lighthouse_core::RunId;
    use serde_json::{Value, json};

    fn tool(level: &str) -> AgentTool {
        AgentTool {
            name: format!("demo.{level}"),
            level: level.to_owned(),
        }
    }

    #[test]
    fn autonomy_follows_the_strongest_tool() {
        assert_eq!(autonomy(&[]), "observe");
        assert_eq!(autonomy(&[tool("read"), tool("draft")]), "prepare");
        assert_eq!(autonomy(&[tool("read"), tool("write")]), "propose");
    }

    #[tokio::test]
    async fn agents_show_how_often_the_owner_corrected_them() {
        let harness = harness(Some(TOKEN)).await;
        let run = RunId::new();
        harness
            .audit
            .start_run(run, "brief", "manual")
            .await
            .unwrap();
        let id = Uuid::now_v7();
        let proposal = NewApproval {
            id,
            tool: "mail.send".to_owned(),
            input: json!({"to": "a"}),
            quotes_external: false,
            reason: None,
            consulted: Vec::new(),
        };
        harness
            .audit
            .pause_run(run, "brief", &json!({}), &[proposal])
            .await
            .unwrap();
        harness
            .audit
            .decide_approval(id, false, None)
            .await
            .unwrap();
        let (_, body) = call(&harness.app, "GET", "/agents", Some(TOKEN)).await;
        let agents: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(agents[0]["corrections"], 1, "{body}");
        assert_eq!(agents[0]["autonomy"], "observe");
        let (_, body) = call(&harness.app, "GET", "/agents/brief", Some(TOKEN)).await;
        let agent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(agent["corrections"], 1);
    }
}
