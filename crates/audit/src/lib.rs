//! Immutable log of runs, tool calls and costs.
//!
//! Steps are append-only, enforced by a database trigger. A run row only
//! changes when it pauses, resumes or finishes.

mod approvals;

pub use approvals::{Approval, Checkpoint, NewApproval};

use chrono::{DateTime, Utc};
use lighthouse_core::{RunId, TenantId};
use lighthouse_store::Store;
use serde::Serialize;
use sqlx::FromRow;
use std::str::FromStr;
use utoipa::ToSchema;
use uuid::Uuid;

/// Errors from the audit log.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// A database operation failed.
    #[error("audit log error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Outcome of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Still running.
    Running,
    /// Paused until the owner decides on its proposals.
    Waiting,
    /// Finished and delivered.
    Succeeded,
    /// An error stopped the run.
    Failed,
    /// The agent was not allowed to start.
    Rejected,
    /// A limit stopped the run.
    LimitExceeded,
}

impl RunStatus {
    /// Returns the stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::LimitExceeded => "limit_exceeded",
        }
    }
}

impl FromStr for RunStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        [
            Self::Running,
            Self::Waiting,
            Self::Succeeded,
            Self::Failed,
            Self::Rejected,
            Self::LimitExceeded,
        ]
        .into_iter()
        .find(|status| status.as_str() == value)
        .ok_or_else(|| format!("unknown run status `{value}`"))
    }
}

/// One step to record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewStep {
    /// `model`, `tool`, `skill` or `delivery`.
    pub kind: String,
    /// Model, tool, skill or target name.
    pub name: String,
    /// `ok`, `error` or `rejected`.
    pub status: String,
    /// Extra information. Must not contain secrets.
    pub detail: Option<String>,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in EUR.
    pub cost_eur: f64,
    /// Duration in milliseconds.
    pub duration_ms: u64,
}

/// A run as stored.
#[derive(Clone, Debug, FromRow, Serialize, ToSchema)]
pub struct RunRecord {
    /// Run ID.
    pub id: Uuid,
    /// Agent ID.
    pub agent_id: String,
    /// What started the run.
    pub trigger: String,
    /// Status name.
    pub status: String,
    /// Why the run did not succeed.
    pub reason: Option<String>,
    /// The final report.
    pub report: Option<String>,
    /// Total input tokens.
    pub input_tokens: i64,
    /// Total output tokens.
    pub output_tokens: i64,
    /// Total cost in EUR.
    pub cost_eur: f64,
    /// Name of the latest step, which says what a running agent is doing.
    pub activity: Option<String>,
    /// Start time.
    pub started_at: DateTime<Utc>,
    /// End time.
    pub finished_at: Option<DateTime<Utc>>,
}

/// A step as stored.
#[derive(Clone, Debug, FromRow, Serialize, ToSchema)]
pub struct StepRecord {
    /// Position in the run.
    pub seq: i32,
    /// Step kind.
    pub kind: String,
    /// Step name.
    pub name: String,
    /// Status.
    pub status: String,
    /// Extra information.
    pub detail: Option<String>,
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Cost in EUR.
    pub cost_eur: f64,
    /// Duration in milliseconds.
    pub duration_ms: i64,
    /// Time of the record.
    pub created_at: DateTime<Utc>,
}

/// A run with its steps.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RunDetail {
    /// The run.
    pub run: RunRecord,
    /// Its steps in order.
    pub steps: Vec<StepRecord>,
}

/// A successful tool call, as listed for review.
#[derive(Clone, Debug, FromRow, Serialize, ToSchema)]
pub struct ToolCall {
    /// Step ID.
    pub id: Uuid,
    /// The run.
    pub run_id: Uuid,
    /// The agent of the run.
    pub agent_id: String,
    /// Qualified tool name.
    pub tool: String,
    /// Tool arguments as JSON text.
    pub detail: Option<String>,
    /// When it ran.
    pub created_at: DateTime<Utc>,
}

