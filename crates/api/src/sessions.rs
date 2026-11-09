//! Desktop agents, Claude Code sessions and their permission requests.

use crate::auth::Caller;
use crate::{ApiError, Scoped, internal};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use lighthouse_desk::TaskView;
use lighthouse_sessions::{
    ChangedFile, Launch, LaunchQueue, LaunchReport, NewLaunch, NewPermission, NewSessionEvent,
    Node, NodeReport, NodeState, Permission, Session, SessionEvent, SessionMessage,
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
    caller.device.filter(|_| caller.node).ok_or_else(|| {
        ApiError(
            StatusCode::FORBIDDEN,
            "this endpoint needs the token of a desktop agent".to_owned(),
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
    Scoped(state): Scoped,
    caller: Caller,
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
    Scoped(state): Scoped,
    caller: Caller,
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
    Scoped(state): Scoped,
    caller: Caller,
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
pub(crate) async fn nodes(Scoped(state): Scoped) -> Result<Json<Vec<Node>>, ApiError> {
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
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
    /// Costs and tokens, if Claude Code exports its OpenTelemetry metrics.
    pub usage: Option<lighthouse_sessions::SessionUsage>,
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
    Scoped(state): Scoped,
    caller: Caller,
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

/// Takes the OpenTelemetry metrics Claude Code exports, as OTLP over HTTP with
/// JSON, and keeps cost and tokens per session. Set
/// `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` to this address and send the node
/// token as `Authorization` header.
#[utoipa::path(
    post,
    operation_id = "export_metrics",
    path = "/node/otel/v1/metrics",
    request_body(content = Value, description = "OTLP metrics export in JSON"),
    responses((status = 200, description = "Stored")),
    security(("bearer" = []))
)]
pub(crate) async fn export_metrics(
    Scoped(state): Scoped,
    caller: Caller,
    Json(export): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let node = node_of(caller)?;
    let reports = lighthouse_sessions::parse_otlp(&export);
    state
        .sessions
        .record_usage(node, &reports)
        .await
        .map_err(internal)?;
    for report in &reports {
        state.events.emit(
            "sessions.changed",
            json!({"node_id": node, "session_id": report.session_id}),
        );
    }
    Ok(Json(json!({})))
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
    Scoped(state): Scoped,
    caller: Caller,
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
    Scoped(state): Scoped,
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
        usage: sessions.usage(node, &id).await.map_err(internal)?,
    }))
}

/// Sends a message to a session. The desktop agent hands it over as the next
/// instruction when the session stops, see `docs/desktop-agent.md`. A session
/// Lighthouse launched that has ended is resumed with the message instead.
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
    Scoped(state): Scoped,
    Path((node, id)): Path<(Uuid, String)>,
    Json(message): Json<NewSessionMessage>,
) -> Result<(StatusCode, Json<SessionMessage>), ApiError> {
    if message.text.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the message is empty".to_owned(),
        ));
    }
    let sessions = &state.sessions;
    let launched = sessions
        .launch_of_session(node, &id)
        .await
        .map_err(internal)?;
    let known = sessions.session(node, &id).await.map_err(internal)?;
    let sent = sessions
        .send_message(node, &id, &message.text)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown session".to_owned()))?;
    let ended = known.is_some_and(|session| session.status == "done");
    if let (Some(earlier), true) = (launched, ended) {
        // Nobody holds the session open any more: resume it with the message
        // instead of waiting for a stop that never comes.
        sessions.take_messages(node, &id).await.map_err(internal)?;
        let resume = NewLaunch {
            node,
            cwd: earlier.cwd,
            prompt: message.text.clone(),
            resume: Some(id.clone()),
            model: earlier.model,
        };
        if let Some(launch) = sessions.launch(&resume).await.map_err(internal)? {
            launch_changed(&state, &launch);
        }
    }
    state.events.emit(
        "sessions.changed",
        json!({"node_id": node, "session_id": id}),
    );
    Ok((StatusCode::CREATED, Json(sent)))
}

