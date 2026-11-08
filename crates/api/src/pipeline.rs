//! Suggestions for the pipeline: deal values from similar deals, and the top
//! deals as proposed tasks. Plain heuristics, explained, never automatic.

use crate::{ApiError, Scoped, desk_error, internal, not_found};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use lighthouse_desk::{NewTask, Proposer, SourceLink, TaskView};
use lighthouse_sync::{ItemFilter, Order, SyncedItem};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use utoipa::ToSchema;
use uuid::Uuid;

const SIMILAR: i64 = 50;
const EXAMPLES: usize = 5;

/// A won deal the suggestion is based on.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SimilarDeal {
    /// Item ID.
    pub id: Uuid,
    /// Title.
    pub title: Option<String>,
    /// Value.
    pub amount: f64,
}

/// A suggested value for a deal.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct DealSuggestion {
    /// The median value of similar won deals, if there are any.
    pub suggested: Option<f64>,
    /// Currency of the deal.
    pub currency: Option<String>,
    /// Number of won deals it is based on.
    pub basis: usize,
    /// How it was worked out.
    pub explanation: String,
    /// Some of those deals.
    pub examples: Vec<SimilarDeal>,
}

/// Returns the median of values.
pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

/// Suggests a value for a deal: the median of similar won deals of the same
/// source, and of the same pipeline or category if the deal has one.
#[utoipa::path(
    get,
    operation_id = "suggest_deal_value",
    path = "/deals/{id}/suggestion",
    params(("id" = Uuid, Path, description = "Deal ID")),
    responses((status = 200, body = DealSuggestion), (status = 404, description = "Unknown deal")),
    security(("bearer" = []))
)]
pub(crate) async fn suggestion(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<DealSuggestion>, ApiError> {
    let deal = state
        .sync
        .item(id)
        .await
        .map_err(internal)?
        .filter(|item| item.kind == "deal")
        .ok_or_else(|| not_found("deal"))?;
    let similar = state
        .sync
        .similar_won_deals(&deal, SIMILAR)
        .await
        .map_err(internal)?;
    let same_currency: Vec<&SyncedItem> = similar
        .iter()
        .filter(|other| deal.currency.is_none() || other.currency == deal.currency)
        .collect();
    let mut values: Vec<f64> = same_currency
        .iter()
        .filter_map(|other| other.amount)
        .collect();
    let suggested = median(&mut values);
    let explanation = match suggested {
        Some(value) => format!(
            "Median of {} won deals of the same source{}: {value:.0}.",
            values.len(),
            ["pipeline", "category"]
                .iter()
                .find(|field| deal.data.get(**field).is_some())
                .map(|field| format!(" and {field}"))
                .unwrap_or_default()
        ),
        None => "No won deals like this one yet, so there is nothing to compare with.".to_owned(),
    };
    Ok(Json(DealSuggestion {
        suggested,
        currency: deal.currency.clone(),
        basis: values.len(),
        explanation,
        examples: same_currency
            .iter()
            .take(EXAMPLES)
            .map(|other| SimilarDeal {
                id: other.id,
                title: other.title.clone(),
                amount: other.amount.unwrap_or_default(),
            })
            .collect(),
    }))
}

/// Which deals to turn into tasks.
#[derive(Deserialize, ToSchema)]
pub struct TopDealsRequest {
    /// How many, default 5.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only this organization.
    #[serde(default)]
    pub org: Option<String>,
}

/// Result of turning deals into tasks.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct TopDealsResult {
    /// IDs of the proposed tasks.
    pub proposed: Vec<Uuid>,
    /// Deals that already have an open or proposed task.
    pub skipped: usize,
}

fn weight(deal: &SyncedItem) -> f64 {
    let probability = deal
        .data
        .get("probability")
        .and_then(lighthouse_sync::mapping::parse_amount)
        .unwrap_or(100.0)
        / 100.0;
    let cold = if deal.flags.iter().any(|flag| flag == "cold") {
        1.2
    } else {
        1.0
    };
    deal.amount.unwrap_or_default() * probability * cold
}

