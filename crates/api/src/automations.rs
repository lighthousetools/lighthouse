//! Automations: a trigger and ordered steps, stored as files like agents.

use crate::catalog::{AgentChangeError, AgentSource};
use crate::{ApiError, ApiState, Scoped, TriggerError, internal, not_found};
use async_trait::async_trait;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lighthouse_audit::RunRecord;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

const RUNS: i64 = 50;

/// The trigger of an automation.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AutomationTriggerInfo {
    /// `cron`, `event`, `webhook` or `manual`.
    pub kind: String,
    /// Cron expression.
    pub cron: Option<String>,
    /// Event name, such as `run.failed` or `mail.received`.
    pub event: Option<String>,
    /// Fields the event or webhook payload must contain.
    pub filter: BTreeMap<String, String>,
}

/// One step of an automation.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AutomationStepInfo {
    /// `agent`, `tool`, `notify` or `if`.
    pub kind: String,
    /// What it does, in a few words.
    pub summary: String,
    /// Whether it waits for the owner's approval before it acts.
    pub needs_approval: bool,
}

/// An automation.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AutomationInfo {
    /// Where it comes from: `instance`, `tenant` or `personal`.
    #[serde(default)]
    pub layer: String,
    /// The layer that switched it off, if one did.
    #[serde(default)]
    pub disabled_by: Option<String>,
    /// Automation ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Whether it runs on its trigger.
    pub enabled: bool,
    /// Trigger.
    pub trigger: AutomationTriggerInfo,
    /// Steps in order.
    pub steps: Vec<AutomationStepInfo>,
    /// Next scheduled run.
    pub next_run: Option<DateTime<Utc>>,
    /// Whether the apps can change the file.
    pub editable: bool,
    /// The file.
    pub source: String,
}

/// Switches an automation on or off.
#[derive(Deserialize, ToSchema)]
pub struct AutomationSwitch {
    /// Whether it runs on its trigger.
    pub enabled: bool,
}

/// An event from outside, such as a new mail reported by a connector.
#[derive(Deserialize, ToSchema)]
pub struct ExternalEvent {
    /// Event name like `mail.received`. Names starting with `run.` are reserved.
    pub name: String,
    /// Payload that triggers match against and steps read.
    #[serde(default)]
    pub payload: Value,
}

/// Runs started by a request.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AutomationRuns {
    /// IDs of the started runs.
    pub runs: Vec<Uuid>,
}

/// What a dry run would do.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AutomationDryRun {
    /// Automation ID.
    pub id: String,
    /// Whether an event or webhook payload matches the trigger. `None` for other triggers.
    pub trigger_matches: Option<bool>,
    /// Every step, branches included, with filled placeholders.
    pub steps: Vec<PlannedStep>,
}

/// A step of a dry run. Nothing runs and nothing is sent.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PlannedStep {
    /// Position, such as `2` or `2.then.1`.
    pub path: String,
    /// `agent`, `tool`, `notify`, `if` or `branch`.
    pub kind: String,
    /// What it would do.
    pub summary: String,
    /// Tool arguments with the payload filled in.
    pub input: Option<Value>,
    /// Message with the payload filled in.
    pub message: Option<String>,
    /// Whether it would wait for the owner's approval.
    pub needs_approval: bool,
}

/// What to try in a dry run.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct DryRunRequest {
    /// A saved automation.
    #[serde(default)]
    pub id: Option<String>,
    /// Or an automation file that is not saved yet.
    #[serde(default)]
    pub source: Option<String>,
    /// The event or webhook payload to try.
    #[serde(default)]
    pub payload: Value,
}

/// A sentence that describes an automation.
#[derive(Deserialize, ToSchema)]
pub struct AutomationDraftRequest {
    /// What the automation should do.
    pub description: String,
}

