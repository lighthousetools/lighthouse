//! Talk: conversations with the assistant.

use crate::auth::Caller;
use crate::{ApiError, ApiState, Instance, Scoped, desk_error, may_ask, not_found, trigger_error};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use lighthouse_desk::{TalkMessage, TalkThread, transcript};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

/// Messages of the thread the next answer sees.
const HISTORY: usize = 12;
const THREADS: i64 = 200;
const TITLE_LENGTH: usize = 60;

/// A new thread.
#[derive(Deserialize, ToSchema)]
pub struct NewThread {
    /// Title. Defaults to "New conversation".
    #[serde(default)]
    pub title: Option<String>,
}

/// Changes to a thread.
#[derive(Deserialize, ToSchema)]
pub struct ThreadPatch {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// Pin or unpin.
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// A thread with its messages.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ThreadDetail {
    /// The thread.
    pub thread: TalkThread,
    /// Its messages, oldest first.
    pub messages: Vec<TalkMessage>,
}

/// A message from the owner.
#[derive(Deserialize, ToSchema)]
pub struct NewMessage {
    /// The text.
    pub text: String,
}

/// The owner's message and the answer, which may still be pending.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Exchange {
    /// The owner's message.
    pub question: TalkMessage,
    /// The answer. With a talk agent it is `pending` until its run ends.
    pub answer: TalkMessage,
}

fn changed(state: &ApiState, thread: Uuid) {
    state
        .events
        .emit("talk.changed", json!({"thread_id": thread}));
}

/// Lists conversations, pinned ones first.
#[utoipa::path(
    get,
    operation_id = "list_threads",
    path = "/talk/threads",
    responses((status = 200, body = Vec<TalkThread>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(Scoped(state): Scoped) -> Result<Json<Vec<TalkThread>>, ApiError> {
    Ok(Json(state.desk.threads(THREADS).await.map_err(desk_error)?))
}

/// Starts a conversation.
#[utoipa::path(
    post,
    operation_id = "create_thread",
    path = "/talk/threads",
    request_body = NewThread,
    responses((status = 201, body = TalkThread)),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    Scoped(state): Scoped,
    Json(new): Json<NewThread>,
) -> Result<(StatusCode, Json<TalkThread>), ApiError> {
    let title = new.title.unwrap_or_else(|| "New conversation".to_owned());
    let thread = state.desk.create_thread(&title).await.map_err(desk_error)?;
    changed(&state, thread.id);
    Ok((StatusCode::CREATED, Json(thread)))
}

/// Returns a conversation with its messages.
#[utoipa::path(
    get,
    operation_id = "get_thread",
    path = "/talk/threads/{id}",
    params(("id" = Uuid, Path, description = "Thread ID")),
    responses((status = 200, body = ThreadDetail), (status = 404, description = "Unknown thread")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<ThreadDetail>, ApiError> {
    let thread = state
        .desk
        .thread(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("thread"))?;
    let messages = state.desk.messages(id).await.map_err(desk_error)?;
    Ok(Json(ThreadDetail { thread, messages }))
}

/// Renames or pins a conversation.
#[utoipa::path(
    patch,
    operation_id = "update_thread",
    path = "/talk/threads/{id}",
    params(("id" = Uuid, Path, description = "Thread ID")),
    request_body = ThreadPatch,
    responses((status = 200, body = TalkThread), (status = 404, description = "Unknown thread")),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(patch): Json<ThreadPatch>,
) -> Result<Json<TalkThread>, ApiError> {
    let thread = state
        .desk
        .update_thread(id, patch.title.as_deref(), patch.pinned)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("thread"))?;
    changed(&state, id);
    Ok(Json(thread))
}

