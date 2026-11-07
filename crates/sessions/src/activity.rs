//! What a session did, and messages from the owner to a session.

use crate::{SessionError, Sessions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Most events stored per report.
pub const MAX_EVENTS: usize = 100;
const MAX_MESSAGE: usize = 10_000;
const EVENT_COLUMNS: &str = "id, session_id, kind, tool, summary, file_path, detail, created_at";
const MESSAGE_COLUMNS: &str = "id, session_id, text, status, created_at, delivered_at";

/// One thing a session did, as a desktop agent reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct NewSessionEvent {
    /// Session ID from Claude Code.
    pub session_id: String,
    /// `tool`, `file`, `todos`, `plan`, `prompt` or `stop`.
    pub kind: String,
    /// Tool name.
    #[serde(default)]
    pub tool: Option<String>,
    /// One line, such as a command or a file path.
    #[serde(default)]
    pub summary: Option<String>,
    /// Changed file.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Kind-specific data: `todos`, `plan` or `diff`.
    #[serde(default)]
    pub detail: Value,
    /// When it happened. Defaults to the time of the report.
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

/// A stored event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct SessionEvent {
    /// Event ID.
    pub id: Uuid,
    /// Session ID.
    pub session_id: String,
    /// Kind.
    pub kind: String,
    /// Tool name.
    pub tool: Option<String>,
    /// One line.
    pub summary: Option<String>,
    /// Changed file.
    pub file_path: Option<String>,
    /// Kind-specific data.
    pub detail: Value,
    /// When it happened.
    pub created_at: DateTime<Utc>,
}

/// A message from the owner to a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct SessionMessage {
    /// Message ID.
    pub id: Uuid,
    /// Session ID.
    pub session_id: String,
    /// The text, which the session gets as its next instruction.
    pub text: String,
    /// `pending` or `delivered`.
    pub status: String,
    /// When it was sent.
    pub created_at: DateTime<Utc>,
    /// When the session got it.
    pub delivered_at: Option<DateTime<Utc>>,
}

/// A changed file with its latest change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChangedFile {
    /// File path.
    pub path: String,
    /// Number of edits.
    pub edits: i64,
    /// The latest edit as a diff, if the hook had one.
    pub diff: Option<String>,
    /// Time of the latest edit.
    pub changed_at: DateTime<Utc>,
}

impl Sessions {
    /// Stores events of a node. At most [`MAX_EVENTS`] per call.
    pub async fn record_events(
        &self,
        node: Uuid,
        events: &[NewSessionEvent],
    ) -> Result<usize, SessionError> {
        let mut transaction = self.store.pool().begin().await?;
        let mut stored = 0;
        for event in events.iter().take(MAX_EVENTS) {
            sqlx::query(
                "INSERT INTO session_events (id, tenant_id, node_id, session_id, kind, tool, summary, \
                 file_path, detail, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                 coalesce($10, now()))",
            )
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(&event.session_id)
            .bind(&event.kind)
            .bind(&event.tool)
            .bind(&event.summary)
            .bind(&event.file_path)
            .bind(&event.detail)
            .bind(event.at)
            .execute(&mut *transaction)
            .await?;
            stored += 1;
        }
        transaction.commit().await?;
        Ok(stored)
    }

    /// Returns the events of a session, oldest first, optionally of one kind.
    pub async fn events(
        &self,
        node: Uuid,
        session: &str,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let query = format!(
            "SELECT {EVENT_COLUMNS} FROM (SELECT {EVENT_COLUMNS} FROM session_events \
             WHERE tenant_id = $1 AND node_id = $2 AND session_id = $3 \
             AND ($4::text IS NULL OR kind = $4) ORDER BY created_at DESC, id DESC LIMIT $5) latest \
             ORDER BY created_at, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(session)
            .bind(kind)
            .bind(limit)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Returns the latest event of a kind, such as the current todo list.
    pub async fn latest_event(
        &self,
        node: Uuid,
        session: &str,
        kind: &str,
    ) -> Result<Option<SessionEvent>, SessionError> {
        Ok(self.events(node, session, Some(kind), 1).await?.pop())
    }

    /// Returns the files a session changed, latest change first.
    pub async fn changed_files(
        &self,
        node: Uuid,
        session: &str,
    ) -> Result<Vec<ChangedFile>, SessionError> {
        let rows: Vec<(String, i64, Option<String>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT file_path, count(*), (array_agg(detail->>'diff' ORDER BY created_at DESC))[1], \
             max(created_at) FROM session_events WHERE tenant_id = $1 AND node_id = $2 \
             AND session_id = $3 AND kind = 'file' AND file_path IS NOT NULL \
             GROUP BY file_path ORDER BY max(created_at) DESC",
        )
        .bind(self.tenant.as_uuid())
        .bind(node)
        .bind(session)
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(path, edits, diff, changed_at)| ChangedFile {
                path,
                edits,
                diff,
                changed_at,
            })
            .collect())
    }