/// Starts and manages automations.
#[async_trait]
pub trait Automations: Send + Sync {
    /// Lists all automations.
    async fn list(&self) -> Result<Vec<AutomationInfo>, String>;
    /// Returns an automation.
    async fn get(&self, id: &str) -> Result<Option<AutomationInfo>, String>;
    /// Validates and stores an automation file.
    async fn save(
        &self,
        source: &str,
        owner: Option<Uuid>,
    ) -> Result<AutomationInfo, AgentChangeError>;
    /// Deletes an automation. Returns whether it existed.
    async fn delete(&self, id: &str, owner: Option<Uuid>) -> Result<bool, AgentChangeError>;
    /// Switches an automation on or off. Returns `None` for an unknown one.
    async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<AutomationInfo>, AgentChangeError>;
    /// Starts a run now, whatever the trigger.
    async fn run(&self, id: &str, payload: Value) -> Result<Uuid, TriggerError>;
    /// Handles a call of the webhook of an automation. Returns `None` when the
    /// payload does not match its filter.
    async fn webhook(&self, id: &str, payload: Value) -> Result<Option<Uuid>, TriggerError>;
    /// Starts every enabled automation that waits for this event.
    async fn emit(&self, name: &str, payload: Value) -> Result<Vec<Uuid>, String>;
    /// Shows what an automation would do with a payload, without running it.
    async fn dry_run(&self, request: DryRunRequest) -> Result<AutomationDryRun, AgentChangeError>;
    /// Writes an automation file from a sentence. It is not saved.
    async fn draft(&self, description: &str) -> Result<String, String>;
}

fn trigger_error(error: TriggerError) -> ApiError {
    match error {
        TriggerError::UnknownAgent(_) => not_found("automation"),
        TriggerError::Refused(reason) => ApiError(StatusCode::CONFLICT, reason),
        TriggerError::Failed(_) => internal(error),
    }
}

fn change_error(error: AgentChangeError) -> ApiError {
    match error {
        AgentChangeError::Rejected(reason) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason),
        AgentChangeError::Failed(reason) => internal(reason),
    }
}

fn changed(state: &ApiState, id: &str) {
    state.events.emit("automations.changed", json!({"id": id}));
}

/// Lists the automations.
#[utoipa::path(
    get,
    operation_id = "list_automations",
    path = "/automations",
    responses((status = 200, body = Vec<AutomationInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(Scoped(state): Scoped) -> Result<Json<Vec<AutomationInfo>>, ApiError> {
    Ok(Json(state.automations.list().await.map_err(internal)?))
}

/// Returns an automation.
#[utoipa::path(
    get,
    operation_id = "get_automation",
    path = "/automations/{id}",
    params(("id" = String, Path, description = "Automation ID")),
    responses((status = 200, body = AutomationInfo), (status = 404, description = "Unknown automation")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    Scoped(state): Scoped,
    Path(id): Path<String>,
) -> Result<Json<AutomationInfo>, ApiError> {
    let automation = state.automations.get(&id).await.map_err(internal)?;
    automation.map(Json).ok_or_else(|| not_found("automation"))
}

/// Creates or replaces an automation. The ID comes from the file.
#[utoipa::path(
    post,
    operation_id = "save_automation",
    path = "/automations",
    request_body = AgentSource,
    responses(
        (status = 200, body = AutomationInfo),
        (status = 422, description = "Invalid file or read-only automation")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn save(
    Scoped(state): Scoped,
    caller: crate::Caller,
    axum::extract::Query(query): axum::extract::Query<crate::catalog::LayerQuery>,
    Json(request): Json<AgentSource>,
) -> Result<Json<AutomationInfo>, ApiError> {
    let owner = crate::catalog::save_owner(&query, &caller)?;
    let automation = state
        .automations
        .save(&request.source, owner)
        .await
        .map_err(change_error)?;
    changed(&state, &automation.id);
    Ok(Json(automation))
}

/// Switches an automation on or off without changing its file.
#[utoipa::path(
    patch,
    operation_id = "switch_automation",
    path = "/automations/{id}",
    params(("id" = String, Path, description = "Automation ID")),
    request_body = AutomationSwitch,
    responses((status = 200, body = AutomationInfo), (status = 404, description = "Unknown automation")),
    security(("bearer" = []))
)]
pub(crate) async fn switch(
    Scoped(state): Scoped,
    caller: crate::Caller,
    Path(id): Path<String>,
    Json(switch): Json<AutomationSwitch>,
) -> Result<Json<AutomationInfo>, ApiError> {
    // One that belongs to the caller is theirs to switch; a shared one is for
    // owners and admins, and a member switches it off for themselves in
    // `/settings/me/automations/{id}/enabled`.
    let known = state.automations.get(&id).await.map_err(crate::internal)?;
    if known.is_some_and(|automation| automation.layer != "personal") {
        caller.require_manager()?;
    }
    let automation = state
        .automations
        .set_enabled(&id, switch.enabled)
        .await
        .map_err(change_error)?
        .ok_or_else(|| not_found("automation"))?;
    changed(&state, &id);
    Ok(Json(automation))
}

/// Deletes an automation created in the apps.
#[utoipa::path(
    delete,
    operation_id = "delete_automation",
    path = "/automations/{id}",
    params(("id" = String, Path, description = "Automation ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Unknown automation"),
        (status = 422, description = "Managed as a file on the server")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    caller: crate::Caller,
    axum::extract::Query(query): axum::extract::Query<crate::catalog::LayerQuery>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let layer = state
        .automations
        .get(&id)
        .await
        .map_err(crate::internal)?
        .map(|automation| automation.layer);
    let owner = crate::catalog::delete_owner(&query, &caller, layer.as_deref())?;
    if !state
        .automations
        .delete(&id, owner)
        .await
        .map_err(change_error)?
    {
        return Err(not_found("automation"));
    }
    changed(&state, &id);
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the runs of an automation, newest first.
#[utoipa::path(
    get,
    operation_id = "automation_runs",
    path = "/automations/{id}/runs",
    params(("id" = String, Path, description = "Automation ID")),
    responses((status = 200, body = Vec<RunRecord>)),
    security(("bearer" = []))
)]
pub(crate) async fn runs(
    Scoped(state): Scoped,
    Path(id): Path<String>,
) -> Result<Json<Vec<RunRecord>>, ApiError> {
    let agent = format!("{}{id}", lighthouse_core::automation::RUN_PREFIX);
    let runs = state
        .audit
        .list_agent_runs(&agent, RUNS)
        .await
        .map_err(internal)?;
    Ok(Json(runs))
}