/// The longest context handed to a starting session, in characters.
const MAX_CONTEXT: usize = 8_000;
const CONTEXT_ENTRIES: usize = 5;
const CONTEXT_TASKS: i64 = 200;
const CONTEXT_PROMPTS: i64 = 5;

/// Parameters of a session context.
#[derive(Deserialize)]
pub(crate) struct ContextQuery {
    cwd: Option<String>,
    source: Option<String>,
}

/// What a session gets when it starts, resumes or was compacted.
#[derive(Serialize, serde::Deserialize, ToSchema)]
pub struct SessionContext {
    /// Markdown for the session's context. Empty when there is nothing to say.
    pub text: String,
}

fn project_of(cwd: Option<&str>) -> Option<String> {
    let name = std::path::Path::new(cwd?)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    (!name.is_empty()).then_some(name)
}

fn cut_context(text: String) -> String {
    if text.chars().count() <= MAX_CONTEXT {
        return text;
    }
    let mut short: String = text.chars().take(MAX_CONTEXT).collect();
    short.push_str("\n[cut]");
    short
}

fn todo_lines(todos: &Value) -> Vec<String> {
    todos
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let content = item["content"].as_str()?;
                    let status = item["status"].as_str().unwrap_or("pending");
                    Some(format!("- [{status}] {content}"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Composes what Lighthouse knows for a session of this desktop agent: the
/// session's own state, Brain entries and open tasks of its project. A new
/// session (`source=startup`) gets only the project part.
#[utoipa::path(
    get,
    operation_id = "get_session_context",
    path = "/node/sessions/{id}/context",
    params(
        ("id" = String, Path, description = "Session ID"),
        ("cwd" = Option<String>, Query, description = "Working directory of the session"),
        ("source" = Option<String>, Query, description = "startup, resume, compact or clear")
    ),
    responses((status = 200, body = SessionContext)),
    security(("bearer" = []))
)]
pub(crate) async fn context(
    Scoped(state): Scoped,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<ContextQuery>,
) -> Result<Json<SessionContext>, ApiError> {
    let node = node_of(caller)?;
    let sessions = &state.sessions;
    let known = sessions.session(node, &id).await.map_err(internal)?;
    let project = project_of(query.cwd.as_deref())
        .or_else(|| known.as_ref().map(|session| session.project.clone()))
        .filter(|project| project != "unknown");
    let startup = query.source.as_deref().unwrap_or("startup") == "startup";
    let mut parts: Vec<String> = Vec::new();
    if let (Some(session), false) = (&known, startup) {
        let mut lines = vec![format!(
            "## This session in Lighthouse\n\nStatus: {}{}",
            session.status,
            session
                .summary
                .as_deref()
                .map(|summary| format!(". Latest: {summary}"))
                .unwrap_or_default()
        )];
        if let Some(plan) = sessions
            .latest_event(node, &id, "plan")
            .await
            .map_err(internal)?
            .and_then(|event| event.detail["plan"].as_str().map(str::to_owned))
        {
            lines.push(format!("### Plan\n\n{plan}"));
        }
        if let Some(todos) = sessions
            .latest_event(node, &id, "todos")
            .await
            .map_err(internal)?
        {
            let todos = todo_lines(&todos.detail["todos"]);
            if !todos.is_empty() {
                lines.push(format!("### Todos\n\n{}", todos.join("\n")));
            }
        }
        let prompts = sessions
            .events(node, &id, Some("prompt"), CONTEXT_PROMPTS)
            .await
            .map_err(internal)?;
        let prompts: Vec<String> = prompts
            .iter()
            .filter_map(|event| event.summary.as_deref().map(|text| format!("- {text}")))
            .collect();
        if !prompts.is_empty() {
            lines.push(format!(
                "### Latest prompts of the owner\n\n{}",
                prompts.join("\n")
            ));
        }
        let messages = sessions.messages(node, &id).await.map_err(internal)?;
        if !messages.is_empty() {
            let messages: Vec<String> = messages
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(|message| format!("- {}", message.text))
                .collect();
            lines.push(format!(
                "### Messages from the owner through Lighthouse\n\n{}",
                messages.join("\n")
            ));
        }
        let files = sessions.changed_files(node, &id).await.map_err(internal)?;
        if !files.is_empty() {
            let files: Vec<String> = files
                .iter()
                .take(15)
                .map(|file| format!("- {} ({} edits)", file.path, file.edits))
                .collect();
            lines.push(format!("### Files changed\n\n{}", files.join("\n")));
        }
        parts.push(lines.join("\n\n"));
    }
    if let Some(project) = &project {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        for link in [format!("project:{project}"), format!("session:{id}")] {
            for entry in state
                .brain
                .linked(&link, CONTEXT_ENTRIES as i64)
                .await
                .map_err(internal)?
            {
                if seen.insert(entry.id) {
                    entries.push(entry);
                }
            }
        }
        for hit in state
            .brain
            .search(project, CONTEXT_ENTRIES)
            .await
            .map_err(internal)?
        {
            if seen.insert(hit.entry.id) {
                entries.push(hit.entry);
            }
        }
        if !entries.is_empty() {
            let lines: Vec<String> = entries
                .iter()
                .take(CONTEXT_ENTRIES * 2)
                .map(|entry| {
                    format!(
                        "- **{}** ({}, `{}`): {}",
                        entry.title,
                        entry.kind,
                        entry.id,
                        entry.snippet.replace('\n', " ")
                    )
                })
                .collect();
            parts.push(format!(
                "## Second Brain on {project}\n\n{}",
                lines.join("\n")
            ));
        }
        let today = chrono::Utc::now().with_timezone(&state.zone).date_naive();
        let needle = project.to_lowercase();
        let tasks: Vec<String> = state
            .desk
            .tasks(TaskView::All, today, CONTEXT_TASKS)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|task| {
                task.title.to_lowercase().contains(&needle)
                    || task
                        .notes
                        .as_deref()
                        .is_some_and(|notes| notes.to_lowercase().contains(&needle))
            })
            .take(10)
            .map(|task| {
                let due = task
                    .due_on
                    .map(|due| format!(", due {due}"))
                    .unwrap_or_default();
                format!("- {}{due}", task.title)
            })
            .collect();
        if !tasks.is_empty() {
            parts.push(format!(
                "## Open tasks on {project}\n\n{}",
                tasks.join("\n")
            ));
        }
    }
    if parts.is_empty() {
        return Ok(Json(SessionContext {
            text: String::new(),
        }));
    }
    parts.insert(
        0,
        "# Lighthouse\n\nKept by Lighthouse across sessions and compaction. Use the Lighthouse MCP tools \
         `search_brain` and `save_to_brain` to look up and keep decisions and the state of this work."
            .to_owned(),
    );
    Ok(Json(SessionContext {
        text: cut_context(parts.join("\n\n")),
    }))
}