    /// Queues a message for a session. Returns `None` for an unknown session.
    pub async fn send_message(
        &self,
        node: Uuid,
        session: &str,
        text: &str,
    ) -> Result<Option<SessionMessage>, SessionError> {
        let text: String = text.trim().chars().take(MAX_MESSAGE).collect();
        let query = format!(
            "INSERT INTO session_messages (id, tenant_id, node_id, session_id, text, status) \
             SELECT $1, $2, $3, $4, $5, 'pending' WHERE EXISTS \
             (SELECT 1 FROM sessions WHERE tenant_id = $2 AND node_id = $3 AND id = $4) \
             RETURNING {MESSAGE_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(session)
            .bind(text)
            .fetch_optional(self.store.pool())
            .await?)
    }

    /// Lists the messages of a session, oldest first.
    pub async fn messages(
        &self,
        node: Uuid,
        session: &str,
    ) -> Result<Vec<SessionMessage>, SessionError> {
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM session_messages WHERE tenant_id = $1 AND node_id = $2 \
             AND session_id = $3 ORDER BY created_at, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(session)
            .fetch_all(self.store.pool())
            .await?)
    }

    /// Hands the pending messages of a session over, once.
    pub async fn take_messages(
        &self,
        node: Uuid,
        session: &str,
    ) -> Result<Vec<SessionMessage>, SessionError> {
        let query = format!(
            "UPDATE session_messages SET status = 'delivered', delivered_at = now() \
             WHERE tenant_id = $1 AND node_id = $2 AND session_id = $3 AND status = 'pending' \
             RETURNING {MESSAGE_COLUMNS}"
        );
        let mut messages: Vec<SessionMessage> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(node)
            .bind(session)
            .fetch_all(self.store.pool())
            .await?;
        messages.sort_by_key(|message| (message.created_at, message.id));
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeReport, SessionReport};
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    fn event(kind: &str, file: Option<&str>, detail: Value) -> NewSessionEvent {
        NewSessionEvent {
            session_id: "s1".to_owned(),
            kind: kind.to_owned(),
            tool: Some("Edit".to_owned()),
            summary: None,
            file_path: file.map(str::to_owned),
            detail,
            at: None,
        }
    }

    #[tokio::test]
    async fn events_files_todos_and_messages_of_a_session() {
        let sessions = Sessions::new(fresh_store().await, TenantId::SINGLE);
        let node = Uuid::now_v7();
        let report = NodeReport {
            name: "laptop".to_owned(),
            sessions: vec![SessionReport {
                id: "s1".to_owned(),
                project: "app".to_owned(),
                branch: None,
                status: "waiting".to_owned(),
                summary: None,
                cost_eur: 0.0,
            }],
        };
        sessions.report(node, &report).await.unwrap();
        let events = [
            event(
                "todos",
                None,
                json!({"todos": [{"content": "Write tests", "status": "pending"}]}),
            ),
            event("file", Some("src/a.rs"), json!({"diff": "-a\n+b"})),
            event("file", Some("src/a.rs"), json!({"diff": "-b\n+c"})),
            event("file", Some("src/b.rs"), json!({})),
        ];
        assert_eq!(sessions.record_events(node, &events).await.unwrap(), 4);
        assert_eq!(
            sessions.events(node, "s1", None, 10).await.unwrap().len(),
            4
        );
        assert_eq!(
            sessions.events(node, "s1", None, 2).await.unwrap()[1]
                .file_path
                .as_deref(),
            Some("src/b.rs")
        );
        let todos = sessions
            .latest_event(node, "s1", "todos")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(todos.detail["todos"][0]["content"], "Write tests");
        let files = sessions.changed_files(node, "s1").await.unwrap();
        let a = files.iter().find(|file| file.path == "src/a.rs").unwrap();
        assert_eq!((a.edits, a.diff.as_deref()), (2, Some("-b\n+c")));

        assert!(
            sessions
                .send_message(node, "nope", "x")
                .await
                .unwrap()
                .is_none()
        );
        sessions
            .send_message(node, "s1", " Now run the tests ")
            .await
            .unwrap()
            .unwrap();
        sessions
            .send_message(node, "s1", "Then commit")
            .await
            .unwrap()
            .unwrap();
        assert!(
            sessions
                .take_messages(Uuid::now_v7(), "s1")
                .await
                .unwrap()
                .is_empty()
        );
        let taken = sessions.take_messages(node, "s1").await.unwrap();
        let texts: Vec<_> = taken.iter().map(|message| message.text.as_str()).collect();
        assert_eq!(texts, ["Now run the tests", "Then commit"]);
        assert!(sessions.take_messages(node, "s1").await.unwrap().is_empty());
        assert_eq!(
            sessions.messages(node, "s1").await.unwrap()[0].status,
            "delivered"
        );
    }
}