/// Proposes a task for each of the top open deals: the highest value times
/// probability, with cold deals first among equals. The owner accepts or
/// rejects each task; nothing is sent.
#[utoipa::path(
    post,
    operation_id = "top_deals_as_tasks",
    path = "/deals/top-tasks",
    request_body = TopDealsRequest,
    responses((status = 200, body = TopDealsResult)),
    security(("bearer" = []))
)]
pub(crate) async fn top_tasks(
    Scoped(state): Scoped,
    Json(request): Json<TopDealsRequest>,
) -> Result<Json<TopDealsResult>, ApiError> {
    let filter = ItemFilter {
        kinds: vec!["deal".to_owned()],
        org: request.org.clone(),
        open: true,
        order: Order::Recent,
        limit: 500,
        ..ItemFilter::default()
    };
    let mut deals = state.sync.items(&filter).await.map_err(internal)?.items;
    deals.sort_by(|left, right| weight(right).total_cmp(&weight(left)));
    let today = Utc::now().with_timezone(&state.zone).date_naive();
    let mut existing = state
        .desk
        .tasks(TaskView::All, today, 1000)
        .await
        .map_err(desk_error)?;
    existing.retain(|task| {
        task.source
            .as_ref()
            .is_some_and(|source| source.kind == "deal")
    });
    let mut result = TopDealsResult {
        proposed: Vec::new(),
        skipped: 0,
    };
    for deal in deals
        .into_iter()
        .take(request.limit.unwrap_or(5).clamp(1, 20))
    {
        let reference = deal.id.to_string();
        if existing.iter().any(|task| {
            task.source
                .as_ref()
                .is_some_and(|source| source.reference == reference)
        }) {
            result.skipped += 1;
            continue;
        }
        let title = deal
            .title
            .clone()
            .unwrap_or_else(|| deal.external_id.clone());
        let mut notes = String::new();
        if let Some(state_name) = &deal.state {
            let _ = writeln!(notes, "Stage: {state_name}");
        }
        if let Some(amount) = deal.amount {
            let _ = writeln!(
                notes,
                "Value: {amount:.0} {}",
                deal.currency.clone().unwrap_or_default()
            );
        }
        if deal.flags.iter().any(|flag| flag == "cold") {
            notes.push_str("No activity for 30 days.\n");
        }
        let task = state
            .desk
            .create_task(
                NewTask {
                    title: format!("Move {title} forward"),
                    notes: Some(notes),
                    source: Some(SourceLink {
                        kind: "deal".to_owned(),
                        reference,
                        label: Some(title),
                        url: deal.url.clone(),
                    }),
                    ..NewTask::default()
                },
                Some(Proposer::system(lighthouse_desk::Origin::Pipeline)),
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

/// Format of the numbers.
#[derive(Deserialize)]
pub(crate) struct ExportQuery {
    org: Option<String>,
    months: Option<i32>,
}

/// Exports the key numbers as CSV: one row per currency, metric and period.
#[utoipa::path(
    get,
    operation_id = "export_numbers",
    path = "/numbers/export",
    params(
        ("org" = Option<String>, Query, description = "Only sources of this organization"),
        ("months" = Option<i32>, Query, description = "Months of revenue, default 12")
    ),
    responses((status = 200, description = "CSV", content_type = "text/csv")),
    security(("bearer" = []))
)]
pub(crate) async fn export(
    Scoped(state): Scoped,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let months = query.months.unwrap_or(12).clamp(1, 36);
    let numbers = state
        .sync
        .numbers(query.org.as_deref(), months, Utc::now())
        .await
        .map_err(internal)?;
    let mut csv = String::from("currency,metric,period,value\n");
    for currency in &numbers.currencies {
        let code = &currency.currency;
        let mut row = |metric: &str, period: &str, value: f64| {
            let _ = writeln!(csv, "{code},{metric},{period},{value}");
        };
        row("pipeline_open", "", currency.pipeline_open);
        row("pipeline_weighted", "", currency.pipeline_weighted);
        row("deals_open", "", currency.deals_open as f64);
        row("deals_cold", "", currency.deals_cold as f64);
        row("cash", "", currency.cash);
        row("monthly_burn", "", currency.monthly_burn);
        if let Some(runway) = currency.runway_months {
            row("runway_months", "", runway);
        }
        row("receivables_open", "", currency.receivables_open);
        row("receivables_overdue", "", currency.receivables_overdue);
        row("payables_open", "", currency.payables_open);
        row("holdings", "", currency.holdings);
        row("revenue_total", "", currency.revenue_total);
        row(
            "revenue_previous_total",
            "",
            currency.revenue_previous_total,
        );
        for month in &currency.revenue {
            row("revenue", &month.period, month.amount);
        }
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"numbers.csv\"",
            ),
        ],
        csv,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_sync::MappedItem;
    use serde_json::{Value, json};

    fn deal(id: &str, amount: f64, state: &str, data: Value) -> MappedItem {
        MappedItem {
            external_id: id.to_owned(),
            title: Some(format!("Deal {id}")),
            amount: Some(amount),
            currency: Some("EUR".to_owned()),
            state: Some(state.to_owned()),
            data,
            ..MappedItem::default()
        }
    }

    #[test]
    fn medians_of_odd_and_even_lists() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    #[tokio::test]
    async fn suggestions_tasks_and_csv_come_from_the_cache() {
        let harness = harness(Some(TOKEN)).await;
        let pipeline = json!({"pipeline": "services"});
        let stored = harness
            .sync
            .upsert(
                "crm",
                "deals",
                "deal",
                &[
                    deal("open", 1.0, "proposal", pipeline.clone()),
                    deal("small", 10_000.0, "proposal", json!({"probability": "10"})),
                    deal("w1", 8_000.0, "won", pipeline.clone()),
                    deal("w2", 12_000.0, "won", pipeline.clone()),
                    deal("w3", 99_000.0, "won", json!({"pipeline": "licenses"})),
                ],
                &[],
            )
            .await
            .unwrap();
        let app = &harness.app;
        let uri = format!("/deals/{}/suggestion", stored[0].id);
        let (status, body) = call(app, "GET", &uri, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let suggestion: DealSuggestion = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (suggestion.suggested, suggestion.basis),
            (Some(10_000.0), 2)
        );
        assert!(suggestion.explanation.contains("pipeline"));
        let lonely = format!("/deals/{}/suggestion", stored[1].id);
        let (_, body) = call(app, "GET", &lonely, Some(TOKEN)).await;
        let suggestion: DealSuggestion = serde_json::from_str(&body).unwrap();
        assert_eq!(
            suggestion.basis, 3,
            "without a pipeline every won deal compares"
        );
        let missing = format!("/deals/{}/suggestion", Uuid::nil());
        assert_eq!(
            call(app, "GET", &missing, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );

        let (_, body) = send(
            app,
            "POST",
            "/deals/top-tasks",
            Some(TOKEN),
            Some(json!({"limit": 1})),
        )
        .await;
        let result: TopDealsResult = serde_json::from_str(&body).unwrap();
        assert_eq!(result.proposed.len(), 1);
        let (_, body) = call(app, "GET", "/tasks?view=proposed", Some(TOKEN)).await;
        assert!(body.contains("Move Deal small forward"), "{body}");
        let tasks: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(tasks[0]["origin"], "pipeline");
        assert!(tasks[0]["run_id"].is_null() && tasks[0]["agent_id"].is_null());
        let (_, body) = send(
            app,
            "POST",
            "/deals/top-tasks",
            Some(TOKEN),
            Some(json!({"limit": 1})),
        )
        .await;
        let again: TopDealsResult = serde_json::from_str(&body).unwrap();
        assert_eq!((again.proposed.len(), again.skipped), (0, 1));

        let (status, body) = call(app, "GET", "/numbers/export?months=2", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("currency,metric,period,value\n"));
        assert!(body.contains("EUR,pipeline_open,,10001"), "{body}");
    }
}
