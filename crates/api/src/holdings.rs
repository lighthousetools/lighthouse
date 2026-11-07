//! The owner's own notes on holdings and reminders before earnings dates.
//! Data only: Lighthouse gives no advice and never trades.

use crate::{ApiError, ApiState, desk_error, internal, not_found};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use lighthouse_brain::{BrainError, Entry, EntrySummary, NewEntry, Source};
use lighthouse_desk::{NewTask, Proposer, SourceLink, TaskView};
use lighthouse_sync::{ItemFilter, SyncedItem};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Earnings further out than this get no reminder yet.
const LOOKAHEAD_DAYS: i64 = 60;

/// A thesis of the owner.
#[derive(Deserialize, ToSchema)]
pub struct NewThesis {
    /// `thesis` (the default), `metric` for a number the owner watches, such as
    /// "Gross margin above 40%", or `question` for one to answer.
    #[serde(default)]
    pub kind: Option<String>,
    /// Title, such as "Why I hold it".
    pub title: String,
    /// The owner's reasoning in Markdown.
    pub body: String,
}

/// When to remind before earnings.
#[derive(Deserialize, ToSchema)]
pub struct ReminderRequest {
    /// Days before the earnings date, default 3.
    #[serde(default)]
    pub days_before: Option<i64>,
}

/// Result of the reminders.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ReminderResult {
    /// IDs of the proposed tasks.
    pub proposed: Vec<Uuid>,
    /// Holdings that already have a reminder for that date.
    pub skipped: usize,
}

fn link(holding: &SyncedItem) -> String {
    format!("holding:{}:{}", holding.source, holding.external_id)
}

/// The kinds of the owner's notes on a holding.
const NOTE_KINDS: [&str; 3] = ["thesis", "metric", "question"];

/// The Brain link of a kind of note: theses link to the holding itself.
fn note_link(holding: &SyncedItem, kind: &str) -> String {
    if kind == "thesis" {
        link(holding)
    } else {
        format!("{}#{kind}", link(holding))
    }
}

/// The owner's notes on a holding, by kind.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct HoldingNotes {
    /// Why the owner holds it.
    pub theses: Vec<EntrySummary>,
    /// Numbers the owner watches.
    pub metrics: Vec<EntrySummary>,
    /// Open questions.
    pub questions: Vec<EntrySummary>,
}

/// Lists the owner's theses, metrics and questions on a holding, from the Brain.
#[utoipa::path(
    get,
    operation_id = "holding_notes",
    path = "/holdings/{id}/notes",
    params(("id" = Uuid, Path, description = "Holding ID")),
    responses((status = 200, body = HoldingNotes), (status = 404, description = "Unknown holding")),
    security(("bearer" = []))
)]
pub(crate) async fn notes(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<HoldingNotes>, ApiError> {
    let holding = holding(&state, id).await?;
    let linked = |kind: &'static str| {
        let state = state.clone();
        let key = note_link(&holding, kind);
        async move { state.brain.linked(&key, 100).await.map_err(internal) }
    };
    Ok(Json(HoldingNotes {
        theses: linked("thesis").await?,
        metrics: linked("metric").await?,
        questions: linked("question").await?,
    }))
}

/// Lists watched symbols: quotes and `watch` items without a position.
#[utoipa::path(
    get,
    operation_id = "watchlist",
    path = "/holdings/watchlist",
    responses((status = 200, body = Vec<SyncedItem>)),
    security(("bearer" = []))
)]
pub(crate) async fn watchlist(
    State(state): State<ApiState>,
) -> Result<Json<Vec<SyncedItem>>, ApiError> {
    Ok(Json(state.sync.watched().await.map_err(internal)?))
}

async fn holding(state: &ApiState, id: Uuid) -> Result<SyncedItem, ApiError> {
    state
        .sync
        .item(id)
        .await
        .map_err(internal)?
        .filter(|item| item.kind == "holding" || item.kind == "quote")
        .ok_or_else(|| not_found("holding"))
}

