//! Sessions the server asks a desktop agent to start or resume.
//!
//! A launch waits as `queued` until its node takes it (`started`). The node
//! then reports `running` with the session ID and ends it as `succeeded`,
//! `failed` or `cancelled`. Only the node a launch belongs to can take or
//! report it, and a user sees only launches on their own nodes.

use crate::{SessionError, Sessions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// The longest prompt or result kept, in characters.
pub const MAX_LAUNCH_TEXT: usize = 20_000;
/// A node takes at most this many launches at once.
pub const MAX_TAKEN: i64 = 3;

const LAUNCH_COLUMNS: &str = "id, node_id, cwd, prompt, resume, model, status, cancel_requested, \
     session_id, result, cost_usd, created_at, updated_at, finished_at";

/// A session to start or resume.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct NewLaunch {
    /// The machine that runs it: its ID, or its name. A name that several machines share picks the one seen last.
    pub node: String,
    /// Working directory on that machine.
    pub cwd: String,
    /// The instruction.
    pub prompt: String,
    /// Session ID to continue instead of starting a new session.
    #[serde(default)]
    pub resume: Option<String>,
    /// Model, if not the default of Claude Code.
    #[serde(default)]
    pub model: Option<String>,
}

/// A stored launch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Launch {
    /// Launch ID.
    pub id: Uuid,
    /// The machine.
    pub node_id: Uuid,
    /// Working directory.
    pub cwd: String,
    /// The instruction.
    pub prompt: String,
    /// Session it continues.
    pub resume: Option<String>,
    /// Model.
    pub model: Option<String>,
    /// `queued`, `started`, `running`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    /// Whether someone asked to stop it.
    pub cancel_requested: bool,
    /// The Claude Code session, once known.
    pub session_id: Option<String>,
    /// The final answer or the error.
    pub result: Option<String>,
    /// Costs Claude Code reported, in USD.
    pub cost_usd: Option<f64>,
    /// When it was asked for.
    pub created_at: DateTime<Utc>,
    /// Latest change.
    pub updated_at: DateTime<Utc>,
    /// When it ended.
    pub finished_at: Option<DateTime<Utc>>,
}

/// What a node reports about a launch.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LaunchReport {
    /// `running`, `succeeded`, `failed` or `cancelled`.
    pub status: String,
    /// The Claude Code session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The final answer or the error.
    #[serde(default)]
    pub result: Option<String>,
    /// Costs in USD.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// Launches handed to a node, and running ones it should stop.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct LaunchQueue {
    /// Launches to start now.
    pub launches: Vec<Launch>,
    /// IDs of running launches to stop.
    pub cancel: Vec<Uuid>,
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_LAUNCH_TEXT).collect()
}

fn terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled")
}