/// Parameters of the launch list.
#[derive(Deserialize)]
pub(crate) struct LaunchQuery {
    node: Option<Uuid>,
    limit: Option<i64>,
}

fn launch_changed(state: &crate::ApiState, launch: &Launch) {
    state.events.emit(
        "sessions.changed",
        json!({"node_id": launch.node_id, "launch_id": launch.id, "session_id": launch.session_id}),
    );
}

/// Asks a desktop agent to start a Claude Code session, or to resume one.
#[utoipa::path(
    post,
    operation_id = "launch_session",
    path = "/sessions/launch",
    request_body = NewLaunch,
    responses(
        (status = 202, body = Launch),
        (status = 400, description = "Empty prompt or directory"),
        (status = 404, description = "Unknown node")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn launch(
    Scoped(state): Scoped,
    Json(new): Json<NewLaunch>,
) -> Result<(StatusCode, Json<Launch>), ApiError> {
    if new.prompt.trim().is_empty() || new.cwd.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "prompt and cwd are required".to_owned(),
        ));
    }
    let launch = state
        .sessions
        .launch(&new)
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown node".to_owned()))?;
    launch_changed(&state, &launch);
    Ok((StatusCode::ACCEPTED, Json(launch)))
}

/// Lists launches, newest first.
#[utoipa::path(
    get,
    operation_id = "list_launches",
    path = "/sessions/launches",
    params(
        ("node" = Option<Uuid>, Query, description = "Only launches of this node"),
        ("limit" = Option<i64>, Query, description = "Maximum number of launches")
    ),
    responses((status = 200, body = Vec<Launch>)),
    security(("bearer" = []))
)]
pub(crate) async fn launches(
    Scoped(state): Scoped,
    Query(query): Query<LaunchQuery>,
) -> Result<Json<Vec<Launch>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    Ok(Json(
        state
            .sessions
            .launches(query.node, limit)
            .await
            .map_err(internal)?,
    ))
}

