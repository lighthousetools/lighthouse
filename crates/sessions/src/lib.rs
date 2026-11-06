//! Desktop agents and the Claude Code sessions they watch.
//!
//! A desktop agent reports its sessions and forwards permission requests. Its
//! device token identifies the node, so one machine cannot touch another's
//! requests. The first decision wins, whichever device it comes from.

use chrono::{DateTime, Duration, Utc};
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Claude Code waits at most ten minutes for an HTTP hook.
const REQUEST_LIFETIME_MINUTES: i64 = 10;
/// A node that was silent for this long counts as offline.
const ONLINE_SECONDS: i64 = 90;

const REQUEST_COLUMNS: &str =
    "id, node_id, session_id, project, tool, input, status, created_at, expires_at, decided_at";

/// Errors from session storage.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// A session as a desktop agent reports it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionReport {
    /// Session ID from Claude Code.
    pub id: String,
    /// Project name.
    pub project: String,
    /// Git branch.
    pub branch: Option<String>,
    /// `running`, `waiting` or `done`.
    pub status: String,
    /// Latest status text.
    pub summary: Option<String>,
    /// Costs so far in EUR.
    #[serde(default)]
    pub cost_eur: f64,
}

/// What a desktop agent sends regularly.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct NodeReport {
    /// Machine name.
    pub name: String,
    /// Current sessions.
    pub sessions: Vec<SessionReport>,
}

/// The answer to a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NodeState {
    /// Whether remote actions are paused.
    pub paused: bool,
}

/// A machine with a desktop agent.
#[derive(Clone, Debug, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Node {
    /// Node ID, the same as its device ID.
    pub id: Uuid,
    /// Machine name.
    pub name: String,
    /// Whether remote actions are paused.
    pub paused: bool,
    /// Whether it reported recently.
    pub online: bool,
    /// Last report.
    pub last_seen_at: DateTime<Utc>,
}

/// A session with its machine.
#[derive(Clone, Debug, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Session {
    /// Session ID.
    pub id: String,
    /// The machine.
    pub node_id: Uuid,
    /// Machine name.
    pub node_name: String,
    /// Project name.
    pub project: String,
    /// Git branch.
    pub branch: Option<String>,
    /// `running`, `waiting` or `done`.
    pub status: String,
    /// Latest status text.
    pub summary: Option<String>,
    /// Costs so far in EUR.
    pub cost_eur: f64,
    /// First report.
    pub started_at: DateTime<Utc>,
    /// Latest report.
    pub updated_at: DateTime<Utc>,
}

/// A permission request from a session.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct NewPermission {
    /// Session ID.
    pub session_id: String,
    /// Project name.
    pub project: String,
    /// Tool name.
    pub tool: String,
    /// Tool arguments.
    pub input: Value,
}

/// A stored permission request.
#[derive(Clone, Debug, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Permission {
    /// Request ID.
    pub id: Uuid,
    /// The machine.
    pub node_id: Uuid,
    /// Session ID.
    pub session_id: String,
    /// Project name.
    pub project: String,
    /// Tool name.
    pub tool: String,
    /// Tool arguments.
    pub input: Value,
    /// `pending`, `allowed`, `denied` or `expired`.
    pub status: String,
    /// When it was asked.
    pub created_at: DateTime<Utc>,
    /// When it lapses.
    pub expires_at: DateTime<Utc>,
    /// When it was decided.
    pub decided_at: Option<DateTime<Utc>>,
}

/// Storage of nodes, sessions and permission requests.
#[derive(Clone, Debug)]
pub struct Sessions {
    store: Store,
    tenant: TenantId,
}

