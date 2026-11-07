//! Talk: conversations with the assistant, as threads of messages.
//!
//! An answer comes from an agent run, whose tool calls and approvals the apps
//! show from the run, or from the Second Brain when no talk agent is set up.

use crate::{Desk, DeskError, invalid, required_text};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_MESSAGE: usize = 20_000;
const THREAD_COLUMNS: &str = "id, title, pinned, created_at, updated_at";
const MESSAGE_COLUMNS: &str = "id, thread_id, role, text, status, run_id, sources, created_at";

/// A conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct TalkThread {
    /// Thread ID.
    pub id: Uuid,
    /// Title, from the first message unless the owner names it.
    pub title: String,
    /// Whether the owner pinned it.
    pub pinned: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last message.
    pub updated_at: DateTime<Utc>,
}

/// A message of a thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct TalkMessage {
    /// Message ID.
    pub id: Uuid,
    /// Its thread.
    pub thread_id: Uuid,
    /// `owner` or `assistant`.
    pub role: String,
    /// The text. Empty while an answer is pending.
    pub text: String,
    /// `done`, `pending` or `failed`.
    pub status: String,
    /// The run that answers it, with its tool calls and approvals.
    pub run_id: Option<Uuid>,
    /// Brain entries the answer used.
    pub sources: Value,
    /// When it was written.
    pub created_at: DateTime<Utc>,
}

impl Desk {
    /// Starts a thread.
    pub async fn create_thread(&self, title: &str) -> Result<TalkThread, DeskError> {
        let title = required_text("title", title, 200)?;
        let query = format!(
            "INSERT INTO talk_threads (id, tenant_id, title) VALUES ($1, $2, $3) RETURNING {THREAD_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(title)
            .fetch_one(self.pool())
            .await?)
    }

    /// Lists threads: pinned ones first, then by last message.
    pub async fn threads(&self, limit: i64) -> Result<Vec<TalkThread>, DeskError> {
        let query = format!(
            "SELECT {THREAD_COLUMNS} FROM talk_threads WHERE tenant_id = $1 \
             ORDER BY pinned DESC, updated_at DESC LIMIT $2"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(limit)
            .fetch_all(self.pool())
            .await?)
    }

    /// Returns a thread.
    pub async fn thread(&self, id: Uuid) -> Result<Option<TalkThread>, DeskError> {
        let query =
            format!("SELECT {THREAD_COLUMNS} FROM talk_threads WHERE tenant_id = $1 AND id = $2");
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Renames or pins a thread.
    pub async fn update_thread(
        &self,
        id: Uuid,
        title: Option<&str>,
        pinned: Option<bool>,
    ) -> Result<Option<TalkThread>, DeskError> {
        let title = title
            .map(|title| required_text("title", title, 200))
            .transpose()?;
        let query = format!(
            "UPDATE talk_threads SET title = coalesce($3, title), pinned = coalesce($4, pinned) \
             WHERE tenant_id = $1 AND id = $2 RETURNING {THREAD_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(title)
            .bind(pinned)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Deletes a thread with its messages. Returns whether it existed.
    pub async fn delete_thread(&self, id: Uuid) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM talk_threads WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns the messages of a thread, oldest first.
    pub async fn messages(&self, thread: Uuid) -> Result<Vec<TalkMessage>, DeskError> {
        let query = format!(
            "SELECT {MESSAGE_COLUMNS} FROM talk_messages WHERE tenant_id = $1 AND thread_id = $2 \
             ORDER BY created_at, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(thread)
            .fetch_all(self.pool())
            .await?)
    }

    /// Adds a message to a thread.
    pub async fn add_message(
        &self,
        thread: Uuid,
        role: &str,
        text: &str,
        status: &str,
        run_id: Option<Uuid>,
        sources: Value,
    ) -> Result<TalkMessage, DeskError> {
        if role == "owner" && text.trim().is_empty() {
            return Err(invalid("the message is empty"));
        }
        if text.chars().count() > MAX_MESSAGE {
            return Err(invalid(format!(
                "a message has at most {MAX_MESSAGE} characters"
            )));
        }
        let mut transaction = self.pool().begin().await?;
        let query = format!(
            "INSERT INTO talk_messages (id, tenant_id, thread_id, role, text, status, run_id, sources) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {MESSAGE_COLUMNS}"
        );
        let message: TalkMessage = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(thread)
            .bind(role)
            .bind(text)
            .bind(status)
            .bind(run_id)
            .bind(sources)
            .fetch_one(&mut *transaction)
            .await?;
        sqlx::query("UPDATE talk_threads SET updated_at = now() WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(thread)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(message)
    }

    /// Fills in the pending answer of a run, once the run ended.
    pub async fn finish_answer(
        &self,
        run: Uuid,
        succeeded: bool,
        text: &str,
    ) -> Result<Option<TalkMessage>, DeskError> {
        let query = format!(
            "UPDATE talk_messages SET text = $3, status = $4 WHERE tenant_id = $1 AND run_id = $2 \
             AND status = 'pending' RETURNING {MESSAGE_COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(run)
            .bind(text)
            .bind(if succeeded { "done" } else { "failed" })
            .fetch_optional(self.pool())
            .await?)
    }
}

/// Writes the latest messages of a thread as context for the next answer.
pub fn transcript(messages: &[TalkMessage], latest: usize) -> String {
    let start = messages.len().saturating_sub(latest);
    messages[start..]
        .iter()
        .filter(|message| !message.text.is_empty())
        .map(|message| {
            let speaker = if message.role == "owner" {
                "Owner"
            } else {
                "Lighthouse"
            };
            format!("{speaker}: {}", message.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    #[tokio::test]
    async fn threads_keep_messages_and_pending_answers() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        let thread = desk.create_thread("Release plan").await.unwrap();
        assert!(desk.create_thread(" ").await.is_err());
        desk.add_message(thread.id, "owner", "What is open?", "done", None, json!([]))
            .await
            .unwrap();
        assert!(
            desk.add_message(thread.id, "owner", " ", "done", None, json!([]))
                .await
                .is_err()
        );
        let run = Uuid::now_v7();
        desk.add_message(thread.id, "assistant", "", "pending", Some(run), json!([]))
            .await
            .unwrap();
        let answered = desk
            .finish_answer(run, true, "Two issues")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answered.status, "done");
        assert!(
            desk.finish_answer(run, true, "again")
                .await
                .unwrap()
                .is_none()
        );
        let messages = desk.messages(thread.id).await.unwrap();
        assert_eq!(
            transcript(&messages, 10),
            "Owner: What is open?\n\nLighthouse: Two issues"
        );
        assert_eq!(transcript(&messages, 1), "Lighthouse: Two issues");
        let pinned = desk
            .update_thread(thread.id, Some("Plan"), Some(true))
            .await
            .unwrap()
            .unwrap();
        assert!(pinned.pinned);
        desk.create_thread("Other").await.unwrap();
        assert_eq!(
            desk.threads(10).await.unwrap()[0].id,
            thread.id,
            "pinned first"
        );
        assert!(desk.thread(thread.id).await.unwrap().is_some());
        assert!(desk.delete_thread(thread.id).await.unwrap());
        assert!(!desk.delete_thread(thread.id).await.unwrap());
        assert!(
            desk.update_thread(thread.id, None, None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
