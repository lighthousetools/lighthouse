//! Structured briefing documents: morning briefs and weekly recaps.

use crate::{ApiError, ApiState, desk_error, not_found};
use axum::Json;
use axum::extract::{Path, Query, State};
use lighthouse_desk::{BriefingDocument, MORNING_BRIEF, WEEKLY_RECAP};
use serde::Deserialize;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 20;

/// Filter of the document list.
#[derive(Deserialize)]
pub(crate) struct DocumentQuery {
    kind: Option<String>,
    limit: Option<i64>,
}

fn limit(query: &DocumentQuery) -> i64 {
    query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 200)
}

/// Lists documents, latest period first.
#[utoipa::path(
    get,
    operation_id = "list_documents",
    path = "/documents",
    params(
        ("kind" = Option<String>, Query, description = "morning_brief, weekly_recap or another kind"),
        ("limit" = Option<i64>, Query, description = "Maximum number of documents")
    ),
    responses((status = 200, body = Vec<BriefingDocument>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<DocumentQuery>,
) -> Result<Json<Vec<BriefingDocument>>, ApiError> {
    let documents = state
        .desk
        .documents(query.kind.as_deref(), limit(&query))
        .await
        .map_err(desk_error)?;
    Ok(Json(documents))
}

async fn latest(state: &ApiState, kind: &str) -> Result<Json<BriefingDocument>, ApiError> {
    let document = state.desk.latest_document(kind).await.map_err(desk_error)?;
    document.map(Json).ok_or_else(|| not_found("document"))
}

/// Returns the latest morning brief.
#[utoipa::path(
    get,
    operation_id = "latest_brief",
    path = "/briefs/latest",
    responses((status = 200, body = BriefingDocument), (status = 404, description = "No brief yet")),
    security(("bearer" = []))
)]
pub(crate) async fn latest_brief(
    State(state): State<ApiState>,
) -> Result<Json<BriefingDocument>, ApiError> {
    latest(&state, MORNING_BRIEF).await
}

/// Lists weekly recaps, latest week first.
#[utoipa::path(
    get,
    operation_id = "list_recaps",
    path = "/recaps",
    params(("limit" = Option<i64>, Query, description = "Maximum number of recaps")),
    responses((status = 200, body = Vec<BriefingDocument>)),
    security(("bearer" = []))
)]
pub(crate) async fn recaps(
    State(state): State<ApiState>,
    Query(query): Query<DocumentQuery>,
) -> Result<Json<Vec<BriefingDocument>>, ApiError> {
    let recaps = state
        .desk
        .documents(Some(WEEKLY_RECAP), limit(&query))
        .await
        .map_err(desk_error)?;
    Ok(Json(recaps))
}

/// Returns the weekly recap of an ISO week, such as `2026-W45`, or `latest`.
#[utoipa::path(
    get,
    operation_id = "get_recap",
    path = "/recaps/{week}",
    params(("week" = String, Path, description = "ISO week like 2026-W45, or latest")),
    responses((status = 200, body = BriefingDocument), (status = 404, description = "No recap for that week")),
    security(("bearer" = []))
)]
pub(crate) async fn recap(
    State(state): State<ApiState>,
    Path(week): Path<String>,
) -> Result<Json<BriefingDocument>, ApiError> {
    if week == "latest" {
        return latest(&state, WEEKLY_RECAP).await;
    }
    let document = state
        .desk
        .document_for(WEEKLY_RECAP, &week)
        .await
        .map_err(desk_error)?;
    document.map(Json).ok_or_else(|| not_found("recap"))
}

/// Returns the document a run wrote.
#[utoipa::path(
    get,
    operation_id = "run_document",
    path = "/runs/{id}/document",
    params(("id" = Uuid, Path, description = "Run ID")),
    responses((status = 200, body = BriefingDocument), (status = 404, description = "The run wrote none")),
    security(("bearer" = []))
)]
pub(crate) async fn of_run(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<BriefingDocument>, ApiError> {
    let document = state.desk.run_document(id).await.map_err(desk_error)?;
    document.map(Json).ok_or_else(|| not_found("document"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness};
    use axum::http::StatusCode;
    use lighthouse_core::RunId;
    use lighthouse_desk::extract_document;

    #[tokio::test]
    async fn briefs_and_recaps_are_found_by_kind_and_week() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        assert_eq!(
            call(app, "GET", "/briefs/latest", Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let mut runs = Vec::new();
        for (kind, period) in [
            ("morning_brief", "2026-11-06"),
            ("weekly_recap", "2026-W44"),
            ("weekly_recap", "2026-W45"),
        ] {
            let run = RunId::new();
            harness
                .audit
                .start_run(run, "briefing", "cron")
                .await
                .unwrap();
            let report = format!(
                "x\n```lighthouse-document\n{{\"kind\": \"{kind}\", \"title\": \"{period}\", \"sections\": []}}\n```"
            );
            let content = extract_document(&report).1.unwrap().unwrap();
            harness
                .desk
                .save_document(run.as_uuid(), "briefing", period, &content)
                .await
                .unwrap();
            runs.push(run);
        }
        let (status, body) = call(app, "GET", "/briefs/latest", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("2026-11-06"));
        let (_, body) = call(app, "GET", "/recaps", Some(TOKEN)).await;
        let recaps: Vec<BriefingDocument> = serde_json::from_str(&body).unwrap();
        assert_eq!(recaps[0].period, "2026-W45");
        let (_, body) = call(app, "GET", "/recaps/latest", Some(TOKEN)).await;
        assert!(body.contains("2026-W45"));
        let (status, body) = call(app, "GET", "/recaps/2026-W44", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("2026-W44"));
        assert_eq!(
            call(app, "GET", "/recaps/2020-W01", Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let (_, body) = call(
            app,
            "GET",
            "/documents?kind=weekly_recap&limit=1",
            Some(TOKEN),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Vec<BriefingDocument>>(&body)
                .unwrap()
                .len(),
            1
        );
        let uri = format!("/runs/{}/document", runs[0].as_uuid());
        assert_eq!(call(app, "GET", &uri, Some(TOKEN)).await.0, StatusCode::OK);
        let uri = format!("/runs/{}/document", Uuid::nil());
        assert_eq!(
            call(app, "GET", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