/// Returns one launch.
#[utoipa::path(
    get,
    operation_id = "get_launch",
    path = "/sessions/launches/{id}",
    params(("id" = Uuid, Path, description = "Launch ID")),
    responses((status = 200, body = Launch), (status = 404, description = "Unknown launch")),
    security(("bearer" = []))
)]
pub(crate) async fn get_launch(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Launch>, ApiError> {
    state
        .sessions
        .launch_by_id(id)
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown launch".to_owned()))
}

/// Cancels a launch. A queued one ends at once; a running one ends when its
/// desktop agent stops the process.
#[utoipa::path(
    delete,
    operation_id = "cancel_launch",
    path = "/sessions/launches/{id}",
    params(("id" = Uuid, Path, description = "Launch ID")),
    responses((status = 200, body = Launch), (status = 404, description = "Unknown or already over")),
    security(("bearer" = []))
)]
pub(crate) async fn cancel_launch(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Launch>, ApiError> {
    let launch = state
        .sessions
        .cancel_launch(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "unknown launch or already over".to_owned(),
            )
        })?;
    launch_changed(&state, &launch);
    Ok(Json(launch))
}

/// Hands queued launches to this desktop agent and names running ones to stop.
/// Waits up to `wait` seconds for one.
#[utoipa::path(
    get,
    operation_id = "take_launches",
    path = "/node/launches",
    params(("wait" = Option<u64>, Query, description = "Seconds to wait, at most 60")),
    responses((status = 200, body = LaunchQueue)),
    security(("bearer" = []))
)]
pub(crate) async fn take_launches(
    Scoped(state): Scoped,
    caller: Caller,
    Query(query): Query<WaitQuery>,
) -> Result<Json<LaunchQueue>, ApiError> {
    let node = node_of(caller)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_WAIT_SECONDS));
    let deadline = Instant::now() + wait;
    loop {
        let queue = state.sessions.take_launches(node).await.map_err(internal)?;
        if !queue.launches.is_empty() || !queue.cancel.is_empty() || Instant::now() >= deadline {
            for launch in &queue.launches {
                launch_changed(&state, launch);
            }
            return Ok(Json(queue));
        }
        sleep(POLL).await;
    }
}

/// Reports what became of a launch of this desktop agent.
#[utoipa::path(
    post,
    operation_id = "report_launch",
    path = "/node/launches/{id}",
    params(("id" = Uuid, Path, description = "Launch ID")),
    request_body = LaunchReport,
    responses((status = 200, body = Launch), (status = 404, description = "Unknown, foreign or already over")),
    security(("bearer" = []))
)]
pub(crate) async fn report_launch(
    Scoped(state): Scoped,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(report): Json<LaunchReport>,
) -> Result<Json<Launch>, ApiError> {
    let node = node_of(caller)?;
    let launch = state
        .sessions
        .report_launch(node, id, &report)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "unknown launch or already over".to_owned(),
            )
        })?;
    launch_changed(&state, &launch);
    Ok(Json(launch))
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
        for path in ["/node/events", "/node/report"] {
            let (status, _) = send(
                app,
                "POST",
                path,
                Some(&owner),
                Some(json!({"events": [], "name": "x", "sessions": []})),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{path} refuses a token that is not a node's"
            );
            let (status, _) = send(
                app,
                "POST",
                path,
                Some(TOKEN),
                Some(json!({"events": [], "name": "x", "sessions": []})),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{path} refuses the admin token"
            );
        }
        let (status, _) = call(app, "GET", "/node/sessions/s1/messages", Some(&owner)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
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