impl Sessions {
    /// Creates the storage for a tenant.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self { store, tenant }
    }

    /// Stores a report of a node and returns its state.
    pub async fn report(&self, node: Uuid, report: &NodeReport) -> Result<NodeState, SessionError> {
        let mut transaction = self.store.pool().begin().await?;
        let (paused,): (bool,) = sqlx::query_as(
            "INSERT INTO nodes (id, tenant_id, name) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET name = $3, last_seen_at = now() RETURNING paused",
        )
        .bind(node)
        .bind(self.tenant.as_uuid())
        .bind(&report.name)
        .fetch_one(&mut *transaction)
        .await?;
        for session in &report.sessions {
            sqlx::query(
                "INSERT INTO sessions (id, tenant_id, node_id, project, branch, status, summary, \
                 cost_eur) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (node_id, id) DO UPDATE SET project = $4, branch = $5, status = $6, \
                 summary = $7, cost_eur = $8, updated_at = now()",
            )
            .bind(&session.id)
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(&session.project)
            .bind(&session.branch)
            .bind(&session.status)
            .bind(&session.summary)
            .bind(session.cost_eur)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(NodeState { paused })
    }

    /// Lists nodes by name.
    pub async fn nodes(&self) -> Result<Vec<Node>, SessionError> {
        Ok(sqlx::query_as(
            "SELECT id, name, paused, last_seen_at, \
             last_seen_at > now() - make_interval(secs => $2) AS online \
             FROM nodes WHERE tenant_id = $1 ORDER BY name",
        )
        .bind(self.tenant.as_uuid())
        .bind(ONLINE_SECONDS as f64)
        .fetch_all(self.store.pool())
        .await?)
    }

    /// Lists sessions, latest first.
    pub async fn sessions(&self, limit: i64) -> Result<Vec<Session>, SessionError> {
        Ok(sqlx::query_as(
            "SELECT s.id, s.node_id, n.name AS node_name, s.project, s.branch, s.status, \
             s.summary, s.cost_eur, s.started_at, s.updated_at \
             FROM sessions s JOIN nodes n ON n.id = s.node_id \
             WHERE s.tenant_id = $1 ORDER BY s.updated_at DESC LIMIT $2",
        )
        .bind(self.tenant.as_uuid())
        .bind(limit)
        .fetch_all(self.store.pool())
        .await?)
    }

    /// Pauses a node or lets it continue. Pausing denies its open requests,
    /// so a kill switch leaves nothing waiting. Returns whether the node exists.
    pub async fn set_paused(&self, node: Uuid, paused: bool) -> Result<bool, SessionError> {
        let mut transaction = self.store.pool().begin().await?;
        let result = sqlx::query("UPDATE nodes SET paused = $1 WHERE id = $2 AND tenant_id = $3")
            .bind(paused)
            .bind(node)
            .bind(self.tenant.as_uuid())
            .execute(&mut *transaction)
            .await?;
        if paused {
            sqlx::query(
                "UPDATE permission_requests SET status = 'denied', decided_at = now() \
                 WHERE node_id = $1 AND status = 'pending'",
            )
            .bind(node)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Stores a request. A paused node gets a denied one right away.
    pub async fn request(
        &self,
        node: Uuid,
        new: &NewPermission,
    ) -> Result<Permission, SessionError> {
        let expires = Utc::now() + Duration::minutes(REQUEST_LIFETIME_MINUTES);
        let query = format!(
            "INSERT INTO permission_requests (id, tenant_id, node_id, session_id, project, tool, \
             input, status, expires_at, decided_at) \
             SELECT $1, $2, $3, $4, $5, $6, $7, \
             CASE WHEN n.paused THEN 'denied' ELSE 'pending' END, $8, \
             CASE WHEN n.paused THEN now() END FROM nodes n WHERE n.id = $3 \
             RETURNING {REQUEST_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(&new.session_id)
            .bind(&new.project)
            .bind(&new.tool)
            .bind(&new.input)
            .bind(expires)
            .fetch_one(self.store.pool())
            .await?)
    }

    /// Returns a request of a node, marking it expired if it lapsed.
    pub async fn node_request(
        &self,
        node: Uuid,
        id: Uuid,
    ) -> Result<Option<Permission>, SessionError> {
        self.expire().await?;
        let query = format!(
            "SELECT {REQUEST_COLUMNS} FROM permission_requests WHERE id = $1 AND node_id = $2"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(id)
            .bind(node)
            .fetch_optional(self.store.pool())
            .await?)
    }

    async fn expire(&self) -> Result<(), SessionError> {
        sqlx::query(
            "UPDATE permission_requests SET status = 'expired', decided_at = now() \
             WHERE tenant_id = $1 AND status = 'pending' AND expires_at <= now()",
        )
        .bind(self.tenant.as_uuid())
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Lists requests, newest first.
    pub async fn requests(
        &self,
        status: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Permission>, SessionError> {
        self.expire().await?;
        let query = format!(
            "SELECT {REQUEST_COLUMNS} FROM permission_requests WHERE tenant_id = $1 \
             AND ($2::text IS NULL OR status = $2) ORDER BY created_at DESC LIMIT $3"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(status)
            .bind(limit)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Decides an open request. The first decision wins.
    pub async fn decide(&self, id: Uuid, allow: bool) -> Result<Option<Permission>, SessionError> {
        let query = format!(
            "UPDATE permission_requests SET status = $1, decided_at = now() \
             WHERE id = $2 AND tenant_id = $3 AND status = 'pending' AND expires_at > now() \
             RETURNING {REQUEST_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(if allow { "allowed" } else { "denied" })
            .bind(id)
            .bind(self.tenant.as_uuid())
            .fetch_optional(self.store.pool())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    fn report(status: &str) -> NodeReport {
        NodeReport {
            name: "mac".to_owned(),
            sessions: vec![SessionReport {
                id: "s1".to_owned(),
                project: "billing".to_owned(),
                branch: Some("main".to_owned()),
                status: status.to_owned(),
                summary: None,
                cost_eur: 0.5,
            }],
        }
    }

    fn permission() -> NewPermission {
        NewPermission {
            session_id: "s1".to_owned(),
            project: "billing".to_owned(),
            tool: "Bash".to_owned(),
            input: json!({"command": "cargo test"}),
        }
    }

    async fn sessions() -> (Sessions, Uuid) {
        let sessions = Sessions::new(fresh_store().await, TenantId::SINGLE);
        let node = Uuid::now_v7();
        sessions.report(node, &report("running")).await.unwrap();
        (sessions, node)
    }

    #[tokio::test]
    async fn reports_update_sessions_in_place() {
        let (sessions, node) = sessions().await;
        sessions.report(node, &report("waiting")).await.unwrap();
        let list = sessions.sessions(10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, "waiting");
        assert_eq!(list[0].node_name, "mac");
        let nodes = sessions.nodes().await.unwrap();
        assert!(nodes[0].online);
    }

    #[tokio::test]
    async fn the_first_decision_wins_and_the_node_sees_it() {
        let (sessions, node) = sessions().await;
        let request = sessions.request(node, &permission()).await.unwrap();
        assert_eq!(request.status, "pending");
        assert_eq!(
            sessions.requests(Some("pending"), 5).await.unwrap().len(),
            1
        );
        let decided = sessions.decide(request.id, true).await.unwrap().unwrap();
        assert_eq!(decided.status, "allowed");
        assert!(sessions.decide(request.id, false).await.unwrap().is_none());
        let seen = sessions.node_request(node, request.id).await.unwrap();
        assert_eq!(seen.unwrap().status, "allowed");
    }

    #[tokio::test]
    async fn a_node_cannot_read_requests_of_another() {
        let (sessions, node) = sessions().await;
        let request = sessions.request(node, &permission()).await.unwrap();
        let other = sessions.node_request(Uuid::now_v7(), request.id).await;
        assert!(other.unwrap().is_none());
    }

    #[tokio::test]
    async fn pausing_denies_open_and_new_requests() {
        let (sessions, node) = sessions().await;
        let open = sessions.request(node, &permission()).await.unwrap();
        assert!(sessions.set_paused(node, true).await.unwrap());
        let seen = sessions.node_request(node, open.id).await.unwrap().unwrap();
        assert_eq!(seen.status, "denied");
        let new = sessions.request(node, &permission()).await.unwrap();
        assert_eq!(new.status, "denied");
        let state = sessions.report(node, &report("running")).await.unwrap();
        assert!(state.paused);
        assert!(sessions.set_paused(node, false).await.unwrap());
        let new = sessions.request(node, &permission()).await.unwrap();
        assert_eq!(new.status, "pending");
        assert!(!sessions.set_paused(Uuid::now_v7(), true).await.unwrap());
    }

    #[tokio::test]
    async fn lapsed_requests_expire_and_cannot_be_allowed() {
        let (sessions, node) = sessions().await;
        let request = sessions.request(node, &permission()).await.unwrap();
        sqlx::query("UPDATE permission_requests SET expires_at = now() - interval '1 minute'")
            .execute(sessions.store.pool())
            .await
            .unwrap();
        assert!(sessions.decide(request.id, true).await.unwrap().is_none());
        let seen = sessions
            .node_request(node, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(seen.status, "expired");
    }
}
