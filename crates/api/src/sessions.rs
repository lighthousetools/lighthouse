//! Desktop agents, Claude Code sessions and their permission requests.

use crate::auth::Caller;
use crate::{ApiError, ApiState, internal};
use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use lighthouse_sessions::{
    ChangedFile, NewPermission, NewSessionEvent, Node, NodeReport, NodeState, Permission, Session,
    SessionEvent, SessionMessage,
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
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

const TIMELINE: i64 = 200;

/// Events a desktop agent reports.
#[derive(Deserialize, ToSchema)]
pub struct SessionEvents {
    /// At most 100 events.
    pub events: Vec<NewSessionEvent>,
}

/// A message to a session.
#[derive(Deserialize, ToSchema)]
pub struct NewSessionMessage {
    /// The next instruction for the session.
    pub text: String,
}

/// Everything known about a session.
#[derive(Serialize, serde::Deserialize, ToSchema)]
pub struct SessionDetail {
    /// The session.
    pub session: Session,
    /// The latest todo list of the session, as Claude Code wrote it.
    pub todos: Option<Value>,
    /// The latest plan the session presented.
    pub plan: Option<String>,
    /// Files it changed, latest first.
    pub files: Vec<ChangedFile>,
    /// Its latest 200 events, oldest first.
    pub timeline: Vec<SessionEvent>,
    /// Messages the owner sent to it.
    pub messages: Vec<SessionMessage>,
}

/// Stores what the sessions of this desktop agent did.
#[utoipa::path(
    post,
    operation_id = "report_session_events",
    path = "/node/events",
    request_body = SessionEvents,
    responses((status = 202, description = "Stored")),
    security(("bearer" = []))
)]
pub(crate) async fn record_events(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<SessionEvents>,
) -> Result<StatusCode, ApiError> {
    let node = node_of(caller)?;
    state
        .sessions
        .record_events(node, &body.events)
        .await
        .map_err(internal)?;
    let sessions: std::collections::BTreeSet<&str> = body
        .events
        .iter()
        .map(|event| event.session_id.as_str())
        .collect();
    for session in sessions {
        state.events.emit(
            "sessions.changed",
            json!({"node_id": node, "session_id": session}),
        );
    }
    Ok(StatusCode::ACCEPTED)
}

/// Hands over the messages the owner sent to a session of this desktop agent.
/// Waits up to `wait` seconds for one.
#[utoipa::path(
    get,
    operation_id = "take_session_messages",
    path = "/node/sessions/{id}/messages",
    params(
        ("id" = String, Path, description = "Session ID"),
        ("wait" = Option<u64>, Query, description = "Seconds to wait, at most 60")
    ),
    responses((status = 200, body = Vec<SessionMessage>)),
    security(("bearer" = []))
)]
pub(crate) async fn take_messages(
    State(state): State<ApiState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Result<Json<Vec<SessionMessage>>, ApiError> {
    let node = node_of(caller)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_WAIT_SECONDS));
    let deadline = Instant::now() + wait;
    loop {
        let messages = state
            .sessions
            .take_messages(node, &id)
            .await
            .map_err(internal)?;
        if !messages.is_empty() || Instant::now() >= deadline {
            if !messages.is_empty() {
                state.events.emit(
                    "sessions.changed",
                    json!({"node_id": node, "session_id": id}),
                );
            }
            return Ok(Json(messages));
        }
        sleep(POLL).await;
    }
}

/// Returns a session with its todo list, plan, changed files, timeline and messages.
#[utoipa::path(
    get,
    operation_id = "get_session",
    path = "/sessions/{node}/{id}",
    params(
        ("node" = Uuid, Path, description = "Node ID"),
        ("id" = String, Path, description = "Session ID")
    ),
    responses((status = 200, body = SessionDetail), (status = 404, description = "Unknown session")),
    security(("bearer" = []))
)]
pub(crate) async fn session(
    State(state): State<ApiState>,
    Path((node, id)): Path<(Uuid, String)>,
) -> Result<Json<SessionDetail>, ApiError> {
    let sessions = &state.sessions;
    let session = sessions
        .session(node, &id)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown session".to_owned()))?;
    let todos = sessions
        .latest_event(node, &id, "todos")
        .await
        .map_err(internal)?
        .map(|event| event.detail["todos"].clone());
    let plan = sessions
        .latest_event(node, &id, "plan")
        .await
        .map_err(internal)?
        .and_then(|event| event.detail["plan"].as_str().map(str::to_owned));
    Ok(Json(SessionDetail {
        session,
        todos,
        plan,
        files: sessions.changed_files(node, &id).await.map_err(internal)?,
        timeline: sessions
            .events(node, &id, None, TIMELINE)
            .await
            .map_err(internal)?,
        messages: sessions.messages(node, &id).await.map_err(internal)?,
    }))
}

