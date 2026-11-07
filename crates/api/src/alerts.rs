//! Things that broke.

use crate::{ApiError, ApiState, desk_error, not_found};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lighthouse_desk::{Alert, AlertCounts, AlertView};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 100;

/// Filter of the alert list.
#[derive(Deserialize)]
pub(crate) struct AlertQuery {
    view: Option<AlertView>,
    limit: Option<i64>,
}

/// Lists alerts, unresolved and worst first.
#[utoipa::path(
    get,
    operation_id = "list_alerts",
    path = "/alerts",
    params(
        ("view" = Option<AlertView>, Query, description = "active (default), open, acknowledged, resolved or all"),
        ("limit" = Option<i64>, Query, description = "Maximum number of alerts")
    ),
    responses((status = 200, body = Vec<Alert>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<AlertQuery>,
) -> Result<Json<Vec<Alert>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
    let alerts = state
        .desk
        .alerts(query.view.unwrap_or_default(), limit)
        .await
        .map_err(desk_error)?;
    Ok(Json(alerts))
}

/// Counts unresolved alerts per severity.
#[utoipa::path(
    get,
    operation_id = "count_alerts",
    path = "/alerts/counts",
    responses((status = 200, body = AlertCounts)),
    security(("bearer" = []))
)]
pub(crate) async fn counts(State(state): State<ApiState>) -> Result<Json<AlertCounts>, ApiError> {
    Ok(Json(state.desk.alert_counts().await.map_err(desk_error)?))
}

/// Returns an alert.
#[utoipa::path(
    get,
    operation_id = "get_alert",
    path = "/alerts/{id}",
    params(("id" = Uuid, Path, description = "Alert ID")),
    responses((status = 200, body = Alert), (status = 404, description = "Unknown alert")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Alert>, ApiError> {
    let alert = state.desk.alert(id).await.map_err(desk_error)?;
    alert.map(Json).ok_or_else(|| not_found("alert"))
}

/// One day of an alert's chart.
#[derive(Clone, Debug, serde::Serialize, Deserialize, utoipa::ToSchema)]
pub struct SeriesPoint {
    /// Day in UTC, as `YYYY-MM-DD`.
    pub day: String,
    /// Runs or refreshes that day.
    pub total: i64,
    /// Those that failed.
    pub failed: i64,
    /// Cost of the runs in EUR. Zero for refreshes.
    pub cost_eur: f64,
}

/// The chart of an alert.
#[derive(Clone, Debug, serde::Serialize, Deserialize, utoipa::ToSchema)]
pub struct AlertSeries {
    /// `runs`, `syncs` or `none` when the alert has no history to chart.
    pub metric: String,
    /// One point per day, oldest first.
    pub points: Vec<SeriesPoint>,
}

/// Length of an alert's chart.
#[derive(Deserialize)]
pub(crate) struct SeriesQuery {
    days: Option<i32>,
}

/// Returns a daily series for an alert's chart: runs and failures of the agent
/// for run and cost alerts, failed runs that name the connector for connector
/// alerts, and refreshes of the source for check alerts.
#[utoipa::path(
    get,
    operation_id = "alert_series",
    path = "/alerts/{id}/series",
    params(
        ("id" = Uuid, Path, description = "Alert ID"),
        ("days" = Option<i32>, Query, description = "Days, default 14, at most 90")
    ),
    responses((status = 200, body = AlertSeries), (status = 404, description = "Unknown alert")),
    security(("bearer" = []))
)]
pub(crate) async fn series(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Query(query): Query<SeriesQuery>,
) -> Result<Json<AlertSeries>, ApiError> {
    let alert = state
        .desk
        .alert(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("alert"))?;
    let days = query.days.unwrap_or(14).clamp(1, 90);
    let runs = |rows: Vec<(String, i64, i64, f64)>| AlertSeries {
        metric: "runs".to_owned(),
        points: rows
            .into_iter()
            .map(|(day, total, failed, cost_eur)| SeriesPoint {
                day,
                total,
                failed,
                cost_eur,
            })
            .collect(),
    };
    let audit = &state.audit;
    let series = match alert.key.split_once(':') {
        Some(("run" | "cost", agent)) => runs(
            audit
                .daily_runs(Some(agent), None, days)
                .await
                .map_err(crate::internal)?,
        ),
        Some(("connector", connector)) => runs(
            audit
                .daily_runs(None, Some(&format!("`{connector}`")), days)
                .await
                .map_err(crate::internal)?,
        ),
        Some(("check", rest)) => {
            let source = rest.split_once(':').map_or(rest, |(source, _)| source);
            let rows = state
                .sync
                .daily_syncs(source, days)
                .await
                .map_err(crate::internal)?;
            AlertSeries {
                metric: "syncs".to_owned(),
                points: rows
                    .into_iter()
                    .map(|(day, total, failed)| SeriesPoint {
                        day,
                        total,
                        failed,
                        cost_eur: 0.0,
                    })
                    .collect(),
            }
        }
        _ => AlertSeries {
            metric: "none".to_owned(),
            points: Vec::new(),
        },
    };
    Ok(Json(series))
}