/// How a run ended.
#[derive(Clone, Debug)]
pub struct RunEnd<'a> {
    /// Final status.
    pub status: RunStatus,
    /// Why it did not succeed.
    pub reason: Option<&'a str>,
    /// The report.
    pub report: Option<&'a str>,
}

/// Run costs of one user.
#[derive(Clone, Debug, Serialize, FromRow, ToSchema)]
pub struct UserCost {
    /// The user the runs worked for.
    pub user_id: Option<Uuid>,
    /// Number of runs.
    pub runs: i64,
    /// Costs in EUR.
    pub cost_eur: f64,
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
}

/// Model costs of one day and provider.
#[derive(Clone, Debug, Serialize, FromRow, ToSchema)]
pub struct DailyCost {
    /// The day, as `YYYY-MM-DD` in UTC.
    pub day: String,
    /// Provider name.
    pub provider: String,
    /// Costs in EUR.
    pub cost_eur: f64,
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
}

/// Writes and reads the audit log of one tenant.
#[derive(Clone, Debug)]
pub struct AuditLog {
    store: Store,
    tenant: TenantId,
    viewer: Option<Uuid>,
}

/// The user a request acts for, so runs it starts are recorded for them even
/// when they are registered deep inside the runtime.
pub mod acting_user {
    use std::future::Future;
    use uuid::Uuid;

    tokio::task_local! {
        static USER: Option<Uuid>;
    }

    /// Runs a future on behalf of a user.
    pub async fn scope<F: Future>(user: Option<Uuid>, future: F) -> F::Output {
        USER.scope(user, future).await
    }

    /// The user the current task acts for, if any.
    pub fn current() -> Option<Uuid> {
        USER.try_with(|user| *user).ok().flatten()
    }
}

/// Runs the viewer, given as parameter `$n`, may see: their own and those of
/// nobody in particular. Without a viewer, every run.
pub(crate) fn own_run(parameter: u8) -> String {
    format!("(${parameter}::uuid IS NULL OR user_id IS NULL OR user_id = ${parameter})")
}

/// Approvals of runs the viewer may see.
pub(crate) fn own_approval(parameter: u8) -> String {
    format!(
        "(${parameter}::uuid IS NULL OR run_id IN (SELECT r.id FROM runs r WHERE r.tenant_id = approvals.tenant_id \
         AND (r.user_id IS NULL OR r.user_id = ${parameter})))"
    )
}

const RUN_COLUMNS: &str = "id, agent_id, trigger, status, reason, report, input_tokens, output_tokens, \
                           cost_eur, started_at, finished_at, \
                           (SELECT name FROM run_steps s WHERE s.run_id = runs.id \
                            ORDER BY seq DESC LIMIT 1) AS activity";