/// Sends a message to a session. The desktop agent hands it over as the next
/// instruction when the session stops, see `docs/desktop-agent.md`.
#[utoipa::path(
    post,
    operation_id = "send_session_message",
    path = "/sessions/{node}/{id}/messages",
    params(
        ("node" = Uuid, Path, description = "Node ID"),
        ("id" = String, Path, description = "Session ID")
    ),
    request_body = NewSessionMessage,
    responses(
        (status = 201, body = SessionMessage),
        (status = 400, description = "Empty message"),
        (status = 404, description = "Unknown session")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn send_message(
    State(state): State<ApiState>,
    Path((node, id)): Path<(Uuid, String)>,
    Json(message): Json<NewSessionMessage>,
) -> Result<(StatusCode, Json<SessionMessage>), ApiError> {
    if message.text.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the message is empty".to_owned(),
        ));
    }
    let sent = state
        .sessions
        .send_message(node, &id, &message.text)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown session".to_owned()))?;
    state.events.emit(
        "sessions.changed",
        json!({"node_id": node, "session_id": id}),
    );
    Ok((StatusCode::CREATED, Json(sent)))
}

#[cfg(test)]
mod tests {
    use crate::TokenIssued;
    use crate::tests::{TOKEN, call, harness, send, signed_in};
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn a_session_reports_its_work_and_receives_messages() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let owner = signed_in(app).await;
        let (_, body) = send(
            app,
            "POST",
            "/auth/tokens",
            Some(&owner),
            Some(json!({"name": "mac", "kind": "node"})),
        )
        .await;
        let node: TokenIssued = serde_json::from_str(&body).unwrap();
        let report = json!({"name": "mac", "sessions": [{"id": "s1", "project": "app", "branch": null,
                            "status": "waiting", "summary": null}]});
        send(app, "POST", "/node/report", Some(&node.token), Some(report)).await;
        let (_, body) = call(app, "GET", "/sessions", Some(&owner)).await;
        let sessions: Value = serde_json::from_str(&body).unwrap();
        let node_id = sessions[0]["node_id"].as_str().unwrap().to_owned();

        let events = json!({"events": [
            {"session_id": "s1", "kind": "todos", "tool": "TodoWrite",
             "detail": {"todos": [{"content": "Add tests", "status": "in_progress"}]}},
            {"session_id": "s1", "kind": "plan", "tool": "ExitPlanMode", "detail": {"plan": "1. Tests"}},
            {"session_id": "s1", "kind": "file", "tool": "Edit", "file_path": "src/lib.rs",
             "detail": {"diff": "-a\n+b"}},
            {"session_id": "s1", "kind": "tool", "tool": "Bash", "summary": "cargo test"}
        ]});
        let (status, _) = send(app, "POST", "/node/events", Some(&node.token), Some(events)).await;
        assert_eq!(status, StatusCode::ACCEPTED);

        let uri = format!("/sessions/{node_id}/s1");
        let (status, body) = call(app, "GET", &uri, Some(&owner)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(detail["todos"][0]["content"], "Add tests");
        assert_eq!(detail["plan"], "1. Tests");
        assert_eq!(detail["files"][0]["path"], "src/lib.rs");
        assert_eq!(detail["timeline"].as_array().unwrap().len(), 4);
        let missing = format!("/sessions/{node_id}/nope");
        assert_eq!(
            call(app, "GET", &missing, Some(&owner)).await.0,
            StatusCode::NOT_FOUND
        );

        let messages = format!("{uri}/messages");
        let (status, _) = send(
            app,
            "POST",
            &messages,
            Some(&owner),
            Some(json!({"text": "Run the tests"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            send(
                app,
                "POST",
                &messages,
                Some(&owner),
                Some(json!({"text": " "}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let unknown = format!("/sessions/{node_id}/nope/messages");
        assert_eq!(
            send(
                app,
                "POST",
                &unknown,
                Some(&owner),
                Some(json!({"text": "x"}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (_, body) = call(
            app,
            "GET",
            "/node/sessions/s1/messages?wait=1",
            Some(&node.token),
        )
        .await;
        let taken: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(taken[0]["text"], "Run the tests");
        let (_, body) = call(
            app,
            "GET",
            "/node/sessions/s1/messages?wait=1",
            Some(&node.token),
        )
        .await;
        assert_eq!(body, "[]");
    }
}