/// Deletes a conversation.
#[utoipa::path(
    delete,
    operation_id = "delete_thread",
    path = "/talk/threads/{id}",
    params(("id" = Uuid, Path, description = "Thread ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown thread")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_thread(id).await.map_err(desk_error)? {
        return Err(not_found("thread"));
    }
    changed(&state, id);
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a message and gets the answer.
///
/// With a talk agent chosen, a run of that agent answers with the thread as
/// context: the answer is `pending` until the run ends, and the run shows the
/// tool calls and approvals. Without one, the built-in assistant answers the
/// same way with the person's connected services, or, when none is connected,
/// the Second Brain answers at once.
#[utoipa::path(
    post,
    operation_id = "send_message",
    path = "/talk/threads/{id}/messages",
    params(("id" = Uuid, Path, description = "Thread ID")),
    request_body = NewMessage,
    responses(
        (status = 200, body = Exchange),
        (status = 202, body = Exchange),
        (status = 400, description = "Empty message"),
        (status = 404, description = "Unknown thread"),
        (status = 409, description = "The entitlements refuse the run or the question")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn send(
    State(instance): State<Instance>,
    caller: Caller,
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(message): Json<NewMessage>,
) -> Result<(StatusCode, Json<Exchange>), ApiError> {
    let thread = state
        .desk
        .thread(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("thread"))?;
    let history = state.desk.messages(id).await.map_err(desk_error)?;
    let earlier = transcript(&history, HISTORY);
    // A refused run or question leaves the conversation as it was.
    let run = match &talk_agent(&state, caller.user) {
        Some(agent) => {
            let context = format!(
                "This run answers the owner in a conversation. Answer the latest message.\n\n\
                 Conversation so far:\n{earlier}\n\nLatest message from the owner:\n{}",
                message.text
            );
            Some(
                state
                    .trigger
                    .trigger_with_context(agent, &context)
                    .await
                    .map_err(trigger_error)?,
            )
        }
        None => {
            let run = state
                .trigger
                .trigger_assistant(&message.text, &earlier)
                .await
                .map_err(trigger_error)?;
            if run.is_none() {
                may_ask(&instance, &caller).await?;
            }
            run
        }
    };
    let question = state
        .desk
        .add_message(id, "owner", &message.text, "done", None, json!([]))
        .await
        .map_err(desk_error)?;
    if history.is_empty() && thread.title == "New conversation" {
        let title: String = message.text.trim().chars().take(TITLE_LENGTH).collect();
        state
            .desk
            .update_thread(id, Some(&title), None)
            .await
            .map_err(desk_error)?;
    }
    let (status, answer) = match run {
        Some(run) => {
            let answer = state
                .desk
                .add_message(id, "assistant", "", "pending", Some(run), json!([]))
                .await
                .map_err(desk_error)?;
            (StatusCode::ACCEPTED, answer)
        }
        None => {
            let question_text = if earlier.is_empty() {
                message.text.clone()
            } else {
                format!(
                    "{}\n\n(Earlier in this conversation:\n{earlier})",
                    message.text
                )
            };
            let answer = match state.assistant.ask(&question_text).await {
                Ok(answer) => {
                    state
                        .desk
                        .add_message(
                            id,
                            "assistant",
                            &answer.answer,
                            "done",
                            Some(answer.run_id),
                            serde_json::to_value(&answer.sources).unwrap_or_default(),
                        )
                        .await
                }
                Err(reason) => {
                    state
                        .desk
                        .add_message(id, "assistant", &reason, "failed", None, json!([]))
                        .await
                }
            }
            .map_err(desk_error)?;
            (StatusCode::OK, answer)
        }
    };
    changed(&state, id);
    Ok((status, Json(Exchange { question, answer })))
}

/// The agent that answers a person's conversations: their own choice, the
/// tenant's, then the instance's. Without one, the built-in assistant answers.
fn talk_agent(state: &ApiState, user: Option<uuid::Uuid>) -> Option<String> {
    let effective = state.settings.effective(user);
    match effective.values.get(lighthouse_settings::TALK_AGENT) {
        Some((value, _)) => value.as_str().map(str::to_owned),
        None => state.talk_agent.clone(),
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{TOKEN, call, harness, send};
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn the_brain_answers_with_the_conversation_as_context() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let (status, body) = send(app, "POST", "/talk/threads", Some(TOKEN), Some(json!({}))).await;
        assert_eq!(status, StatusCode::CREATED);
        let thread: Value = serde_json::from_str(&body).unwrap();
        let uri = format!("/talk/threads/{}", thread["id"].as_str().unwrap());
        let messages = format!("{uri}/messages");
        let (status, body) = send(
            app,
            "POST",
            &messages,
            Some(TOKEN),
            Some(json!({"text": "What is due?"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let exchange: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(exchange["answer"]["text"], "You asked What is due?");
        let (_, body) = send(
            app,
            "POST",
            &messages,
            Some(TOKEN),
            Some(json!({"text": "And then?"})),
        )
        .await;
        let exchange: Value = serde_json::from_str(&body).unwrap();
        assert!(
            exchange["answer"]["text"]
                .as_str()
                .unwrap()
                .contains("Earlier in this conversation")
        );
        let (_, body) = send(
            app,
            "POST",
            "/talk/threads",
            Some(TOKEN),
            Some(json!({"title": "Other"})),
        )
        .await;
        let other: Value = serde_json::from_str(&body).unwrap();
        let failing = format!("/talk/threads/{}/messages", other["id"].as_str().unwrap());
        let (_, body) = send(
            app,
            "POST",
            &failing,
            Some(TOKEN),
            Some(json!({"text": "fail"})),
        )
        .await;
        let exchange: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(exchange["answer"]["status"], "failed");
        assert_eq!(
            send(
                app,
                "POST",
                &messages,
                Some(TOKEN),
                Some(json!({"text": " "}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );

        let (_, body) = call(app, "GET", &uri, Some(TOKEN)).await;
        let detail: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(detail["thread"]["title"], "What is due?");
        assert_eq!(detail["messages"].as_array().unwrap().len(), 4);
        let (_, body) = send(
            app,
            "PATCH",
            &uri,
            Some(TOKEN),
            Some(json!({"pinned": true})),
        )
        .await;
        assert!(body.contains(r#""pinned":true"#));
        assert!(
            call(app, "GET", "/talk/threads", Some(TOKEN))
                .await
                .1
                .contains("What is due?")
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "GET", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(app, "PATCH", &uri, Some(TOKEN), Some(json!({})))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(
                app,
                "POST",
                &messages,
                Some(TOKEN),
                Some(json!({"text": "x"}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
}

#[cfg(test)]
mod agent_tests {
    use crate::tests::{TOKEN, harness_with_talk_agent, send};
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn a_talk_agent_answers_through_a_run() {
        let harness = harness_with_talk_agent(Some(TOKEN), "known").await;
        let app = &harness.app;
        let (_, body) = send(app, "POST", "/talk/threads", Some(TOKEN), Some(json!({}))).await;
        let thread: Value = serde_json::from_str(&body).unwrap();
        let messages = format!("/talk/threads/{}/messages", thread["id"].as_str().unwrap());
        let (status, body) = send(
            app,
            "POST",
            &messages,
            Some(TOKEN),
            Some(json!({"text": "Plan my day"})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let exchange: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(exchange["answer"]["status"], "pending");
        assert!(exchange["answer"]["run_id"].is_string());
    }
}