#[derive(Deserialize)]
pub(crate) struct PayloadBody {
    #[serde(default)]
    payload: Value,
}

/// Starts an automation now, whatever its trigger, for example to test it.
#[utoipa::path(
    post,
    operation_id = "run_automation",
    path = "/automations/{id}/run",
    params(("id" = String, Path, description = "Automation ID")),
    request_body(content = Value, description = "Optional `payload` that the steps read as the event"),
    responses((status = 202, body = crate::RunStarted), (status = 404, description = "Unknown automation")),
    security(("bearer" = []))
)]
pub(crate) async fn run(
    Scoped(state): Scoped,
    Path(id): Path<String>,
    body: Option<Json<PayloadBody>>,
) -> Result<(StatusCode, Json<crate::RunStarted>), ApiError> {
    let payload = body.map(|Json(body)| body.payload).unwrap_or(Value::Null);
    let run = state
        .automations
        .run(&id, payload)
        .await
        .map_err(trigger_error)?;
    Ok((StatusCode::ACCEPTED, Json(crate::RunStarted { id: run })))
}

/// Calls the webhook of an automation with a JSON payload.
///
/// It needs a bearer token like every endpoint. Issue a device token for the
/// service that calls it.
#[utoipa::path(
    post,
    operation_id = "automation_webhook",
    path = "/automations/{id}/webhook",
    params(("id" = String, Path, description = "Automation ID")),
    request_body(content = Value, description = "Any JSON payload"),
    responses(
        (status = 202, body = AutomationRuns),
        (status = 404, description = "Unknown automation"),
        (status = 409, description = "Not a webhook automation, or switched off")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn webhook(
    Scoped(state): Scoped,
    Path(id): Path<String>,
    payload: Option<Json<Value>>,
) -> Result<(StatusCode, Json<AutomationRuns>), ApiError> {
    let payload = payload.map(|Json(value)| value).unwrap_or(Value::Null);
    let run = state
        .automations
        .webhook(&id, payload)
        .await
        .map_err(trigger_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(AutomationRuns {
            runs: run.into_iter().collect(),
        }),
    ))
}

