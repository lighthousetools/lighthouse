//! Approvals for tools with external effect, and the saved state of paused runs.

use crate::{AuditError, AuditLog, RunStatus};
use chrono::{DateTime, Duration, Utc};
use lighthouse_core::RunId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Hours until an open approval counts as denied.
const EXPIRY_HOURS: i64 = 72;

const COLUMNS: &str = "id, run_id, agent_id, tool, input, quotes_external, status, created_at, \
                       expires_at, decided_at";

/// A proposed tool call.
#[derive(Clone, Debug, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Approval {
    /// Approval ID.
    pub id: Uuid,
    /// The paused run.
    pub run_id: Uuid,
    /// The agent that proposes the call.
    pub agent_id: String,
    /// Qualified tool name.
    pub tool: String,
    /// Tool arguments.
    pub input: Value,
    /// Whether the run had read external content, which may have shaped the proposal.
    pub quotes_external: bool,
    /// `pending`, `approved`, `denied` or `expired`.
    pub status: String,
    /// When it was proposed.
    pub created_at: DateTime<Utc>,
    /// When it counts as denied.
    pub expires_at: DateTime<Utc>,
    /// When it was decided.
    pub decided_at: Option<DateTime<Utc>>,
}

/// A tool call to propose.
#[derive(Clone, Debug)]
pub struct NewApproval {
    /// Approval ID.
    pub id: Uuid,
    /// Qualified tool name.
    pub tool: String,
    /// Tool arguments.
    pub input: Value,
    /// Whether the run had read external content.
    pub quotes_external: bool,
}

/// A saved run, taken by the one caller that resumes it.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    /// Agent of the run.
    pub agent_id: String,
    /// Saved loop state.
    pub state: Value,
}

