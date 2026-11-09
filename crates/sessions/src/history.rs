//! What a desktop agent finds on its machine: Claude Code sessions that can be resumed, and skills.
//!
//! Only metadata arrives: a session's project, branch, first prompt, message count and last activity, and a skill's
//! name, description and path. A user sees only what their own machines reported.

use crate::{SessionError, Sessions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// The most sessions or skills taken in one report.
pub const MAX_REPORTED: usize = 2_000;
/// The longest first prompt kept.
pub const MAX_PROMPT: usize = 200;

/// A session found on a machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PastSession {
    /// Session ID from Claude Code.
    pub id: String,
    /// Project name.
    pub project: String,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Git branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// What the owner asked first, cut short.
    #[serde(default)]
    pub first_prompt: Option<String>,
    /// Number of messages.
    #[serde(default)]
    pub messages: i32,
    /// Last activity.
    pub last_active_at: DateTime<Utc>,
}

/// A stored past session, with the machine it is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct HistoryEntry {
    /// The machine.
    pub node_id: Uuid,
    /// Session ID from Claude Code.
    pub session_id: String,
    /// Project name.
    pub project: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Git branch.
    pub branch: Option<String>,
    /// What the owner asked first.
    pub first_prompt: Option<String>,
    /// Number of messages.
    pub messages: i32,
    /// Last activity.
    pub last_active_at: DateTime<Utc>,
}

/// A skill found on a machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LocalSkill {
    /// Its name.
    pub name: String,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Folder of the skill.
    pub path: String,
    /// `user` for the owner's own skills, `project` for those of a project.
    pub scope: String,
    /// The project folder, for project skills.
    #[serde(default)]
    pub project: Option<String>,
}

/// A stored skill, with the machine it is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct StoredSkill {
    /// The machine.
    pub node_id: Uuid,
    /// Its name.
    pub name: String,
    /// What it is for.
    pub description: Option<String>,
    /// Folder of the skill.
    pub path: String,
    /// `user` or `project`.
    pub scope: String,
    /// The project folder, for project skills.
    pub project: Option<String>,
}

fn short(text: Option<&str>, limit: usize) -> Option<String> {
    text.map(|text| text.trim().chars().take(limit).collect::<String>())
        .filter(|text| !text.is_empty())
}

