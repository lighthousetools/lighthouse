//! What the assistant knows about the owner.

use crate::{ApiError, ApiState, Scoped, desk_error, not_found};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use lighthouse_desk::{Fact, FactFilter, FactPatch, NewFact};
use serde_json::json;
use uuid::Uuid;

fn changed(state: &ApiState, id: Uuid) {
    state.events.emit("memory.changed", json!({"id": id}));
}

/// Lists facts about the owner, newest first.
#[utoipa::path(
    get,
    operation_id = "list_facts",
    path = "/memory",
    params(
        ("status" = Option<String>, Query, description = "proposed or confirmed"),
        ("category" = Option<String>, Query, description = "Only this grouping")
    ),
    responses((status = 200, body = Vec<Fact>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    Scoped(state): Scoped,
    Query(filter): Query<FactFilter>,
) -> Result<Json<Vec<Fact>>, ApiError> {
    Ok(Json(state.desk.facts(&filter).await.map_err(desk_error)?))
}

/// Stores a fact the owner states. It is confirmed at once.
#[utoipa::path(
    post,
    operation_id = "create_fact",
    path = "/memory",
    request_body = NewFact,
    responses((status = 201, body = Fact), (status = 400, description = "Invalid fact")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    Scoped(state): Scoped,
    Json(fact): Json<NewFact>,
) -> Result<(StatusCode, Json<Fact>), ApiError> {
    let fact = state.desk.create_fact(fact).await.map_err(desk_error)?;
    changed(&state, fact.id);
    Ok((StatusCode::CREATED, Json(fact)))
}

/// Returns a fact.
#[utoipa::path(
    get,
    operation_id = "get_fact",
    path = "/memory/{id}",
    params(("id" = Uuid, Path, description = "Fact ID")),
    responses((status = 200, body = Fact), (status = 404, description = "Unknown fact")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Fact>, ApiError> {
    let fact = state.desk.fact(id).await.map_err(desk_error)?;
    fact.map(Json).ok_or_else(|| not_found("fact"))
}

/// Rewords, regroups, pauses or resumes a fact. Rewording confirms it.
#[utoipa::path(
    patch,
    operation_id = "update_fact",
    path = "/memory/{id}",
    params(("id" = Uuid, Path, description = "Fact ID")),
    request_body = FactPatch,
    responses(
        (status = 200, body = Fact),
        (status = 400, description = "Invalid change"),
        (status = 404, description = "Unknown fact")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(patch): Json<FactPatch>,
) -> Result<Json<Fact>, ApiError> {
    let fact = state
        .desk
        .update_fact(id, patch)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("fact"))?;
    changed(&state, id);
    Ok(Json(fact))
}

/// Confirms a fact an agent learned.
#[utoipa::path(
    post,
    operation_id = "confirm_fact",
    path = "/memory/{id}/confirm",
    params(("id" = Uuid, Path, description = "Fact ID")),
    responses(
        (status = 200, body = Fact),
        (status = 409, description = "Confirmed already or unknown")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn confirm(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Fact>, ApiError> {
    let fact = state
        .desk
        .confirm_fact(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "the fact is confirmed already or unknown".to_owned(),
            )
        })?;
    changed(&state, id);
    Ok(Json(fact))
}

/// Forgets a fact.
#[utoipa::path(
    delete,
    operation_id = "delete_fact",
    path = "/memory/{id}",
    params(("id" = Uuid, Path, description = "Fact ID")),
    responses((status = 204, description = "Forgotten"), (status = 404, description = "Unknown fact")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_fact(id).await.map_err(desk_error)? {
        return Err(not_found("fact"));
    }
    changed(&state, id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_desk::{ProposedFact, Proposer};

    #[tokio::test]
    async fn the_owner_states_confirms_pauses_and_forgets_facts() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let new = json!({"text": "Prefers meetings in the morning", "category": "calendar"});
        let (status, body) = send(app, "POST", "/memory", Some(TOKEN), Some(new)).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let stated: Fact = serde_json::from_str(&body).unwrap();
        let bad = json!({"text": " "});
        assert_eq!(
            send(app, "POST", "/memory", Some(TOKEN), Some(bad)).await.0,
            StatusCode::BAD_REQUEST
        );

        let proposer = Proposer::agent("inbox".to_owned(), Uuid::now_v7());
        let learned = harness
            .desk
            .propose_fact(
                ProposedFact {
                    text: "Answers mail in batches".to_owned(),
                    confidence: 0.6,
                    ..ProposedFact::default()
                },
                &proposer,
            )
            .await
            .unwrap();
        let (_, body) = call(app, "GET", "/memory?status=proposed", Some(TOKEN)).await;
        assert!(body.contains("Answers mail in batches") && !body.contains("morning"));
        let confirm = format!("/memory/{}/confirm", learned.id);
        assert_eq!(
            call(app, "POST", &confirm, Some(TOKEN)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "POST", &confirm, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );

        let uri = format!("/memory/{}", stated.id);
        let pause = json!({"paused": true});
        let (status, body) = send(app, "PATCH", &uri, Some(TOKEN), Some(pause)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(serde_json::from_str::<Fact>(&body).unwrap().paused);
        let (_, body) = call(app, "GET", "/memory?category=calendar", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<Vec<Fact>>(&body).unwrap().len(), 1);
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
        assert_eq!(
            send(app, "PATCH", &uri, Some(TOKEN), Some(json!({})))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
