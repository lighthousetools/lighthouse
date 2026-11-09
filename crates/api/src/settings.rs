//! Settings: the monthly budget and the model routes. Lighthouse is a personal
//! assistant with one owner, so there are no members to manage.

use crate::catalog::ModelRoute;
use crate::{ApiError, ApiState, Scoped, desk_error, internal};
use axum::Json;
use axum::http::StatusCode;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The key of the budget setting.
pub const BUDGET: &str = "budget";
const DEFAULT_ALERT_PERCENT: u8 = 80;

/// The owner's monthly budget for model costs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Budget {
    /// Budget per calendar month in EUR. `None` means no budget.
    pub monthly_eur: Option<f64>,
    /// At which share of the budget an alert warns, in percent.
    #[serde(default = "default_percent")]
    pub alert_at_percent: u8,
}

const fn default_percent() -> u8 {
    DEFAULT_ALERT_PERCENT
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            monthly_eur: None,
            alert_at_percent: DEFAULT_ALERT_PERCENT,
        }
    }
}

/// The cost limit of one run of an agent.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RunLimit {
    /// Agent ID.
    pub agent_id: String,
    /// Limit per run in EUR, from the agent file.
    pub max_cost_eur: f64,
}

/// The budget with this month's spending.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BudgetView {
    /// The budget.
    #[serde(flatten)]
    pub budget: Budget,
    /// Model costs since the start of this month, in the owner's time zone.
    pub spent_this_month_eur: f64,
    /// What is left of the budget, if there is one.
    pub remaining_eur: Option<f64>,
    /// The cost limits per run.
    pub run_limits: Vec<RunLimit>,
}

/// The start of the current month in a time zone.
pub fn month_start(zone: chrono_tz::Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&zone);
    zone.with_ymd_and_hms(local.year(), local.month(), 1, 0, 0, 0)
        .earliest()
        .map_or(now, |start| start.with_timezone(&Utc))
}

/// The stored budget, or none.
pub async fn budget(state: &ApiState) -> Result<Budget, ApiError> {
    Ok(state
        .desk
        .setting::<Budget>(BUDGET)
        .await
        .map_err(desk_error)?
        .unwrap_or_default())
}

async fn view(state: &ApiState, budget: Budget) -> Result<BudgetView, ApiError> {
    let spent = state
        .audit
        .cost_since(month_start(state.tenant_zone, Utc::now()))
        .await
        .map_err(internal)?;
    Ok(BudgetView {
        remaining_eur: budget.monthly_eur.map(|limit| limit - spent),
        budget,
        spent_this_month_eur: spent,
        run_limits: state
            .catalog
            .agents()
            .into_iter()
            .map(|agent| RunLimit {
                agent_id: agent.id,
                max_cost_eur: agent.max_cost_eur,
            })
            .collect(),
    })
}

/// Returns the monthly budget, this month's spending and the cost limits per run.
#[utoipa::path(
    get,
    operation_id = "get_budget",
    path = "/settings/budget",
    responses((status = 200, body = BudgetView)),
    security(("bearer" = []))
)]
pub(crate) async fn get_budget(Scoped(state): Scoped) -> Result<Json<BudgetView>, ApiError> {
    let budget = budget(&state).await?;
    Ok(Json(view(&state, budget).await?))
}

/// Sets the monthly budget. An alert warns at `alert_at_percent` and again
/// when the budget is used up; runs are not stopped by it.
#[utoipa::path(
    put,
    operation_id = "set_budget",
    path = "/settings/budget",
    request_body = Budget,
    responses((status = 200, body = BudgetView), (status = 400, description = "Invalid budget"),
        (status = 403, description = "Needs the owner or admin role")),
    security(("bearer" = []))
)]
pub(crate) async fn set_budget(
    Scoped(state): Scoped,
    caller: crate::Caller,
    Json(budget): Json<Budget>,
) -> Result<Json<BudgetView>, ApiError> {
    caller.require_manager()?;
    if budget
        .monthly_eur
        .is_some_and(|limit| !limit.is_finite() || limit < 0.0)
        || !(1..=100).contains(&budget.alert_at_percent)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`monthly_eur` must be zero or more and `alert_at_percent` between 1 and 100"
                .to_owned(),
        ));
    }
    state
        .desk
        .set_setting(BUDGET, &budget)
        .await
        .map_err(desk_error)?;
    state
        .events
        .emit("settings.changed", serde_json::json!({"key": BUDGET}));
    Ok(Json(view(&state, budget).await?))
}

/// Lists which provider and model serve each task. Read-only: routing lives in
/// the configuration.
#[utoipa::path(
    get,
    operation_id = "model_routes",
    path = "/settings/models",
    responses((status = 200, body = Vec<ModelRoute>)),
    security(("bearer" = []))
)]
pub(crate) async fn models(Scoped(state): Scoped) -> Json<Vec<ModelRoute>> {
    Json(state.catalog.models())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn the_budget_is_set_and_shows_this_months_spending() {
        let harness = harness(Some(TOKEN)).await;
        let (status, body) = call(&harness.app, "GET", "/settings/budget", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let empty: Value = serde_json::from_str(&body).unwrap();
        assert!(empty["monthly_eur"].is_null() && empty["remaining_eur"].is_null());
        assert_eq!(empty["alert_at_percent"], 80);
        assert_eq!(empty["run_limits"][0]["agent_id"], "brief");
        let (status, body) = send(
            &harness.app,
            "PUT",
            "/settings/budget",
            Some(TOKEN),
            Some(json!({"monthly_eur": 25.0, "alert_at_percent": 90})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let set: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (set["monthly_eur"].as_f64(), set["remaining_eur"].as_f64()),
            (Some(25.0), Some(25.0))
        );
        for bad in [
            json!({"monthly_eur": -1.0}),
            json!({"monthly_eur": 5.0, "alert_at_percent": 0}),
        ] {
            let (status, _) = send(
                &harness.app,
                "PUT",
                "/settings/budget",
                Some(TOKEN),
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, body) = call(&harness.app, "GET", "/settings/models", Some(TOKEN)).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    }

    #[test]
    fn months_start_in_the_owners_time_zone() {
        let now = "2026-11-30T23:30:00Z".parse().unwrap();
        let berlin = month_start(chrono_tz::Europe::Berlin, now);
        assert_eq!(
            berlin.to_rfc3339(),
            "2026-11-30T23:00:00+00:00",
            "December started in Berlin"
        );
        let utc = month_start(chrono_tz::UTC, now);
        assert_eq!(utc.to_rfc3339(), "2026-11-01T00:00:00+00:00");
    }
}
