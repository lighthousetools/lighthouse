//! Desktop agents, Claude Code sessions and their permission requests.

use crate::auth::Caller;
use crate::{ApiError, ApiState, internal};
use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use lighthouse_sessions::{NewPermission, Node, NodeReport, NodeState, Permission, Session};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 100;
const MAX_WAIT_SECONDS: u64 = 60;
const POLL: Duration = Duration::from_millis(500);

fn node_of(caller: Caller) -> Result<Uuid, ApiError> {
    caller.0.ok_or_else(|| {
        ApiError(
            StatusCode::FORBIDDEN,
            "this endpoint needs a device token".to_owned(),
        )
    })
}

/// Parameters of a list.
#[derive(Deserialize)]
pub(crate) struct ListQuery {
    status: Option<String>,
    limit: Option<i64>,
}

/// Parameters of a wait.
#[derive(Deserialize)]
pub(crate) struct WaitQuery {
    wait: Option<u64>,
}

/// A decision on a permission request.
#[derive(Deserialize, ToSchema)]
pub struct PermissionDecision {
    /// Whether the tool may run.
    pub allow: bool,
}

/// A change to a node.
#[derive(Deserialize, ToSchema)]
pub struct NodeChange {
    /// Pauses remote actions and denies open requests.
    pub paused: bool,
}

/// Reports the state of a desktop agent.
#[utoipa::path(
    post,
    operation_id = "report_node",
    path = "/node/report",
    request_body = NodeReport,
    responses((status = 200, body = NodeState)),
    security(("bearer" = []))
)]
pub(crate) async fn report(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Json(report): Json<NodeReport>,
) -> Result<Json<NodeState>, ApiError> {
    let node = node_of(caller)?;
    let result = state.sessions.report(node, &report).await;
    Ok(Json(result.map_err(internal)?))
}

/// Forwards a permission request of a session.
#[utoipa::path(
    post,
    operation_id = "request_permission",
    path = "/node/permissions",
    request_body = NewPermission,
    responses((status = 201, body = Permission)),
    security(("bearer" = []))
)]
pub(crate) async fn request(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Json(new): Json<NewPermission>,
) -> Result<(StatusCode, Json<Permission>), ApiError> {
    let node = node_of(caller)?;
    let permission = state.sessions.request(node, &new).await.map_err(internal)?;
    Ok((StatusCode::CREATED, Json(permission)))
}

/// Waits for a decision. Answers once it is made or after `wait` seconds.
#[utoipa::path(
    get,
    path = "/node/permissions/{id}",
    params(
        ("id" = Uuid, Path, description = "Request ID"),
        ("wait" = Option<u64>, Query, description = "Seconds to wait, at most 60")
    ),
    responses((status = 200, body = Permission), (status = 404, description = "Unknown request")),
    security(("bearer" = []))
)]
pub(crate) async fn await_decision(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<Uuid>,
    Query(query): Query<WaitQuery>,
) -> Result<Json<Permission>, ApiError> {
    let node = node_of(caller)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_WAIT_SECONDS));
    let deadline = Instant::now() + wait;
    loop {
        let permission = state
            .sessions
            .node_request(node, id)
            .await
            .map_err(internal)?
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown request".to_owned()))?;
        if permission.status != "pending" || Instant::now() >= deadline {
            return Ok(Json(permission));
        }
        sleep(POLL).await;
    }
}

/// Lists the desktop agents.
#[utoipa::path(
    get,
    path = "/nodes",
    responses((status = 200, body = Vec<Node>)),
    security(("bearer" = []))
)]
pub(crate) async fn nodes(State(state): State<ApiState>) -> Result<Json<Vec<Node>>, ApiError> {
    Ok(Json(state.sessions.nodes().await.map_err(internal)?))
}

/// Pauses a desktop agent or lets it continue. This is the kill switch.
#[utoipa::path(
    patch,
    path = "/nodes/{id}",
    params(("id" = Uuid, Path, description = "Node ID")),
    request_body = NodeChange,
    responses((status = 204), (status = 404, description = "Unknown node")),
    security(("bearer" = []))
)]
pub(crate) async fn update_node(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(change): Json<NodeChange>,
) -> Result<StatusCode, ApiError> {
    if state
        .sessions
        .set_paused(id, change.paused)
        .await
        .map_err(internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "unknown node".to_owned()))
    }
}

/// Lists Claude Code sessions, latest first.
#[utoipa::path(
    get,
    path = "/sessions",
    params(("limit" = Option<i64>, Query, description = "Maximum number of sessions")),
    responses((status = 200, body = Vec<Session>)),
    security(("bearer" = []))
)]
pub(crate) async fn sessions(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Session>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    Ok(Json(
        state.sessions.sessions(limit).await.map_err(internal)?,
    ))
}

/// Lists permission requests, newest first.
#[utoipa::path(
    get,
    path = "/permissions",
    params(
        ("status" = Option<String>, Query, description = "pending, allowed, denied or expired"),
        ("limit" = Option<i64>, Query, description = "Maximum number of requests")
    ),
    responses((status = 200, body = Vec<Permission>)),
    security(("bearer" = []))
)]
pub(crate) async fn permissions(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Permission>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    let list = state
        .sessions
        .requests(query.status.as_deref(), limit)
        .await
        .map_err(internal)?;
    Ok(Json(list))
}

/// Decides a permission request. The first decision wins.
#[utoipa::path(
    post,
    operation_id = "decide_permission",
    path = "/permissions/{id}/decision",
    params(("id" = Uuid, Path, description = "Request ID")),
    request_body = PermissionDecision,
    responses(
        (status = 200, body = Permission),
        (status = 409, description = "Already decided, expired or unknown")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn decide(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(decision): Json<PermissionDecision>,
) -> Result<Json<Permission>, ApiError> {
    state
        .sessions
        .decide(id, decision.allow)
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "the request is no longer open".to_owned(),
            )
        })
}