impl AuditLog {
    /// Creates the log.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self {
            store,
            tenant,
            viewer: None,
        }
    }

    /// The same log as seen by one user. Runs it starts belong to that user.
    pub fn for_user(&self, user: Option<Uuid>) -> Self {
        Self {
            viewer: user,
            ..self.clone()
        }
    }

    /// The tenant of the log.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    pub(crate) fn viewer(&self) -> Option<Uuid> {
        self.viewer
    }

    /// Records the start of a run.
    pub async fn start_run(
        &self,
        id: RunId,
        agent_id: &str,
        trigger: &str,
    ) -> Result<(), AuditError> {
        // A run belongs to the user who started it. Runs from schedules and
        // automations belong to the person agents work for in the tenant.
        sqlx::query(
            "INSERT INTO runs (id, tenant_id, agent_id, trigger, status, user_id) \
             VALUES ($1, $2, $3, $4, $5, coalesce($6, tenant_primary_user($2)))",
        )
        .bind(id.as_uuid())
        .bind(self.tenant.as_uuid())
        .bind(agent_id)
        .bind(trigger)
        .bind(RunStatus::Running.as_str())
        .bind(self.viewer.or_else(acting_user::current))
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Appends a step and adds its tokens and cost to the run.
    pub async fn record_step(&self, run: RunId, step: &NewStep) -> Result<(), AuditError> {
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO run_steps (id, tenant_id, run_id, seq, kind, name, status, detail, input_tokens,
                                    output_tokens, cost_eur, duration_ms)
             SELECT $1, $2, $3, COALESCE(MAX(seq), 0) + 1, $4, $5, $6, $7, $8, $9, $10, $11
             FROM run_steps WHERE run_id = $3",
        )
        .bind(Uuid::now_v7())
        .bind(self.tenant.as_uuid())
        .bind(run.as_uuid())
        .bind(&step.kind)
        .bind(&step.name)
        .bind(&step.status)
        .bind(&step.detail)
        .bind(step.input_tokens as i64)
        .bind(step.output_tokens as i64)
        .bind(step.cost_eur)
        .bind(step.duration_ms as i64)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE runs SET input_tokens = input_tokens + $1, output_tokens = output_tokens + $2,
                             cost_eur = cost_eur + $3 WHERE id = $4 AND tenant_id = $5",
        )
        .bind(step.input_tokens as i64)
        .bind(step.output_tokens as i64)
        .bind(step.cost_eur)
        .bind(run.as_uuid())
        .bind(self.tenant.as_uuid())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Records the end of a run.
    pub async fn finish_run(&self, run: RunId, end: RunEnd<'_>) -> Result<(), AuditError> {
        sqlx::query(
            "UPDATE runs SET status = $1, reason = $2, report = $3, finished_at = now() \
             WHERE id = $4 AND tenant_id = $5",
        )
        .bind(end.status.as_str())
        .bind(end.reason)
        .bind(end.report)
        .bind(run.as_uuid())
        .bind(self.tenant.as_uuid())
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Sums the model costs per day and provider, latest day first.
    pub async fn daily_costs(&self, days: i32) -> Result<Vec<DailyCost>, AuditError> {
        Ok(sqlx::query_as(
            "SELECT (created_at AT TIME ZONE 'UTC')::date::text AS day, \
             COALESCE(substring(detail FROM 'provider=([^ ]+)'), 'unknown') AS provider, \
             SUM(cost_eur) AS cost_eur, SUM(input_tokens)::bigint AS input_tokens, \
             SUM(output_tokens)::bigint AS output_tokens \
             FROM run_steps WHERE tenant_id = $1 AND kind = 'model' \
             AND created_at > now() - make_interval(days => $2) \
             GROUP BY 1, 2 ORDER BY 1 DESC, 2",
        )
        .bind(self.tenant.as_uuid())
        .bind(days)
        .fetch_all(self.store.pool())
        .await?)
    }

    /// Sums the cost of runs per user over the last days, highest first. Runs
    /// of nobody in particular have no user.
    pub async fn costs_by_user(&self, days: i32) -> Result<Vec<UserCost>, AuditError> {
        Ok(sqlx::query_as(
            "SELECT user_id, count(*) AS runs, SUM(cost_eur) AS cost_eur, \
             SUM(input_tokens)::bigint AS input_tokens, SUM(output_tokens)::bigint AS output_tokens \
             FROM runs WHERE tenant_id = $1 AND started_at > now() - make_interval(days => $2) \
             GROUP BY user_id ORDER BY 3 DESC, 1",
        )
        .bind(self.tenant.as_uuid())
        .bind(days)
        .fetch_all(self.store.pool())
        .await?)
    }

    /// The cost of all model calls since a time, in EUR.
    pub async fn cost_since(&self, since: DateTime<Utc>) -> Result<f64, AuditError> {
        let row: (Option<f64>,) = sqlx::query_as(
            "SELECT SUM(cost_eur) FROM run_steps WHERE tenant_id = $1 AND kind = 'model' \
             AND created_at >= $2",
        )
        .bind(self.tenant.as_uuid())
        .bind(since)
        .fetch_one(self.store.pool())
        .await?;
        Ok(row.0.unwrap_or_default())
    }

    /// Lists successful calls of the given tools since a time, newest first.
    pub async fn tool_calls(
        &self,
        tools: &[String],
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ToolCall>, AuditError> {
        let query = format!(
            "SELECT s.id, s.run_id, r.agent_id, s.name AS tool, s.detail, s.created_at \
             FROM run_steps s JOIN runs r ON r.id = s.run_id \
             WHERE s.tenant_id = $1 AND s.kind = 'tool' AND s.status = 'ok' \
             AND s.name = ANY($2) AND s.created_at >= $3 AND {} \
             ORDER BY s.created_at DESC LIMIT $4",
            own_run(5).replace("user_id", "r.user_id")
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(tools)
            .bind(since)
            .bind(limit)
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Counts runs, failed runs and their cost per day of the time zone, oldest
    /// first, with a row for every day. `agent` narrows to one agent, `reason`
    /// to runs whose reason contains the text.
    pub async fn daily_runs(
        &self,
        agent: Option<&str>,
        reason: Option<&str>,
        days: i32,
        zone: &str,
    ) -> Result<Vec<(String, i64, i64, f64)>, AuditError> {
        Ok(sqlx::query_as(
            "SELECT to_char(day, 'YYYY-MM-DD'), count(r.id), \
             count(r.id) FILTER (WHERE r.status IN ('failed', 'limit_exceeded', 'rejected')), \
             coalesce(sum(r.cost_eur), 0)::float8 \
             FROM generate_series(date_trunc('day', now() AT TIME ZONE $5) - make_interval(days => $4 - 1), \
                                  date_trunc('day', now() AT TIME ZONE $5), interval '1 day') AS day \
             LEFT JOIN runs r ON r.tenant_id = $1 AND (r.started_at AT TIME ZONE $5) >= day \
                  AND (r.started_at AT TIME ZONE $5) < day + interval '1 day' \
                  AND ($2::text IS NULL OR r.agent_id = $2) \
                  AND ($3::text IS NULL OR r.reason LIKE '%' || $3 || '%') \
             GROUP BY day ORDER BY day",
        )
        .bind(self.tenant.as_uuid())
        .bind(agent)
        .bind(reason)
        .bind(days)
        .bind(zone)
        .fetch_all(self.store.pool())
        .await?)
    }

    /// Returns a value that changes whenever anything the apps show changes.
    pub async fn revision(&self) -> Result<String, AuditError> {
        let (revision,): (String,) = sqlx::query_as(
            "SELECT concat_ws('/', \
             (SELECT count(*) FROM run_steps WHERE tenant_id = $1), \
             (SELECT count(*) || max(finished_at)::text FROM runs WHERE tenant_id = $1), \
             (SELECT count(*) FROM approvals WHERE tenant_id = $1 AND status = 'pending'), \
             (SELECT count(*) FROM permission_requests WHERE tenant_id = $1 AND status = 'pending'), \
             (SELECT max(updated_at)::text FROM sessions WHERE tenant_id = $1), \
             (SELECT count(*) || max(updated_at)::text FROM brain_entries WHERE tenant_id = $1), \
             (SELECT count(*) || max(updated_at)::text FROM tasks WHERE tenant_id = $1), \
             (SELECT count(*) || max(updated_at)::text FROM rules WHERE tenant_id = $1), \
             (SELECT count(*) || max(updated_at)::text FROM facts WHERE tenant_id = $1), \
             (SELECT count(*)::text || sum(count)::text || \
              max(greatest(last_seen_at, acknowledged_at, resolved_at))::text \
              FROM alerts WHERE tenant_id = $1), \
             (SELECT count(*) FROM ready_dismissals WHERE tenant_id = $1), \
             (SELECT count(*) FROM run_documents WHERE tenant_id = $1), \
             (SELECT count(*) FROM session_events WHERE tenant_id = $1), \
             (SELECT count(*) || max(delivered_at)::text FROM session_messages WHERE tenant_id = $1), \
             (SELECT count(*)::text || max(synced_at)::text FROM synced_items WHERE tenant_id = $1), \
             (SELECT count(*)::text || max(updated_at)::text FROM organizations WHERE tenant_id = $1), \
             (SELECT count(*)::text || max(updated_at)::text FROM posts WHERE tenant_id = $1))",
        )
        .bind(self.tenant.as_uuid())
        .fetch_one(self.store.pool())
        .await?;
        Ok(revision)
    }

    /// Fails runs a stopped server left behind. Paused runs keep waiting.
    ///
    /// Only call this at start, while nothing can be running.
    pub async fn fail_interrupted_runs(&self) -> Result<u64, AuditError> {
        let result = sqlx::query(
            "UPDATE runs SET status = $1, reason = 'the server stopped during this run', \
             finished_at = now() WHERE tenant_id = $2 AND status = $3",
        )
        .bind(RunStatus::Failed.as_str())
        .bind(self.tenant.as_uuid())
        .bind(RunStatus::Running.as_str())
        .execute(self.store.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Lists the latest runs of an agent.
    pub async fn list_agent_runs(
        &self,
        agent: &str,
        limit: i64,
    ) -> Result<Vec<RunRecord>, AuditError> {
        let query = format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE tenant_id = $1 AND agent_id = $2 AND {} \
             ORDER BY started_at DESC LIMIT $3",
            own_run(4)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(agent)
            .bind(limit)
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Lists the latest runs.
    pub async fn list_runs(&self, limit: i64) -> Result<Vec<RunRecord>, AuditError> {
        let query = format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE tenant_id = $1 AND {} \
             ORDER BY started_at DESC LIMIT $2",
            own_run(3)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(limit)
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Returns a run with its steps.
    pub async fn run_detail(&self, id: Uuid) -> Result<Option<RunDetail>, AuditError> {
        let query = format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE tenant_id = $1 AND id = $2 AND {}",
            own_run(3)
        );
        let run: Option<RunRecord> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(id)
            .bind(self.viewer)
            .fetch_optional(self.store.pool())
            .await?;
        let Some(run) = run else { return Ok(None) };
        let steps = sqlx::query_as(
            "SELECT seq, kind, name, status, detail, input_tokens, output_tokens, cost_eur, duration_ms, created_at
             FROM run_steps WHERE run_id = $1 AND tenant_id = $2 ORDER BY seq",
        )
        .bind(id)
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        Ok(Some(RunDetail { run, steps }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_store::testing::fresh_store;

    async fn log() -> (AuditLog, Store) {
        let store = fresh_store().await;
        (AuditLog::new(store.clone(), TenantId::DEFAULT), store)
    }

    fn step(kind: &str, tokens: u64, cost: f64) -> NewStep {
        NewStep {
            kind: kind.to_owned(),
            name: "x".to_owned(),
            status: "ok".to_owned(),
            input_tokens: tokens,
            output_tokens: tokens * 2,
            cost_eur: cost,
            ..NewStep::default()
        }
    }

    async fn member(store: &Store, email: &str, role: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, 'u')")
            .bind(id)
            .bind(email)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(TenantId::DEFAULT.as_uuid())
            .bind(id)
            .bind(role)
            .execute(store.pool())
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn runs_and_their_approvals_belong_to_the_user_they_ran_for() {
        let (log, store) = log().await;
        let owner = member(&store, "alex@example.com", "owner").await;
        let sam = member(&store, "sam@example.com", "member").await;
        let as_owner = log.for_user(Some(owner));
        let as_sam = log.for_user(Some(sam));

        let scheduled = RunId::new();
        log.start_run(scheduled, "brief", "cron").await.unwrap();
        let manual = RunId::new();
        acting_user::scope(Some(sam), log.start_run(manual, "brief", "manual"))
            .await
            .unwrap();
        let approval = crate::NewApproval {
            id: Uuid::now_v7(),
            tool: "mail.send".to_owned(),
            input: serde_json::json!({}),
            quotes_external: false,
            reason: None,
            consulted: Vec::new(),
        };
        log.pause_run(
            scheduled,
            "brief",
            &serde_json::json!({}),
            std::slice::from_ref(&approval),
        )
        .await
        .unwrap();

        assert_eq!(
            as_owner.list_runs(10).await.unwrap().len(),
            1,
            "schedules work for the owner"
        );
        assert_eq!(as_sam.list_runs(10).await.unwrap()[0].id, manual.as_uuid());
        assert_eq!(log.list_runs(10).await.unwrap().len(), 2);
        assert!(
            as_sam
                .run_detail(scheduled.as_uuid())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            as_owner
                .run_detail(scheduled.as_uuid())
                .await
                .unwrap()
                .is_some()
        );
        assert!(as_sam.list_agent_runs("brief", 10).await.unwrap().len() == 1);
        assert!(as_sam.list_approvals(None, 10).await.unwrap().is_empty());
        assert!(
            as_sam
                .run_approvals(scheduled.as_uuid())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            as_sam
                .edit_approval(approval.id, &serde_json::json!({"x": 1}))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            as_sam
                .decide_approval(approval.id, true, None)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(as_owner.list_approvals(None, 10).await.unwrap().len(), 1);
        assert!(
            as_owner
                .decide_approval(approval.id, true, None)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            as_sam
                .tool_calls(
                    &["mail.send".to_owned()],
                    Utc::now() - chrono::Duration::days(1),
                    10
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(acting_user::current(), None);
    }

    #[tokio::test]
    async fn records_a_run_with_steps_and_totals() {
        let (log, _) = log().await;
        let run = RunId::new();
        log.start_run(run, "morning-brief", "manual").await.unwrap();
        log.record_step(run, &step("model", 10, 0.5)).await.unwrap();
        log.record_step(run, &step("tool", 0, 0.0)).await.unwrap();
        log.record_step(run, &step("model", 5, 0.25)).await.unwrap();
        let end = RunEnd {
            status: RunStatus::Succeeded,
            reason: None,
            report: Some("Report"),
        };
        log.finish_run(run, end).await.unwrap();

        let detail = log.run_detail(run.as_uuid()).await.unwrap().unwrap();
        assert_eq!(detail.run.status, "succeeded");
        assert_eq!(detail.run.report.as_deref(), Some("Report"));
        assert_eq!(detail.run.input_tokens, 15);
        assert_eq!(detail.run.output_tokens, 30);
        assert!((detail.run.cost_eur - 0.75).abs() < 1e-9);
        assert!(detail.run.finished_at.is_some());
        let order: Vec<_> = detail
            .steps
            .iter()
            .map(|step| (step.seq, step.kind.as_str()))
            .collect();
        assert_eq!(order, [(1, "model"), (2, "tool"), (3, "model")]);
    }

    #[tokio::test]
    async fn lists_newest_first_and_honors_the_limit() {
        let (log, _) = log().await;
        for agent in ["first", "second", "third"] {
            log.start_run(RunId::new(), agent, "cron").await.unwrap();
        }
        let runs = log.list_runs(2).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].agent_id, "third");
    }

    #[tokio::test]
    async fn unknown_run_is_none() {
        let (log, _) = log().await;
        assert!(log.run_detail(Uuid::nil()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn steps_cannot_be_changed_or_deleted() {
        let (log, store) = log().await;
        let run = RunId::new();
        log.start_run(run, "agent", "manual").await.unwrap();
        log.record_step(run, &step("tool", 0, 0.0)).await.unwrap();
        for statement in [
            "UPDATE run_steps SET status = 'forged'",
            "DELETE FROM run_steps",
        ] {
            let error = sqlx::query(statement)
                .execute(store.pool())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("append-only"), "{error}");
        }
    }

    #[tokio::test]
    async fn tool_calls_are_listed_by_name_status_and_time() {
        let (log, _) = log().await;
        let run = RunId::new();
        log.start_run(run, "inbox", "manual").await.unwrap();
        let mut saved = step("tool", 0, 0.0);
        saved.name = "mail.save_draft".to_owned();
        saved.detail = Some("{}".to_owned());
        log.record_step(run, &saved).await.unwrap();
        let mut failed = saved.clone();
        failed.status = "error".to_owned();
        log.record_step(run, &failed).await.unwrap();
        log.record_step(run, &step("tool", 0, 0.0)).await.unwrap();
        let tools = ["mail.save_draft".to_owned()];
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let calls = log.tool_calls(&tools, since, 10).await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].agent_id, "inbox");
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        assert!(log.tool_calls(&tools, later, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn daily_runs_have_a_row_for_every_day() {
        let (log, _) = log().await;
        for (agent, status) in [
            ("brief", RunStatus::Succeeded),
            ("brief", RunStatus::Failed),
            ("other", RunStatus::Failed),
        ] {
            let run = RunId::new();
            log.start_run(run, agent, "cron").await.unwrap();
            log.record_step(run, &step("model", 1, 0.5)).await.unwrap();
            let end = RunEnd {
                status,
                reason: Some("connector `linear` failed"),
                report: None,
            };
            log.finish_run(run, end).await.unwrap();
        }
        let days = log
            .daily_runs(Some("brief"), None, 7, "Europe/Berlin")
            .await
            .unwrap();
        assert_eq!(days.len(), 7);
        let today = days.last().unwrap();
        assert_eq!((today.1, today.2), (2, 1));
        assert!((today.3 - 1.0).abs() < 1e-9);
        let connector = log
            .daily_runs(None, Some("linear"), 3, "UTC")
            .await
            .unwrap();
        assert_eq!(connector.last().unwrap().1, 3);
        assert_eq!(days[0].1, 0);
    }

    #[tokio::test]
    async fn the_revision_follows_the_desk() {
        let (log, store) = log().await;
        let mut seen = vec![log.revision().await.unwrap()];
        for statement in [
            "INSERT INTO tasks (id, tenant_id, title, status, position, origin) \
             VALUES (gen_random_uuid(), '00000000-0000-0000-0000-000000000000', 'Task', 'open', 0, 'owner')",
            "INSERT INTO rules (id, tenant_id, text) VALUES (gen_random_uuid(), '00000000-0000-0000-0000-000000000000', 'Rule')",
            "INSERT INTO facts (id, tenant_id, text, status, confidence, source_kind) \
             VALUES (gen_random_uuid(), '00000000-0000-0000-0000-000000000000', 'Fact', 'confirmed', 1, 'owner')",
            "INSERT INTO alerts (id, tenant_id, kind, key, severity, title, status) \
             VALUES (gen_random_uuid(), '00000000-0000-0000-0000-000000000000', 'run_failed', 'k', 'warning', 'T', 'open')",
            "UPDATE alerts SET status = 'resolved', resolved_at = now() + interval '1 second'",
        ] {
            sqlx::query(statement).execute(store.pool()).await.unwrap();
            let revision = log.revision().await.unwrap();
            assert!(!seen.contains(&revision), "{statement}");
            seen.push(revision);
        }
    }

    #[test]
    fn status_names_round_trip() {
        assert_eq!("limit_exceeded".parse(), Ok(RunStatus::LimitExceeded));
        assert!("nope".parse::<RunStatus>().is_err());
    }
}
