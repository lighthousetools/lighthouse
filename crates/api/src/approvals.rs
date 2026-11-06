//! Decisions on proposed actions.

use crate::{ApiError, ApiState, internal};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lighthouse_audit::Approval;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 100;

/// Filter of the approval list.
#[derive(Deserialize)]
pub(crate) struct ApprovalQuery {
    status: Option<String>,
    limit: Option<i64>,
}

/// A decision on one proposal.
#[derive(Deserialize, ToSchema)]
pub struct Decision {
    /// Whether the action may run.
    pub approve: bool,
    /// Replaces the proposed arguments.
    #[serde(default)]
    pub input: Option<Value>,
}

/// One decision on many proposals, for example a batch of similar ones.
#[derive(Deserialize, ToSchema)]
pub struct BatchDecision {
    /// The proposals.
    pub ids: Vec<Uuid>,
    /// Whether the actions may run.
    pub approve: bool,
}

/// Result of a batch decision.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct BatchResult {
    /// Proposals decided by this request.
    pub decided: Vec<Uuid>,
    /// Proposals already decided or expired.
    pub skipped: Vec<Uuid>,
}

async fn resume(state: &ApiState, run: Uuid) {
    if let Err(error) = state.trigger.resume(run).await {
        tracing::error!(%run, %error, "cannot resume the run");
    }
}

/// Lists proposals, newest first.
#[utoipa::path(
    get,
    operation_id = "list_approvals",
    path = "/approvals",
    params(
        ("status" = Option<String>, Query, description = "pending, approved, denied or expired"),
        ("limit" = Option<i64>, Query, description = "Maximum number of proposals")
    ),
    responses((status = 200, body = Vec<Approval>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<ApprovalQuery>,
) -> Result<Json<Vec<Approval>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    let approvals = state
        .audit
        .list_approvals(query.status.as_deref(), limit)
        .await
        .map_err(internal)?;
    Ok(Json(approvals))
}

/// Decides one proposal. The first decision wins.
#[utoipa::path(
    post,
    operation_id = "decide_approval",
    path = "/approvals/{id}/decision",
    params(("id" = Uuid, Path, description = "Approval ID")),
    request_body = Decision,
    responses(
        (status = 200, body = Approval),
        (status = 409, description = "Already decided, expired or unknown")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn decide(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(decision): Json<Decision>,
) -> Result<Json<Approval>, ApiError> {
    let approval = state
        .audit
        .decide_approval(id, decision.approve, decision.input.as_ref())
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "the proposal is no longer open".to_owned(),
            )
        })?;
    resume(&state, approval.run_id).await;
    Ok(Json(approval))
}

/// Decides many proposals at once.
#[utoipa::path(
    post,
    operation_id = "decide_approvals",
    path = "/approvals/decisions",
    request_body = BatchDecision,
    responses((status = 200, body = BatchResult)),
    security(("bearer" = []))
)]
pub(crate) async fn decide_batch(
    State(state): State<ApiState>,
    Json(batch): Json<BatchDecision>,
) -> Result<Json<BatchResult>, ApiError> {
    let mut result = BatchResult {
        decided: Vec::new(),
        skipped: Vec::new(),
    };
    let mut runs = Vec::new();
    for id in batch.ids {
        let approval = state
            .audit
            .decide_approval(id, batch.approve, None)
            .await
            .map_err(internal)?;
        match approval {
            Some(approval) => {
                result.decided.push(id);
                if !runs.contains(&approval.run_id) {
                    runs.push(approval.run_id);
                }
            }
            None => result.skipped.push(id),
        }
    }
    for run in runs {
        resume(&state, run).await;
    }
    Ok(Json(result))
}
