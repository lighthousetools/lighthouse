//! The owner's to-do list.

use crate::{ApiError, ApiState, desk_error, not_found};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::Utc;
use lighthouse_desk::{NewTask, Task, TaskPatch, TaskView};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 200;

/// Filter of the task list.
#[derive(Deserialize)]
pub(crate) struct TaskQuery {
    view: Option<TaskView>,
    limit: Option<i64>,
}

/// A new order of tasks.
#[derive(Deserialize, ToSchema)]
pub struct TaskOrder {
    /// Task IDs, first to last. Tasks not named keep their order after these.
    pub ids: Vec<Uuid>,
}

/// Result of a reorder.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct TaskOrderResult {
    /// Number of tasks that were found and moved.
    pub moved: u64,
}

fn changed(state: &ApiState, id: Option<Uuid>) {
    state.events.emit("tasks.changed", json!({"id": id}));
}

/// Lists tasks.
#[utoipa::path(
    get,
    operation_id = "list_tasks",
    path = "/tasks",
    params(
        ("view" = Option<TaskView>, Query, description = "all, today, upcoming, done or proposed"),
        ("limit" = Option<i64>, Query, description = "Maximum number of tasks")
    ),
    responses((status = 200, body = Vec<Task>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<TaskQuery>,
) -> Result<Json<Vec<Task>>, ApiError> {
    let today = Utc::now().with_timezone(&state.zone).date_naive();
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000);
    let tasks = state
        .desk
        .tasks(query.view.unwrap_or_default(), today, limit)
        .await
        .map_err(desk_error)?;
    Ok(Json(tasks))
}

/// Creates a task at the top of the list.
#[utoipa::path(
    post,
    operation_id = "create_task",
    path = "/tasks",
    request_body = NewTask,
    responses((status = 201, body = Task), (status = 400, description = "Invalid task")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    State(state): State<ApiState>,
    Json(task): Json<NewTask>,
) -> Result<(StatusCode, Json<Task>), ApiError> {
    let task = state
        .desk
        .create_task(task, None)
        .await
        .map_err(desk_error)?;
    changed(&state, Some(task.id));
    Ok((StatusCode::CREATED, Json(task)))
}

/// Returns a task.
#[utoipa::path(
    get,
    operation_id = "get_task",
    path = "/tasks/{id}",
    params(("id" = Uuid, Path, description = "Task ID")),
    responses((status = 200, body = Task), (status = 404, description = "Unknown task")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Task>, ApiError> {
    let task = state.desk.task(id).await.map_err(desk_error)?;
    task.map(Json).ok_or_else(|| not_found("task"))
}

/// Changes a task. Missing fields stay, `null` clears.
#[utoipa::path(
    patch,
    operation_id = "update_task",
    path = "/tasks/{id}",
    params(("id" = Uuid, Path, description = "Task ID")),
    request_body = TaskPatch,
    responses(
        (status = 200, body = Task),
        (status = 400, description = "Invalid change"),
        (status = 404, description = "Unknown task")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(patch): Json<TaskPatch>,
) -> Result<Json<Task>, ApiError> {
    let task = state
        .desk
        .update_task(id, patch)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("task"))?;
    changed(&state, Some(id));
    Ok(Json(task))
}

/// Deletes a task, which also rejects a proposal.
#[utoipa::path(
    delete,
    operation_id = "delete_task",
    path = "/tasks/{id}",
    params(("id" = Uuid, Path, description = "Task ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown task")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_task(id).await.map_err(desk_error)? {
        return Err(not_found("task"));
    }
    changed(&state, Some(id));
    Ok(StatusCode::NO_CONTENT)
}

fn conflict(what: &str) -> ApiError {
    ApiError(StatusCode::CONFLICT, format!("the task is {what}"))
}

