//! Standing rules for the assistant.

use crate::{ApiError, ApiState, Scoped, desk_error, not_found};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use lighthouse_desk::{NewRule, Rule, RulePatch};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

/// Filter of the rule list.
#[derive(Deserialize)]
pub(crate) struct RuleQuery {
    agent: Option<String>,
}

fn changed(state: &ApiState, id: Uuid) {
    state.events.emit("rules.changed", json!({"id": id}));
}

/// Lists rules, oldest first.
#[utoipa::path(
    get,
    operation_id = "list_rules",
    path = "/rules",
    params(("agent" = Option<String>, Query, description = "Only the enabled rules a run of this agent sees")),
    responses((status = 200, body = Vec<Rule>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    Scoped(state): Scoped,
    Query(query): Query<RuleQuery>,
) -> Result<Json<Vec<Rule>>, ApiError> {
    let rules = match query.agent {
        Some(agent) => state.desk.active_rules(&agent).await,
        None => state.desk.rules().await,
    };
    Ok(Json(rules.map_err(desk_error)?))
}

/// Creates a rule.
#[utoipa::path(
    post,
    operation_id = "create_rule",
    path = "/rules",
    request_body = NewRule,
    responses((status = 201, body = Rule), (status = 400, description = "Invalid rule")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    Scoped(state): Scoped,
    Json(rule): Json<NewRule>,
) -> Result<(StatusCode, Json<Rule>), ApiError> {
    let rule = state.desk.create_rule(rule).await.map_err(desk_error)?;
    changed(&state, rule.id);
    Ok((StatusCode::CREATED, Json(rule)))
}

/// Returns a rule.
#[utoipa::path(
    get,
    operation_id = "get_rule",
    path = "/rules/{id}",
    params(("id" = Uuid, Path, description = "Rule ID")),
    responses((status = 200, body = Rule), (status = 404, description = "Unknown rule")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Rule>, ApiError> {
    let rule = state.desk.rule(id).await.map_err(desk_error)?;
    rule.map(Json).ok_or_else(|| not_found("rule"))
}

/// Changes, enables or disables a rule.
#[utoipa::path(
    patch,
    operation_id = "update_rule",
    path = "/rules/{id}",
    params(("id" = Uuid, Path, description = "Rule ID")),
    request_body = RulePatch,
    responses(
        (status = 200, body = Rule),
        (status = 400, description = "Invalid change"),
        (status = 404, description = "Unknown rule")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(patch): Json<RulePatch>,
) -> Result<Json<Rule>, ApiError> {
    let rule = state
        .desk
        .update_rule(id, patch)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("rule"))?;
    changed(&state, id);
    Ok(Json(rule))
}

/// Deletes a rule.
#[utoipa::path(
    delete,
    operation_id = "delete_rule",
    path = "/rules/{id}",
    params(("id" = Uuid, Path, description = "Rule ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown rule")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_rule(id).await.map_err(desk_error)? {
        return Err(not_found("rule"));
    }
    changed(&state, id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};

    #[tokio::test]
    async fn rules_are_managed_and_filtered_by_agent() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let new = json!({"text": "Always ask before booking travel"});
        let (status, body) = send(app, "POST", "/rules", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let global: Rule = serde_json::from_str(&body).unwrap();
        assert!(global.enabled);
        let scoped =
            json!({"text": "Keep it short", "agent_id": "morning-brief", "category": "work"});
        send(app, "POST", "/rules", Some(TOKEN), Some(scoped)).await;
        let bad = json!({"text": ""});
        assert_eq!(
            send(app, "POST", "/rules", Some(TOKEN), Some(bad)).await.0,
            StatusCode::BAD_REQUEST
        );

        let (_, body) = call(app, "GET", "/rules?agent=weekly-report", Some(TOKEN)).await;
        let rules: Vec<Rule> = serde_json::from_str(&body).unwrap();
        assert_eq!(rules.len(), 1);
        let (_, body) = call(app, "GET", "/rules", Some(TOKEN)).await;
        let rules: Vec<Rule> = serde_json::from_str(&body).unwrap();
        assert_eq!(rules.len(), 2);

        let uri = format!("/rules/{}", global.id);
        let off = json!({"enabled": false});
        let (status, body) = send(app, "PATCH", &uri, Some(TOKEN), Some(off)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!serde_json::from_str::<Rule>(&body).unwrap().enabled);
        let (_, body) = call(app, "GET", "/rules?agent=weekly-report", Some(TOKEN)).await;
        assert_eq!(body, "[]");
        assert_eq!(call(app, "GET", &uri, Some(TOKEN)).await.0, StatusCode::OK);
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "GET", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let patch = json!({"text": "x"});
        assert_eq!(
            send(app, "PATCH", &uri, Some(TOKEN), Some(patch)).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