impl Sessions {
    /// Queues a launch on a node the viewer may see. `None` for an unknown node.
    pub async fn launch(&self, new: &NewLaunch) -> Result<Option<Launch>, SessionError> {
        let query = format!(
            "INSERT INTO session_launches (id, tenant_id, node_id, cwd, prompt, resume, model, status) \
             SELECT $1, $2, n.id, $4, $5, $6, $7, 'queued' FROM nodes n \
             WHERE (n.id::text = $3 OR n.name = $3) AND n.tenant_id = $2 AND {} \
             ORDER BY (n.id::text = $3) DESC, n.last_seen_at DESC LIMIT 1 RETURNING {LAUNCH_COLUMNS}",
            Self::own_node(8)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(new.node.trim())
            .bind(new.cwd.trim())
            .bind(clip(new.prompt.trim()))
            .bind(new.resume.as_deref().filter(|id| !id.is_empty()))
            .bind(new.model.as_deref().filter(|model| !model.is_empty()))
            .bind(self.viewer)
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// Lists launches, newest first, optionally of one node.
    pub async fn launches(
        &self,
        node: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Launch>, SessionError> {
        let query = format!(
            "SELECT {LAUNCH_COLUMNS} FROM session_launches WHERE tenant_id = $1 \
             AND ($2::uuid IS NULL OR node_id = $2) \
             AND node_id IN (SELECT n.id FROM nodes n WHERE n.tenant_id = $1 AND {}) \
             ORDER BY created_at DESC LIMIT $3",
            Self::own_node(4)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(limit)
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Returns one launch the viewer may see.
    pub async fn launch_by_id(&self, id: Uuid) -> Result<Option<Launch>, SessionError> {
        let query = format!(
            "SELECT {LAUNCH_COLUMNS} FROM session_launches WHERE tenant_id = $1 AND id = $2 \
             AND node_id IN (SELECT n.id FROM nodes n WHERE n.tenant_id = $1 AND {})",
            Self::own_node(3)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(id)
            .bind(self.viewer)
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// Cancels a launch: a queued one at once, a started or running one once
    /// its node stops it. `None` if it is unknown or already over.
    pub async fn cancel_launch(&self, id: Uuid) -> Result<Option<Launch>, SessionError> {
        let query = format!(
            "UPDATE session_launches SET \
             status = CASE WHEN status = 'queued' THEN 'cancelled' ELSE status END, \
             finished_at = CASE WHEN status = 'queued' THEN now() ELSE finished_at END, \
             cancel_requested = true, updated_at = now() \
             WHERE tenant_id = $1 AND id = $2 AND status IN ('queued', 'started', 'running') \
             AND node_id IN (SELECT n.id FROM nodes n WHERE n.tenant_id = $1 AND {}) \
             RETURNING {LAUNCH_COLUMNS}",
            Self::own_node(3)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(id)
            .bind(self.viewer)
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// Hands queued launches to their node, oldest first, and names the
    /// running ones it should stop.
    pub async fn take_launches(&self, node: Uuid) -> Result<LaunchQueue, SessionError> {
        let query = format!(
            "UPDATE session_launches SET status = 'started', updated_at = now() WHERE id IN \
             (SELECT id FROM session_launches WHERE tenant_id = $1 AND node_id = $2 \
              AND status = 'queued' ORDER BY created_at LIMIT $3 FOR UPDATE SKIP LOCKED) \
             RETURNING {LAUNCH_COLUMNS}"
        );
        let mut launches: Vec<Launch> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(MAX_TAKEN)
            .fetch_all(self.store.pool())
            .await?;
        launches.sort_by_key(|launch| (launch.created_at, launch.id));
        let cancel = sqlx::query_scalar(
            "SELECT id FROM session_launches WHERE tenant_id = $1 AND node_id = $2 \
             AND cancel_requested AND status IN ('started', 'running')",
        )
        .bind(self.tenant.as_uuid())
        .bind(node)
        .fetch_all(self.store.pool())
        .await?;
        Ok(LaunchQueue { launches, cancel })
    }

    /// Stores what a node reports about one of its launches. An ended launch
    /// stays as it is. `None` if the launch is not this node's or is over.
    pub async fn report_launch(
        &self,
        node: Uuid,
        id: Uuid,
        report: &LaunchReport,
    ) -> Result<Option<Launch>, SessionError> {
        let status = match report.status.as_str() {
            "running" | "succeeded" | "failed" | "cancelled" => report.status.as_str(),
            _ => return Ok(None),
        };
        let query = format!(
            "UPDATE session_launches SET status = $4, \
             session_id = COALESCE($5, session_id), result = COALESCE($6, result), \
             cost_usd = COALESCE($7, cost_usd), updated_at = now(), \
             finished_at = CASE WHEN $8 THEN now() ELSE finished_at END \
             WHERE tenant_id = $1 AND node_id = $2 AND id = $3 \
             AND status IN ('started', 'running') RETURNING {LAUNCH_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(id)
            .bind(status)
            .bind(report.session_id.as_deref())
            .bind(report.result.as_deref().map(clip))
            .bind(report.cost_usd)
            .bind(terminal(status))
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// The latest launch that ran a session, so a message can resume it.
    pub async fn launch_of_session(
        &self,
        node: Uuid,
        session: &str,
    ) -> Result<Option<Launch>, SessionError> {
        let query = format!(
            "SELECT {LAUNCH_COLUMNS} FROM session_launches WHERE tenant_id = $1 AND node_id = $2 \
             AND session_id = $3 AND node_id IN \
             (SELECT n.id FROM nodes n WHERE n.tenant_id = $1 AND {}) \
             ORDER BY created_at DESC LIMIT 1",
            Self::own_node(4)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(session)
            .bind(self.viewer)
            .fetch_optional(self.store.pool())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeReport, SessionReport};
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    async fn node(sessions: &Sessions) -> Uuid {
        let node = Uuid::now_v7();
        let report = NodeReport {
            name: "mac".to_owned(),
            sessions: vec![SessionReport {
                id: "s0".to_owned(),
                project: "app".to_owned(),
                branch: None,
                status: "done".to_owned(),
                summary: None,
                cost_eur: 0.0,
            }],
        };
        sessions.report(node, &report).await.unwrap();
        node
    }

    fn new(node: Uuid) -> NewLaunch {
        NewLaunch {
            node: node.to_string(),
            cwd: "/work/app".to_owned(),
            prompt: "Run the tests".to_owned(),
            resume: None,
            model: Some(String::new()),
        }
    }

    fn report(status: &str) -> LaunchReport {
        LaunchReport {
            status: status.to_owned(),
            session_id: Some("s1".to_owned()),
            result: Some("done".to_owned()),
            cost_usd: Some(0.2),
        }
    }

    #[tokio::test]
    async fn a_launch_goes_from_queued_to_its_result() {
        let sessions = Sessions::new(fresh_store().await, TenantId::DEFAULT);
        let node = node(&sessions).await;
        let launch = sessions.launch(&new(node)).await.unwrap().unwrap();
        assert_eq!(launch.status, "queued");
        assert_eq!(launch.model, None);
        assert!(
            sessions
                .launch(&new(Uuid::now_v7()))
                .await
                .unwrap()
                .is_none()
        );

        let queue = sessions.take_launches(node).await.unwrap();
        assert_eq!(queue.launches.len(), 1);
        assert_eq!(queue.launches[0].status, "started");
        assert!(
            sessions
                .take_launches(node)
                .await
                .unwrap()
                .launches
                .is_empty()
        );
        assert!(
            sessions
                .report_launch(Uuid::now_v7(), launch.id, &report("running"))
                .await
                .unwrap()
                .is_none(),
            "another node cannot report it"
        );
        let running = sessions
            .report_launch(node, launch.id, &report("running"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(running.session_id.as_deref(), Some("s1"));
        assert!(running.finished_at.is_none());
        let done = sessions
            .report_launch(node, launch.id, &report("succeeded"))
            .await
            .unwrap()
            .unwrap();
        assert!(done.finished_at.is_some());
        assert!(
            sessions
                .report_launch(node, launch.id, &report("failed"))
                .await
                .unwrap()
                .is_none(),
            "an ended launch stays"
        );
        assert!(
            sessions
                .report_launch(node, launch.id, &report("bogus"))
                .await
                .unwrap()
                .is_none()
        );
        let found = sessions
            .launch_of_session(node, "s1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, launch.id);
        assert_eq!(sessions.launches(Some(node), 10).await.unwrap().len(), 1);
        assert_eq!(
            sessions.launch_by_id(launch.id).await.unwrap().unwrap(),
            done
        );
    }

    #[tokio::test]
    async fn cancelling_stops_queued_launches_at_once_and_running_ones_through_the_node() {
        let sessions = Sessions::new(fresh_store().await, TenantId::DEFAULT);
        let node = node(&sessions).await;
        let queued = sessions.launch(&new(node)).await.unwrap().unwrap();
        let cancelled = sessions.cancel_launch(queued.id).await.unwrap().unwrap();
        assert_eq!(cancelled.status, "cancelled");
        assert!(sessions.cancel_launch(queued.id).await.unwrap().is_none());

        let running = sessions.launch(&new(node)).await.unwrap().unwrap();
        sessions.take_launches(node).await.unwrap();
        let asked = sessions.cancel_launch(running.id).await.unwrap().unwrap();
        assert_eq!(asked.status, "started");
        let queue = sessions.take_launches(node).await.unwrap();
        assert_eq!(queue.cancel, vec![running.id]);
        sessions
            .report_launch(node, running.id, &report("cancelled"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            sessions
                .take_launches(node)
                .await
                .unwrap()
                .cancel
                .is_empty()
        );
    }
}