/// Lists the owner's theses on a holding, notes in the Brain linked to it.
#[utoipa::path(
    get,
    operation_id = "list_theses",
    path = "/holdings/{id}/theses",
    params(("id" = Uuid, Path, description = "Holding ID")),
    responses((status = 200, body = Vec<EntrySummary>), (status = 404, description = "Unknown holding")),
    security(("bearer" = []))
)]
pub(crate) async fn theses(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<EntrySummary>>, ApiError> {
    let holding = holding(&state, id).await?;
    Ok(Json(
        state
            .brain
            .linked(&link(&holding), 100)
            .await
            .map_err(internal)?,
    ))
}

/// Writes a thesis on a holding, as a note in the Brain linked to it.
#[utoipa::path(
    post,
    operation_id = "create_thesis",
    path = "/holdings/{id}/theses",
    params(("id" = Uuid, Path, description = "Holding ID")),
    request_body = NewThesis,
    responses((status = 201, body = Entry), (status = 404, description = "Unknown holding")),
    security(("bearer" = []))
)]
pub(crate) async fn create_thesis(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(thesis): Json<NewThesis>,
) -> Result<(StatusCode, Json<Entry>), ApiError> {
    let holding = holding(&state, id).await?;
    let kind = thesis.kind.as_deref().unwrap_or("thesis");
    if !NOTE_KINDS.contains(&kind) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`kind` must be thesis, metric or question".to_owned(),
        ));
    }
    let entry = state
        .brain
        .remember(NewEntry {
            kind: "note".to_owned(),
            title: thesis.title,
            body: thesis.body,
            sources: vec![Source {
                kind: "app".to_owned(),
                reference: "holdings".to_owned(),
            }],
            links: vec![note_link(&holding, kind)],
        })
        .await
        .map_err(|error| match error {
            BrainError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
            other => internal(other),
        })?;
    Ok((StatusCode::CREATED, Json(entry)))
}