impl AuditLog {
    /// Saves a run and its proposals, and marks the run as waiting.
    pub async fn pause_run(
        &self,
        run: RunId,
        agent_id: &str,
        state: &Value,
        proposals: &[NewApproval],
    ) -> Result<(), AuditError> {
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO run_checkpoints (run_id, tenant_id, agent_id, state) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(run.as_uuid())
        .bind(self.tenant.as_uuid())
        .bind(agent_id)
        .bind(state)
        .execute(&mut *transaction)
        .await?;
        let expires = Utc::now() + Duration::hours(EXPIRY_HOURS);
        for proposal in proposals {
            sqlx::query(
                "INSERT INTO approvals (id, tenant_id, run_id, agent_id, tool, input, \
                 quotes_external, status, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8)",
            )
            .bind(proposal.id)
            .bind(self.tenant.as_uuid())
            .bind(run.as_uuid())
            .bind(agent_id)
            .bind(&proposal.tool)
            .bind(&proposal.input)
            .bind(proposal.quotes_external)
            .bind(expires)
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query("UPDATE runs SET status = $1 WHERE id = $2")
            .bind(RunStatus::Waiting.as_str())
            .bind(run.as_uuid())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Lists approvals, newest first.
    pub async fn list_approvals(
        &self,
        status: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Approval>, AuditError> {
        let query = format!(
            "SELECT {COLUMNS} FROM approvals WHERE tenant_id = $1 \
             AND ($2::text IS NULL OR status = $2) ORDER BY created_at DESC LIMIT $3"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(status)
            .bind(limit)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Returns the approvals of a run, oldest first.
    pub async fn run_approvals(&self, run: Uuid) -> Result<Vec<Approval>, AuditError> {
        let query = format!(
            "SELECT {COLUMNS} FROM approvals WHERE tenant_id = $1 AND run_id = $2 \
             ORDER BY created_at, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(run)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Decides an open approval. The first decision wins, so a later one from
    /// another device returns `None`. An edit replaces the proposed arguments.
    pub async fn decide_approval(
        &self,
        id: Uuid,
        approve: bool,
        edited_input: Option<&Value>,
    ) -> Result<Option<Approval>, AuditError> {
        let query = format!(
            "UPDATE approvals SET status = $1, decided_at = now(), \
             input = COALESCE($2, input) \
             WHERE id = $3 AND tenant_id = $4 AND status = 'pending' AND expires_at > now() \
             RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(if approve { "approved" } else { "denied" })
            .bind(edited_input)
            .bind(id)
            .bind(self.tenant.as_uuid())
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// Marks overdue approvals as expired and returns the affected runs.
    pub async fn expire_approvals(&self) -> Result<Vec<Uuid>, AuditError> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "UPDATE approvals SET status = 'expired', decided_at = now() \
             WHERE tenant_id = $1 AND status = 'pending' AND expires_at <= now() \
             RETURNING run_id",
        )
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        let mut runs: Vec<Uuid> = rows.into_iter().map(|row| row.0).collect();
        runs.sort_unstable();
        runs.dedup();
        Ok(runs)
    }

    /// Lists paused runs that wait for no decision anymore.
    pub async fn resumable_runs(&self) -> Result<Vec<Uuid>, AuditError> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT run_id FROM run_checkpoints checkpoint WHERE tenant_id = $1 \
             AND NOT EXISTS (SELECT 1 FROM approvals \
                             WHERE run_id = checkpoint.run_id AND status = 'pending')",
        )
        .bind(self.tenant.as_uuid())
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }

    /// Takes the checkpoint of a run whose approvals are all decided.
    ///
    /// Taking deletes the row, so exactly one caller resumes the run.
    pub async fn take_checkpoint(&self, run: Uuid) -> Result<Option<Checkpoint>, AuditError> {
        let row: Option<(String, Value)> = sqlx::query_as(
            "DELETE FROM run_checkpoints WHERE run_id = $1 AND tenant_id = $2 \
             AND NOT EXISTS (SELECT 1 FROM approvals \
                             WHERE run_id = $1 AND status = 'pending') \
             RETURNING agent_id, state",
        )
        .bind(run)
        .bind(self.tenant.as_uuid())
        .fetch_optional(self.store.pool())
        .await?;
        if row.is_some() {
            sqlx::query("UPDATE runs SET status = $1 WHERE id = $2")
                .bind(RunStatus::Running.as_str())
                .bind(run)
                .execute(self.store.pool())
                .await?;
        }
        Ok(row.map(|(agent_id, state)| Checkpoint { agent_id, state }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    async fn paused(ids: &[Uuid]) -> (AuditLog, RunId) {
        let log = AuditLog::new(fresh_store().await, TenantId::SINGLE);
        let run = RunId::new();
        log.start_run(run, "mailer", "manual").await.unwrap();
        let proposals: Vec<_> = ids
            .iter()
            .map(|id| NewApproval {
                id: *id,
                tool: "mail.send".to_owned(),
                input: json!({"to": "a"}),
                quotes_external: true,
            })
            .collect();
        log.pause_run(run, "mailer", &json!({"step": 1}), &proposals)
            .await
            .unwrap();
        (log, run)
    }

    #[tokio::test]
    async fn pausing_marks_the_run_and_lists_the_proposal() {
        let id = Uuid::now_v7();
        let (log, run) = paused(&[id]).await;
        let detail = log.run_detail(run.as_uuid()).await.unwrap().unwrap();
        assert_eq!(detail.run.status, "waiting");
        assert!(detail.run.finished_at.is_none());
        let open = log.list_approvals(Some("pending"), 10).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, id);
        assert!(open[0].quotes_external);
        assert!(
            log.list_approvals(Some("denied"), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_first_decision_wins() {
        let id = Uuid::now_v7();
        let (log, _) = paused(&[id]).await;
        let first = log.decide_approval(id, false, None).await.unwrap().unwrap();
        assert_eq!(first.status, "denied");
        assert!(log.decide_approval(id, true, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_edit_replaces_the_arguments() {
        let id = Uuid::now_v7();
        let (log, _) = paused(&[id]).await;
        let edited = json!({"to": "b"});
        let approval = log
            .decide_approval(id, true, Some(&edited))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approval.input, edited);
    }

    #[tokio::test]
    async fn a_run_resumes_only_after_every_decision_and_only_once() {
        let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
        let (log, run) = paused(&[first, second]).await;
        log.decide_approval(first, true, None).await.unwrap();
        assert!(log.take_checkpoint(run.as_uuid()).await.unwrap().is_none());
        log.decide_approval(second, true, None).await.unwrap();
        let checkpoint = log.take_checkpoint(run.as_uuid()).await.unwrap().unwrap();
        assert_eq!(checkpoint.agent_id, "mailer");
        assert_eq!(checkpoint.state, json!({"step": 1}));
        assert!(log.take_checkpoint(run.as_uuid()).await.unwrap().is_none());
        let detail = log.run_detail(run.as_uuid()).await.unwrap().unwrap();
        assert_eq!(detail.run.status, "running");
        assert_eq!(log.run_approvals(run.as_uuid()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_restart_fails_running_runs_but_keeps_paused_ones() {
        let (log, paused_run) = paused(&[Uuid::now_v7()]).await;
        let running = RunId::new();
        log.start_run(running, "brief", "manual").await.unwrap();
        assert_eq!(log.fail_interrupted_runs().await.unwrap(), 1);
        let detail = log.run_detail(running.as_uuid()).await.unwrap().unwrap();
        assert_eq!(detail.run.status, "failed");
        assert!(detail.run.finished_at.is_some());
        let waiting = log.run_detail(paused_run.as_uuid()).await.unwrap().unwrap();
        assert_eq!(waiting.run.status, "waiting");
    }

    #[tokio::test]
    async fn overdue_approvals_expire_and_cannot_be_decided() {
        let id = Uuid::now_v7();
        let (log, run) = paused(&[id]).await;
        sqlx::query("UPDATE approvals SET expires_at = now() - interval '1 hour'")
            .execute(log.store.pool())
            .await
            .unwrap();
        assert!(log.decide_approval(id, true, None).await.unwrap().is_none());
        assert_eq!(log.expire_approvals().await.unwrap(), vec![run.as_uuid()]);
        assert!(log.expire_approvals().await.unwrap().is_empty());
        assert_eq!(log.resumable_runs().await.unwrap(), vec![run.as_uuid()]);
        let approvals = log.run_approvals(run.as_uuid()).await.unwrap();
        assert_eq!(approvals[0].status, "expired");
    }
}