/// Marks a task as done.
#[utoipa::path(
    post,
    operation_id = "complete_task",
    path = "/tasks/{id}/complete",
    params(("id" = Uuid, Path, description = "Task ID")),
    responses((status = 200, body = Task), (status = 409, description = "Already done or unknown")),
    security(("bearer" = []))
)]
pub(crate) async fn complete(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Task>, ApiError> {
    let task = state.desk.complete_task(id).await.map_err(desk_error)?;
    let task = task.ok_or_else(|| conflict("already done or unknown"))?;
    changed(&state, Some(id));
    Ok(Json(task))
}

/// Puts a done task back on the list.
#[utoipa::path(
    post,
    operation_id = "reopen_task",
    path = "/tasks/{id}/reopen",
    params(("id" = Uuid, Path, description = "Task ID")),
    responses((status = 200, body = Task), (status = 409, description = "Not done or unknown")),
    security(("bearer" = []))
)]
pub(crate) async fn reopen(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Task>, ApiError> {
    let task = state.desk.reopen_task(id).await.map_err(desk_error)?;
    let task = task.ok_or_else(|| conflict("not done or unknown"))?;
    changed(&state, Some(id));
    Ok(Json(task))
}

/// Accepts a task an agent proposed.
#[utoipa::path(
    post,
    operation_id = "accept_task",
    path = "/tasks/{id}/accept",
    params(("id" = Uuid, Path, description = "Task ID")),
    responses((status = 200, body = Task), (status = 409, description = "Not a proposal or unknown")),
    security(("bearer" = []))
)]
pub(crate) async fn accept(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Task>, ApiError> {
    let task = state.desk.accept_task(id).await.map_err(desk_error)?;
    let task = task.ok_or_else(|| conflict("not a proposal or unknown"))?;
    changed(&state, Some(id));
    Ok(Json(task))
}

/// Who a task goes to.
#[derive(Deserialize, ToSchema)]
pub struct Delegation {
    /// The agent that works on it.
    pub agent_id: String,
}

/// Hands a task to an agent. Its run gets the task as context, and its report
/// lands on the task as `result`. The task stays open until the owner closes it.
#[utoipa::path(
    post,
    operation_id = "delegate_task",
    path = "/tasks/{id}/delegate",
    params(("id" = Uuid, Path, description = "Task ID")),
    request_body = Delegation,
    responses(
        (status = 202, body = Task),
        (status = 404, description = "Unknown task or agent"),
        (status = 409, description = "Done already")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn delegate(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(delegation): Json<Delegation>,
) -> Result<(StatusCode, Json<Task>), ApiError> {
    let task = state
        .desk
        .task(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("task"))?;
    if task.status == lighthouse_desk::TaskStatus::Done {
        return Err(conflict("done already"));
    }
    let mut context = format!("The owner handed you this task: {}", task.title);
    if let Some(notes) = &task.notes {
        context.push_str(&format!("\nNotes: {notes}"));
    }
    if let Some(due) = task.due_on {
        context.push_str(&format!("\nDue: {due}"));
    }
    context.push_str("\nPrepare what you can and report what is left for the owner.");
    let run = state
        .trigger
        .trigger_with_context(&delegation.agent_id, &context)
        .await
        .map_err(|error| match error {
            crate::TriggerError::UnknownAgent(_) => not_found("agent"),
            other => crate::internal(other),
        })?;
    let task = state
        .desk
        .delegate_task(id, &delegation.agent_id, run)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("task"))?;
    changed(&state, Some(id));
    Ok((StatusCode::ACCEPTED, Json(task)))
}

/// Changes the order of tasks.
#[utoipa::path(
    post,
    operation_id = "reorder_tasks",
    path = "/tasks/order",
    request_body = TaskOrder,
    responses((status = 200, body = TaskOrderResult)),
    security(("bearer" = []))
)]
pub(crate) async fn reorder(
    State(state): State<ApiState>,
    Json(order): Json<TaskOrder>,
) -> Result<Json<TaskOrderResult>, ApiError> {
    let moved = state
        .desk
        .reorder_tasks(&order.ids)
        .await
        .map_err(desk_error)?;
    changed(&state, None);
    Ok(Json(TaskOrderResult { moved }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_desk::{Proposer, TaskStatus};

    fn task(body: &str) -> Task {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn the_owner_manages_tasks_and_apps_hear_about_it() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let mut events = harness.events.subscribe();
        let new = json!({"title": "Call the bank", "priority": 3, "due_on": "2020-01-01",
                         "source": {"kind": "issue", "reference": "ENG-7"}});
        let (status, body) = send(app, "POST", "/tasks", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created = task(&body);
        assert_eq!(events.recv().await.unwrap().name, "tasks.changed");
        let second = json!({"title": "Plan the trip"});
        let (_, body) = send(app, "POST", "/tasks", Some(TOKEN), Some(second)).await;
        let later = task(&body);

        let (_, body) = call(app, "GET", "/tasks?view=today", Some(TOKEN)).await;
        let today: Vec<Task> = serde_json::from_str(&body).unwrap();
        assert_eq!(today.len(), 1, "an overdue task counts as today");
        let (_, body) = call(app, "GET", "/tasks?view=upcoming", Some(TOKEN)).await;
        assert!(body.contains("Plan the trip") && !body.contains("Call the bank"));

        let uri = format!("/tasks/{}", created.id);
        let patch = json!({"notes": "Ask about fees", "due_on": null});
        let (status, body) = send(app, "PATCH", &uri, Some(TOKEN), Some(patch)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task(&body).due_on, None);
        let (status, _) = send(app, "PATCH", &uri, Some(TOKEN), Some(json!({"title": ""}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let order = json!({"ids": [created.id, later.id]});
        let (_, body) = send(app, "POST", "/tasks/order", Some(TOKEN), Some(order)).await;
        assert!(body.contains("\"moved\":2"));
        let (_, body) = call(app, "GET", "/tasks", Some(TOKEN)).await;
        let all: Vec<Task> = serde_json::from_str(&body).unwrap();
        assert_eq!(all[0].id, created.id);

        let complete = format!("{uri}/complete");
        assert_eq!(
            call(app, "POST", &complete, Some(TOKEN)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "POST", &complete, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );
        let (_, body) = call(app, "GET", "/tasks?view=done", Some(TOKEN)).await;
        assert!(body.contains("Call the bank"));
        let (status, body) = call(app, "POST", &format!("{uri}/reopen"), Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task(&body).status, TaskStatus::Open);
        assert_eq!(
            call(app, "POST", &format!("{uri}/reopen"), Some(TOKEN))
                .await
                .0,
            StatusCode::CONFLICT
        );

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
        let missing = json!({"title": "x"});
        assert_eq!(
            send(app, "PATCH", &uri, Some(TOKEN), Some(missing)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "GET", "/tasks", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn proposals_of_agents_wait_for_acceptance() {
        let harness = harness(Some(TOKEN)).await;
        let proposer = Proposer::agent("inbox".to_owned(), Uuid::now_v7());
        let proposal = harness
            .desk
            .create_task(
                NewTask {
                    title: "Answer the request".to_owned(),
                    ..NewTask::default()
                },
                Some(proposer),
            )
            .await
            .unwrap();
        let app = &harness.app;
        let (_, body) = call(app, "GET", "/tasks?view=proposed", Some(TOKEN)).await;
        assert!(body.contains("Answer the request"));
        let accept = format!("/tasks/{}/accept", proposal.id);
        let (status, body) = call(app, "POST", &accept, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task(&body).status, TaskStatus::Open);
        assert_eq!(
            call(app, "POST", &accept, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );
    }
}