/// Reports an event from outside, such as `mail.received`, and starts the
/// automations that wait for it.
#[utoipa::path(
    post,
    operation_id = "emit_event",
    path = "/automations/events",
    request_body = ExternalEvent,
    responses(
        (status = 202, body = AutomationRuns),
        (status = 400, description = "Invalid or reserved event name")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn emit(
    Scoped(state): Scoped,
    Json(event): Json<ExternalEvent>,
) -> Result<(StatusCode, Json<AutomationRuns>), ApiError> {
    if !lighthouse_core::automation::valid_event_name(&event.name) || event.name.starts_with("run.")
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("`{}` is not an event name you can report", event.name),
        ));
    }
    let runs = state
        .automations
        .emit(&event.name, event.payload)
        .await
        .map_err(internal)?;
    Ok((StatusCode::ACCEPTED, Json(AutomationRuns { runs })))
}

/// Shows what an automation would do with a payload. Nothing runs and nothing is sent.
#[utoipa::path(
    post,
    operation_id = "dry_run_automation",
    path = "/automations/dry-run",
    request_body = DryRunRequest,
    responses(
        (status = 200, body = AutomationDryRun),
        (status = 404, description = "Unknown automation"),
        (status = 422, description = "Invalid file or neither ID nor file")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn dry_run(
    Scoped(state): Scoped,
    Json(request): Json<DryRunRequest>,
) -> Result<Json<AutomationDryRun>, ApiError> {
    let unknown = request.source.is_none() && request.id.is_some();
    let result = state.automations.dry_run(request).await;
    match result {
        Ok(plan) => Ok(Json(plan)),
        Err(AgentChangeError::Rejected(_)) if unknown => Err(not_found("automation")),
        Err(error) => Err(change_error(error)),
    }
}

/// Writes an automation file from a sentence for the owner to review. It is not saved.
#[utoipa::path(
    post,
    operation_id = "draft_automation",
    path = "/automations/draft",
    request_body = AutomationDraftRequest,
    responses(
        (status = 200, body = AgentSource),
        (status = 502, description = "The model wrote no valid automation")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn draft(
    Scoped(state): Scoped,
    Json(request): Json<AutomationDraftRequest>,
) -> Result<Json<AgentSource>, ApiError> {
    let source = state
        .automations
        .draft(&request.description)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(AgentSource { source }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};

    #[tokio::test]
    async fn automations_are_listed_changed_and_started() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let (_, body) = call(app, "GET", "/automations", Some(TOKEN)).await;
        assert!(body.contains("follow-up"));
        assert_eq!(
            call(app, "GET", "/automations/follow-up", Some(TOKEN))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "GET", "/automations/nope", Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );

        let source = json!({"source": "valid"});
        let (status, _) = send(app, "POST", "/automations", Some(TOKEN), Some(source)).await;
        assert_eq!(status, StatusCode::OK);
        let bad = json!({"source": "bad"});
        assert_eq!(
            send(app, "POST", "/automations", Some(TOKEN), Some(bad))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let off = json!({"enabled": false});
        let (status, body) = send(
            app,
            "PATCH",
            "/automations/follow-up",
            Some(TOKEN),
            Some(off.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"enabled\":false"));
        assert_eq!(
            send(app, "PATCH", "/automations/nope", Some(TOKEN), Some(off))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let (status, _) = call(app, "POST", "/automations/follow-up/run", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let payload = json!({"payload": {"from": "a@example.com"}});
        let (status, _) = send(
            app,
            "POST",
            "/automations/follow-up/run",
            Some(TOKEN),
            Some(payload),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            call(app, "POST", "/automations/nope/run", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let hook = json!({"from": "a@example.com"});
        let (status, body) = send(
            app,
            "POST",
            "/automations/follow-up/webhook",
            Some(TOKEN),
            Some(hook),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("runs"));
        assert_eq!(
            call(app, "POST", "/automations/manual/webhook", Some(TOKEN))
                .await
                .0,
            StatusCode::CONFLICT
        );

        let event = json!({"name": "mail.received", "payload": {"from": "a@example.com"}});
        let (status, body) =
            send(app, "POST", "/automations/events", Some(TOKEN), Some(event)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains(&Uuid::nil().to_string()));
        for name in ["run.failed", "nodot"] {
            let event = json!({"name": name});
            assert_eq!(
                send(app, "POST", "/automations/events", Some(TOKEN), Some(event))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let (status, body) = call(app, "GET", "/automations/follow-up/runs", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]");

        assert_eq!(
            call(app, "DELETE", "/automations/follow-up", Some(TOKEN))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", "/automations/nope", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", "/automations/fixed", Some(TOKEN))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
