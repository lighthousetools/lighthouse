//! Things that broke: failed runs, connector errors, offline desktop agents and
//! cost limits.
//!
//! Each problem has a key, such as `connector:linear`. While an alert of that key
//! is unresolved, a repeat raises its count and last-seen time instead of adding
//! a new alert, so the owner hears about a problem once.

use crate::{Desk, DeskError, SourceLink};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::str::FromStr;
use utoipa::ToSchema;
use uuid::Uuid;

const COLUMNS: &str = "id, kind, key, severity, title, detail, source_kind, source_ref, source_label, \
                       source_url, status, count, first_seen_at, last_seen_at, acknowledged_at, resolved_at";

fn rank(column: &str) -> String {
    format!("CASE {column} WHEN 'critical' THEN 3 WHEN 'warning' THEN 2 ELSE 1 END")
}

/// How bad a problem is.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Needs attention soon.
    Warning,
    /// Stops agents from working.
    Critical,
}

impl Severity {
    /// Returns the stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

impl FromStr for Severity {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        [Self::Info, Self::Warning, Self::Critical]
            .into_iter()
            .find(|severity| severity.as_str() == value)
            .ok_or_else(|| format!("unknown severity `{value}`"))
    }
}

/// An alert.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Alert {
    /// Alert ID.
    pub id: Uuid,
    /// `run_failed`, `connector`, `node_offline` or `cost_limit`.
    pub kind: String,
    /// Identifies the problem, such as `connector:linear`.
    pub key: String,
    /// How bad it is.
    pub severity: Severity,
    /// Short description.
    pub title: String,
    /// The latest error text.
    pub detail: Option<String>,
    /// What broke, such as a run or a node.
    pub source: Option<SourceLink>,
    /// `open`, `acknowledged` or `resolved`.
    pub status: String,
    /// How often it happened while unresolved.
    pub count: i32,
    /// First occurrence.
    pub first_seen_at: DateTime<Utc>,
    /// Latest occurrence.
    pub last_seen_at: DateTime<Utc>,
    /// When the owner acknowledged it.
    pub acknowledged_at: Option<DateTime<Utc>>,
    /// When it was resolved.
    pub resolved_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct AlertRow {
    id: Uuid,
    kind: String,
    key: String,
    severity: String,
    title: String,
    detail: Option<String>,
    source_kind: Option<String>,
    source_ref: Option<String>,
    source_label: Option<String>,
    source_url: Option<String>,
    status: String,
    count: i32,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    acknowledged_at: Option<DateTime<Utc>>,
    resolved_at: Option<DateTime<Utc>>,
}

impl From<AlertRow> for Alert {
    fn from(row: AlertRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            key: row.key,
            severity: row.severity.parse().unwrap_or(Severity::Warning),
            title: row.title,
            detail: row.detail,
            source: SourceLink::from_columns(
                row.source_kind,
                row.source_ref,
                row.source_label,
                row.source_url,
            ),
            status: row.status,
            count: row.count,
            first_seen_at: row.first_seen_at,
            last_seen_at: row.last_seen_at,
            acknowledged_at: row.acknowledged_at,
            resolved_at: row.resolved_at,
        }
    }
}

/// A problem to report.
#[derive(Clone, Debug)]
pub struct NewAlert {
    /// Kind of problem.
    pub kind: String,
    /// Identifies the problem.
    pub key: String,
    /// How bad it is.
    pub severity: Severity,
    /// Short description.
    pub title: String,
    /// Error text. Must not contain secrets.
    pub detail: Option<String>,
    /// What broke.
    pub source: Option<SourceLink>,
}

/// Result of raising an alert.
#[derive(Clone, Debug)]
pub struct Raised {
    /// The alert after the change.
    pub alert: Alert,
    /// Whether the problem is new, rather than a repeat of an unresolved one.
    pub is_new: bool,
    /// Whether the severity went up with this repeat.
    pub escalated: bool,
}

/// Number of unresolved alerts per severity.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AlertCounts {
    /// Critical alerts.
    pub critical: i64,
    /// Warnings.
    pub warning: i64,
    /// Information.
    pub info: i64,
}

/// Which alerts to list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertView {
    /// Open and acknowledged.
    #[default]
    Active,
    /// Not acknowledged yet.
    Open,
    /// Acknowledged, not resolved.
    Acknowledged,
    /// Resolved.
    Resolved,
    /// Everything.
    All,
}