impl Sessions {
    /// Stores the sessions a node found. Known sessions are updated.
    pub async fn record_history(
        &self,
        node: Uuid,
        sessions: &[PastSession],
    ) -> Result<(), SessionError> {
        let mut transaction = self.store.pool().begin().await?;
        for session in sessions.iter().take(MAX_REPORTED) {
            sqlx::query(
                "INSERT INTO session_history (node_id, session_id, tenant_id, project, cwd, branch, first_prompt, \
                 messages, last_active_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (node_id, session_id) DO UPDATE SET project = $4, cwd = $5, branch = $6, \
                 first_prompt = $7, messages = $8, last_active_at = $9, updated_at = now()",
            )
            .bind(node)
            .bind(session.id.chars().take(200).collect::<String>())
            .bind(self.tenant.as_uuid())
            .bind(session.project.chars().take(200).collect::<String>())
            .bind(short(session.cwd.as_deref(), 1_000))
            .bind(short(session.branch.as_deref(), 200))
            .bind(short(session.first_prompt.as_deref(), MAX_PROMPT))
            .bind(session.messages.max(0))
            .bind(session.last_active_at)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Lists past sessions, newest first. `query` matches project, branch and first prompt.
    pub async fn history(
        &self,
        node: Option<Uuid>,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HistoryEntry>, SessionError> {
        let pattern = query
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| format!("%{}%", text.replace(['%', '_'], "")));
        let sql = format!(
            "SELECT h.node_id, h.session_id, h.project, h.cwd, h.branch, h.first_prompt, h.messages, \
             h.last_active_at FROM session_history h JOIN nodes n ON n.id = h.node_id \
             WHERE h.tenant_id = $1 AND ($2::uuid IS NULL OR h.node_id = $2) \
             AND ($3::text IS NULL OR h.project ILIKE $3 OR h.first_prompt ILIKE $3 OR h.branch ILIKE $3) \
             AND {} ORDER BY h.last_active_at DESC LIMIT $4",
            Self::own_node(5)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(pattern)
            .bind(limit)
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Replaces the skills a node reported.
    pub async fn record_skills(
        &self,
        node: Uuid,
        skills: &[LocalSkill],
    ) -> Result<(), SessionError> {
        let mut transaction = self.store.pool().begin().await?;
        sqlx::query("DELETE FROM local_skills WHERE node_id = $1 AND tenant_id = $2")
            .bind(node)
            .bind(self.tenant.as_uuid())
            .execute(&mut *transaction)
            .await?;
        for skill in skills.iter().take(MAX_REPORTED) {
            let scope = if skill.scope == "project" {
                "project"
            } else {
                "user"
            };
            sqlx::query(
                "INSERT INTO local_skills (node_id, path, tenant_id, name, description, scope, project) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (node_id, path) DO NOTHING",
            )
            .bind(node)
            .bind(skill.path.chars().take(1_000).collect::<String>())
            .bind(self.tenant.as_uuid())
            .bind(skill.name.chars().take(200).collect::<String>())
            .bind(short(skill.description.as_deref(), 1_000))
            .bind(scope)
            .bind(short(skill.project.as_deref(), 1_000))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Lists the skills of the viewer's machines, by project and name.
    pub async fn skills(&self) -> Result<Vec<StoredSkill>, SessionError> {
        let sql = format!(
            "SELECT s.node_id, s.name, s.description, s.path, s.scope, s.project FROM local_skills s \
             JOIN nodes n ON n.id = s.node_id WHERE s.tenant_id = $1 AND {} \
             ORDER BY s.scope DESC, s.project NULLS FIRST, s.name",
            Self::own_node(2)
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.tenant.as_uuid())
            .bind(self.viewer)
            .fetch_all(self.store.pool())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeReport;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    async fn node(sessions: &Sessions) -> Uuid {
        let node = Uuid::now_v7();
        let report = NodeReport {
            name: "mac".to_owned(),
            sessions: Vec::new(),
        };
        sessions.report(node, &report).await.unwrap();
        node
    }

    fn past(id: &str, project: &str, prompt: &str, minutes: i64) -> PastSession {
        PastSession {
            id: id.to_owned(),
            project: project.to_owned(),
            cwd: Some(format!("/work/{project}")),
            branch: Some("main".to_owned()),
            first_prompt: Some(prompt.to_owned()),
            messages: 12,
            last_active_at: Utc::now() - chrono::Duration::minutes(minutes),
        }
    }

    #[tokio::test]
    async fn past_sessions_are_upserted_searched_and_newest_first() {
        let sessions = Sessions::new(fresh_store().await, TenantId::DEFAULT);
        let node = node(&sessions).await;
        sessions
            .record_history(
                node,
                &[
                    past("a", "billing", "Fix the invoice export", 30),
                    past("b", "atlas", "Ship search", 5),
                ],
            )
            .await
            .unwrap();
        let mut again = past("a", "billing", "Fix the invoice export", 1);
        again.messages = 40;
        sessions.record_history(node, &[again]).await.unwrap();
        let all = sessions.history(None, None, 10).await.unwrap();
        assert_eq!(
            all.iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(all[0].messages, 40);
        let found = sessions
            .history(Some(node), Some("search"), 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].project, "atlas");
        let other = sessions.for_user(Some(Uuid::now_v7()));
        assert!(
            other.history(None, None, 10).await.unwrap().is_empty(),
            "only the owner's machines"
        );
    }

    #[tokio::test]
    async fn skills_are_replaced_per_machine() {
        let sessions = Sessions::new(fresh_store().await, TenantId::DEFAULT);
        let node = node(&sessions).await;
        let skill = |name: &str, scope: &str| LocalSkill {
            name: name.to_owned(),
            description: Some(format!("Does {name}")),
            path: format!("/work/sales/.claude/skills/{name}"),
            scope: scope.to_owned(),
            project: (scope == "project").then(|| "/work/sales".to_owned()),
        };
        sessions
            .record_skills(
                node,
                &[skill("jumpstart", "project"), skill("notes", "user")],
            )
            .await
            .unwrap();
        assert_eq!(sessions.skills().await.unwrap().len(), 2);
        sessions
            .record_skills(node, &[skill("jumpstart", "project")])
            .await
            .unwrap();
        let skills = sessions.skills().await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].project.as_deref(), Some("/work/sales"));
    }
}