/// Proposes a task before each earnings date in the next 60 days, read from
/// the `next_earnings_at` field of holdings. The owner accepts or rejects them.
#[utoipa::path(
    post,
    operation_id = "earnings_reminders",
    path = "/holdings/reminders",
    request_body = ReminderRequest,
    responses((status = 200, body = ReminderResult)),
    security(("bearer" = []))
)]
pub(crate) async fn reminders(
    State(state): State<ApiState>,
    Json(request): Json<ReminderRequest>,
) -> Result<Json<ReminderResult>, ApiError> {
    let days_before = request.days_before.unwrap_or(3).clamp(0, 30);
    let filter = ItemFilter {
        kinds: vec!["holding".to_owned()],
        limit: 500,
        ..ItemFilter::default()
    };
    let holdings = state.sync.items(&filter).await.map_err(internal)?.items;
    let now = Utc::now();
    let today = now.with_timezone(&state.zone).date_naive();
    let existing = state
        .desk
        .tasks(TaskView::All, today, 1000)
        .await
        .map_err(desk_error)?;
    let mut result = ReminderResult {
        proposed: Vec::new(),
        skipped: 0,
    };
    for holding in holdings {
        let Some(earnings) = holding
            .data
            .get("next_earnings_at")
            .and_then(lighthouse_sync::mapping::parse_time)
            .filter(|time| *time > now && *time - now < Duration::days(LOOKAHEAD_DAYS))
        else {
            continue;
        };
        let date = earnings.with_timezone(&state.zone).date_naive();
        let title = format!(
            "Review {} before its earnings on {date}",
            holding
                .title
                .clone()
                .unwrap_or_else(|| holding.external_id.clone())
        );
        let reference = holding.id.to_string();
        if existing.iter().any(|task| {
            task.title == title
                && task
                    .source
                    .as_ref()
                    .is_some_and(|source| source.reference == reference)
        }) {
            result.skipped += 1;
            continue;
        }
        let due = (date - Duration::days(days_before)).max(today);
        let task = state
            .desk
            .create_task(
                NewTask {
                    title,
                    notes: Some("Check your theses on it. Lighthouse gives no advice.".to_owned()),
                    due_on: Some(due),
                    source: Some(SourceLink {
                        kind: "holding".to_owned(),
                        reference,
                        label: holding.title.clone(),
                        url: holding.url.clone(),
                    }),
                    ..NewTask::default()
                },
                Some(Proposer::system(lighthouse_desk::Origin::Holdings)),
            )
            .await
            .map_err(desk_error)?;
        result.proposed.push(task.id);
    }
    state
        .events
        .emit("tasks.changed", serde_json::json!({"id": null}));
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_sync::MappedItem;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn metrics_and_questions_are_kept_apart_and_watched_symbols_listed() {
        let harness = harness(Some(TOKEN)).await;
        let symbol = |id: &str| MappedItem {
            external_id: id.to_owned(),
            title: Some(id.to_owned()),
            data: json!({}),
            ..MappedItem::default()
        };
        let held = harness
            .sync
            .upsert("depot", "holdings", "holding", &[symbol("EXA")], &[])
            .await
            .unwrap();
        harness
            .sync
            .upsert(
                "depot",
                "holdings",
                "quote",
                &[symbol("EXA"), symbol("TRY")],
                &[],
            )
            .await
            .unwrap();
        let app = &harness.app;
        let uri = format!("/holdings/{}/theses", held[0].id);
        for (kind, title) in [
            (Some("metric"), "Margin above 40%"),
            (Some("question"), "Will the plant pay off?"),
            (None, "Why"),
        ] {
            let (status, body) = send(
                app,
                "POST",
                &uri,
                Some(TOKEN),
                Some(json!({"kind": kind, "title": title, "body": "-"})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, _) = send(
            app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"kind": "rumor", "title": "x", "body": "y"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, body) = call(
            app,
            "GET",
            &format!("/holdings/{}/notes", held[0].id),
            Some(TOKEN),
        )
        .await;
        let notes: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(notes["metrics"][0]["title"], "Margin above 40%", "{body}");
        assert_eq!(notes["questions"].as_array().unwrap().len(), 1);
        assert_eq!(notes["theses"].as_array().unwrap().len(), 1);
        let (_, body) = call(app, "GET", "/holdings/watchlist", Some(TOKEN)).await;
        let watched: Vec<Value> = serde_json::from_str(&body).expect(&body);
        assert_eq!(watched.len(), 1, "{body}");
        assert_eq!(watched[0]["external_id"], "TRY");
    }

    #[tokio::test]
    async fn theses_are_linked_and_earnings_become_reminders() {
        let harness = harness(Some(TOKEN)).await;
        let soon = (Utc::now() + Duration::days(10)).to_rfc3339();
        let far = (Utc::now() + Duration::days(200)).to_rfc3339();
        let stored = harness
            .sync
            .upsert(
                "depot",
                "holdings",
                "holding",
                &[
                    MappedItem {
                        external_id: "ACME".to_owned(),
                        title: Some("Acme".to_owned()),
                        data: json!({"next_earnings_at": soon}),
                        ..MappedItem::default()
                    },
                    MappedItem {
                        external_id: "FAR".to_owned(),
                        data: json!({"next_earnings_at": far}),
                        ..MappedItem::default()
                    },
                ],
                &[],
            )
            .await
            .unwrap();
        let app = &harness.app;
        let uri = format!("/holdings/{}/theses", stored[0].id);
        let (status, _) = send(
            app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"title": "Why I hold it", "body": "Steady"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, body) = call(app, "GET", &uri, Some(TOKEN)).await;
        let theses: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(theses.len(), 1);
        let other = format!("/holdings/{}/theses", stored[1].id);
        assert_eq!(call(app, "GET", &other, Some(TOKEN)).await.1, "[]");
        let missing = format!("/holdings/{}/theses", Uuid::nil());
        assert_eq!(
            call(app, "GET", &missing, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );

        let (_, body) = send(
            app,
            "POST",
            "/holdings/reminders",
            Some(TOKEN),
            Some(json!({})),
        )
        .await;
        let result: ReminderResult = serde_json::from_str(&body).unwrap();
        assert_eq!(result.proposed.len(), 1, "only earnings within 60 days");
        let (_, body) = send(
            app,
            "POST",
            "/holdings/reminders",
            Some(TOKEN),
            Some(json!({"days_before": 5})),
        )
        .await;
        let again: ReminderResult = serde_json::from_str(&body).unwrap();
        assert_eq!((again.proposed.len(), again.skipped), (0, 1));
        let (_, body) = call(app, "GET", "/tasks?view=proposed", Some(TOKEN)).await;
        assert!(body.contains("Review Acme before its earnings"));
        assert!(body.contains("\"origin\":\"holdings\""));
        assert!(body.contains("\"run_id\":null"));
    }
}