impl Desk {
    /// Reports a problem. A repeat of an unresolved one counts up.
    pub async fn raise_alert(&self, alert: NewAlert) -> Result<Raised, DeskError> {
        let source = alert.source.as_ref();
        let current_rank = rank("alerts.severity");
        let new_rank = rank("EXCLUDED.severity");
        let previous = self.unresolved_alert(&alert.key).await?;
        let query = format!(
            "INSERT INTO alerts (id, tenant_id, kind, key, severity, title, detail, source_kind, \
             source_ref, source_label, source_url, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'open') \
             ON CONFLICT (tenant_id, key) WHERE status <> 'resolved' DO UPDATE SET \
             count = alerts.count + 1, last_seen_at = now(), title = EXCLUDED.title, \
             detail = EXCLUDED.detail, source_kind = EXCLUDED.source_kind, \
             source_ref = EXCLUDED.source_ref, source_label = EXCLUDED.source_label, \
             source_url = EXCLUDED.source_url, \
             severity = CASE WHEN {new_rank} > {current_rank} THEN EXCLUDED.severity \
                        ELSE alerts.severity END \
             RETURNING {COLUMNS}"
        );
        let row: AlertRow = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(&alert.kind)
            .bind(&alert.key)
            .bind(alert.severity.as_str())
            .bind(&alert.title)
            .bind(&alert.detail)
            .bind(source.map(|link| link.kind.clone()))
            .bind(source.map(|link| link.reference.clone()))
            .bind(source.and_then(|link| link.label.clone()))
            .bind(source.and_then(|link| link.url.clone()))
            .fetch_one(self.pool())
            .await?;
        let alert = Alert::from(row);
        let escalated = previous
            .as_ref()
            .is_some_and(|before| alert.severity > before.severity);
        Ok(Raised {
            is_new: previous.is_none(),
            escalated,
            alert,
        })
    }