fn settled(state: &ApiState, alert: Option<Alert>, what: &str) -> Result<Json<Alert>, ApiError> {
    let alert = alert.ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            format!("the alert is {what} or unknown"),
        )
    })?;
    state.events.emit("alerts.changed", json!({"id": alert.id}));
    Ok(Json(alert))
}

/// Acknowledges an open alert. It stays visible until it is resolved.
#[utoipa::path(
    post,
    operation_id = "acknowledge_alert",
    path = "/alerts/{id}/acknowledge",
    params(("id" = Uuid, Path, description = "Alert ID")),
    responses((status = 200, body = Alert), (status = 409, description = "Not open or unknown")),
    security(("bearer" = []))
)]
pub(crate) async fn acknowledge(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Alert>, ApiError> {
    let alert = state.desk.acknowledge_alert(id).await.map_err(desk_error)?;
    settled(&state, alert, "not open")
}

/// Resolves an alert. If the problem happens again, a new alert opens.
#[utoipa::path(
    post,
    operation_id = "resolve_alert",
    path = "/alerts/{id}/resolve",
    params(("id" = Uuid, Path, description = "Alert ID")),
    responses((status = 200, body = Alert), (status = 409, description = "Resolved already or unknown")),
    security(("bearer" = []))
)]
pub(crate) async fn resolve(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Alert>, ApiError> {
    let alert = state.desk.resolve_alert(id).await.map_err(desk_error)?;
    settled(&state, alert, "resolved already")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness};
    use lighthouse_desk::{NewAlert, Severity};
    use serde_json::Value;

    #[tokio::test]
    async fn alerts_are_listed_counted_acknowledged_and_resolved() {
        let harness = harness(Some(TOKEN)).await;
        let alert = harness
            .desk
            .raise_alert(NewAlert {
                kind: "connector".to_owned(),
                key: "connector:linear".to_owned(),
                severity: Severity::Critical,
                title: "The connector linear needs a new authorization".to_owned(),
                detail: None,
                source: None,
            })
            .await
            .unwrap()
            .alert;
        let app = &harness.app;
        let (_, body) = call(app, "GET", "/alerts", Some(TOKEN)).await;
        assert!(body.contains("connector:linear"));
        let (_, body) = call(app, "GET", "/alerts/counts", Some(TOKEN)).await;
        let counts: AlertCounts = serde_json::from_str(&body).unwrap();
        assert_eq!(counts.critical, 1);

        let uri = format!("/alerts/{}", alert.id);
        assert_eq!(call(app, "GET", &uri, Some(TOKEN)).await.0, StatusCode::OK);
        let acknowledge = format!("{uri}/acknowledge");
        assert_eq!(
            call(app, "POST", &acknowledge, Some(TOKEN)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "POST", &acknowledge, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );
        let resolve = format!("{uri}/resolve");
        assert_eq!(
            call(app, "POST", &resolve, Some(TOKEN)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "POST", &resolve, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );
        let (_, body) = call(app, "GET", "/alerts", Some(TOKEN)).await;
        assert_eq!(body, "[]");
        let (_, body) = call(app, "GET", "/alerts?view=resolved", Some(TOKEN)).await;
        assert!(body.contains("connector:linear"));
        let (_, body) = call(app, "GET", &format!("{uri}/series?days=5"), Some(TOKEN)).await;
        let series: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(series["metric"], "runs");
        assert_eq!(series["points"].as_array().unwrap().len(), 5);
        for (key, metric) in [
            ("run:brief", "runs"),
            ("check:github:pr/ci", "syncs"),
            ("node:x", "none"),
        ] {
            let other = harness
                .desk
                .raise_alert(NewAlert {
                    kind: "x".to_owned(),
                    key: key.to_owned(),
                    severity: Severity::Warning,
                    title: key.to_owned(),
                    detail: None,
                    source: None,
                })
                .await
                .unwrap()
                .alert;
            let (_, body) = call(
                app,
                "GET",
                &format!("/alerts/{}/series", other.id),
                Some(TOKEN),
            )
            .await;
            let series: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(series["metric"], metric, "{key}");
        }
        let unknown = format!("/alerts/{}", Uuid::nil());
        assert_eq!(
            call(app, "GET", &format!("{unknown}/series"), Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "GET", &unknown, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