    async fn unresolved_alert(&self, key: &str) -> Result<Option<Alert>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM alerts WHERE tenant_id = $1 AND key = $2 AND status <> 'resolved'"
        );
        let row: Option<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(key)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Lists alerts, latest first.
    pub async fn alerts(&self, view: AlertView, limit: i64) -> Result<Vec<Alert>, DeskError> {
        let filter = match view {
            AlertView::Active => "status <> 'resolved'",
            AlertView::Open => "status = 'open'",
            AlertView::Acknowledged => "status = 'acknowledged'",
            AlertView::Resolved => "status = 'resolved'",
            AlertView::All => "true",
        };
        let severity = rank("severity");
        let query = format!(
            "SELECT {COLUMNS} FROM alerts WHERE tenant_id = $1 AND {filter} \
             ORDER BY status = 'resolved', {severity} DESC, last_seen_at DESC LIMIT $2"
        );
        let rows: Vec<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(limit)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Returns an alert.
    pub async fn alert(&self, id: Uuid) -> Result<Option<Alert>, DeskError> {
        let query = format!("SELECT {COLUMNS} FROM alerts WHERE tenant_id = $1 AND id = $2");
        let row: Option<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Counts unresolved alerts per severity.
    pub async fn alert_counts(&self) -> Result<AlertCounts, DeskError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT severity, count(*) FROM alerts WHERE tenant_id = $1 AND status <> 'resolved' \
             GROUP BY severity",
        )
        .bind(self.tenant())
        .fetch_all(self.pool())
        .await?;
        let mut counts = AlertCounts::default();
        for (severity, count) in rows {
            match severity.parse() {
                Ok(Severity::Critical) => counts.critical = count,
                Ok(Severity::Warning) => counts.warning = count,
                _ => counts.info += count,
            }
        }
        Ok(counts)
    }

    /// Acknowledges an open alert. It stays until it is resolved.
    pub async fn acknowledge_alert(&self, id: Uuid) -> Result<Option<Alert>, DeskError> {
        let query = format!(
            "UPDATE alerts SET status = 'acknowledged', acknowledged_at = now() \
             WHERE tenant_id = $1 AND id = $2 AND status = 'open' RETURNING {COLUMNS}"
        );
        let row: Option<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Resolves an alert. A later repeat opens a new one.
    pub async fn resolve_alert(&self, id: Uuid) -> Result<Option<Alert>, DeskError> {
        let query = format!(
            "UPDATE alerts SET status = 'resolved', resolved_at = now() \
             WHERE tenant_id = $1 AND id = $2 AND status <> 'resolved' RETURNING {COLUMNS}"
        );
        let row: Option<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Resolves the unresolved alert of a problem, once it works again.
    /// Returns the resolved alerts.
    pub async fn resolve_alert_key(&self, key: &str) -> Result<Vec<Alert>, DeskError> {
        let query = format!(
            "UPDATE alerts SET status = 'resolved', resolved_at = now() \
             WHERE tenant_id = $1 AND key = $2 AND status <> 'resolved' RETURNING {COLUMNS}"
        );
        let rows: Vec<AlertRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(key)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    fn problem(key: &str, severity: Severity) -> NewAlert {
        NewAlert {
            kind: "connector".to_owned(),
            key: key.to_owned(),
            severity,
            title: format!("{key} fails"),
            detail: Some("timeout".to_owned()),
            source: None,
        }
    }

    #[tokio::test]
    async fn repeats_count_up_escalate_and_reopen_after_resolution() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        let first = desk
            .raise_alert(problem("connector:linear", Severity::Warning))
            .await
            .unwrap();
        assert!(first.is_new);
        let repeat = desk
            .raise_alert(problem("connector:linear", Severity::Info))
            .await
            .unwrap();
        assert!(!repeat.is_new && !repeat.escalated);
        assert_eq!(repeat.alert.id, first.alert.id);
        assert_eq!(repeat.alert.count, 2);
        assert_eq!(
            repeat.alert.severity,
            Severity::Warning,
            "severity never drops"
        );
        let worse = desk
            .raise_alert(problem("connector:linear", Severity::Critical))
            .await
            .unwrap();
        assert!(worse.escalated);
        assert_eq!(worse.alert.severity, Severity::Critical);

        let acknowledged = desk
            .acknowledge_alert(first.alert.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.status, "acknowledged");
        assert!(
            desk.acknowledge_alert(first.alert.id)
                .await
                .unwrap()
                .is_none()
        );
        let repeat = desk
            .raise_alert(problem("connector:linear", Severity::Critical))
            .await
            .unwrap();
        assert_eq!(
            repeat.alert.status, "acknowledged",
            "a repeat does not reopen"
        );

        assert_eq!(
            desk.resolve_alert_key("connector:linear")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            desk.resolve_alert_key("connector:linear")
                .await
                .unwrap()
                .is_empty()
        );
        let again = desk
            .raise_alert(problem("connector:linear", Severity::Warning))
            .await
            .unwrap();
        assert!(again.is_new);
        assert_ne!(again.alert.id, first.alert.id);
    }

    #[tokio::test]
    async fn views_and_counts_follow_the_status() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        let critical = desk
            .raise_alert(problem("a", Severity::Critical))
            .await
            .unwrap()
            .alert;
        let warning = desk
            .raise_alert(problem("b", Severity::Warning))
            .await
            .unwrap()
            .alert;
        desk.raise_alert(problem("c", Severity::Info))
            .await
            .unwrap();
        assert_eq!(
            desk.alert_counts().await.unwrap(),
            AlertCounts {
                critical: 1,
                warning: 1,
                info: 1
            }
        );
        let active = desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(active[0].id, critical.id, "worst first");
        desk.acknowledge_alert(warning.id).await.unwrap();
        desk.resolve_alert(critical.id).await.unwrap().unwrap();
        assert!(desk.resolve_alert(critical.id).await.unwrap().is_none());
        let desk = &desk;
        let count = |view| async move { desk.alerts(view, 10).await.unwrap().len() };
        assert_eq!(count(AlertView::Open).await, 1);
        assert_eq!(count(AlertView::Acknowledged).await, 1);
        assert_eq!(count(AlertView::Resolved).await, 1);
        assert_eq!(count(AlertView::All).await, 3);
        assert_eq!(count(AlertView::Active).await, 2);
        assert!(desk.alert(warning.id).await.unwrap().is_some());
        assert_eq!("critical".parse(), Ok(Severity::Critical));
        assert!("bad".parse::<Severity>().is_err());
    }
}
