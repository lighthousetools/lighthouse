//! The owner's to-do list.
//!
//! Tasks are short and personal. An agent can propose one, which waits as
//! `proposed` until the owner accepts or deletes it.

use crate::{Desk, DeskError, Origin, SourceLink, double_option, invalid, required_text};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::str::FromStr;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_TITLE: usize = 500;
const MAX_NOTES: usize = 20_000;
const MAX_PRIORITY: i16 = 3;

const COLUMNS: &str = "id, title, notes, due_on, priority, status, position, source_kind, source_ref, \
                       source_label, source_url, origin, agent_id, run_id, created_at, updated_at, done_at";

/// State of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Proposed by an agent, not yet accepted.
    Proposed,
    /// On the list.
    Open,
    /// Done.
    Done,
}

impl TaskStatus {
    /// Returns the stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Open => "open",
            Self::Done => "done",
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        [Self::Proposed, Self::Open, Self::Done]
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| format!("unknown task status `{value}`"))
    }
}

/// A task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Task {
    /// Task ID.
    pub id: Uuid,
    /// What to do.
    pub title: String,
    /// Free notes.
    pub notes: Option<String>,
    /// Due date in the owner's time zone.
    pub due_on: Option<NaiveDate>,
    /// 0 for none up to 3 for high.
    pub priority: i16,
    /// `proposed`, `open` or `done`.
    pub status: TaskStatus,
    /// Sort key, lowest first.
    pub position: f64,
    /// What the task came from.
    pub source: Option<SourceLink>,
    /// Who created it.
    pub origin: Origin,
    /// The proposing agent.
    pub agent_id: Option<String>,
    /// The proposing run.
    pub run_id: Option<Uuid>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// When it was done.
    pub done_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct TaskRow {
    id: Uuid,
    title: String,
    notes: Option<String>,
    due_on: Option<NaiveDate>,
    priority: i16,
    status: String,
    position: f64,
    source_kind: Option<String>,
    source_ref: Option<String>,
    source_label: Option<String>,
    source_url: Option<String>,
    origin: String,
    agent_id: Option<String>,
    run_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    done_at: Option<DateTime<Utc>>,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Self {
            id: row.id,
            title: row.title,
            notes: row.notes,
            due_on: row.due_on,
            priority: row.priority,
            status: row.status.parse().unwrap_or(TaskStatus::Open),
            position: row.position,
            source: SourceLink::from_columns(
                row.source_kind,
                row.source_ref,
                row.source_label,
                row.source_url,
            ),
            origin: if row.origin == "agent" {
                Origin::Agent
            } else {
                Origin::Owner
            },
            agent_id: row.agent_id,
            run_id: row.run_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
            done_at: row.done_at,
        }
    }
}

/// A task to create.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct NewTask {
    /// What to do.
    pub title: String,
    /// Free notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Due date.
    #[serde(default)]
    pub due_on: Option<NaiveDate>,
    /// 0 to 3.
    #[serde(default)]
    pub priority: i16,
    /// What it came from.
    #[serde(default)]
    pub source: Option<SourceLink>,
}

/// Changes to a task. Missing fields stay, `null` clears.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct TaskPatch {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New notes.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub notes: Option<Option<String>>,
    /// New due date.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<NaiveDate>)]
    pub due_on: Option<Option<NaiveDate>>,
    /// New priority.
    #[serde(default)]
    pub priority: Option<i16>,
    /// New source.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<SourceLink>)]
    pub source: Option<Option<SourceLink>>,
}

/// Which tasks to list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskView {
    /// Open and proposed tasks.
    #[default]
    All,
    /// Open tasks due today or overdue.
    Today,
    /// Open tasks due later or without a date.
    Upcoming,
    /// Done tasks, latest first.
    Done,
    /// Proposals of agents.
    Proposed,
}

fn check_priority(priority: i16) -> Result<i16, DeskError> {
    if (0..=MAX_PRIORITY).contains(&priority) {
        Ok(priority)
    } else {
        Err(invalid(format!("`priority` must be 0 to {MAX_PRIORITY}")))
    }
}

fn check_notes(notes: Option<String>) -> Result<Option<String>, DeskError> {
    let notes = notes
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    if notes
        .as_ref()
        .is_some_and(|text| text.chars().count() > MAX_NOTES)
    {
        return Err(invalid(format!(
            "`notes` is longer than {MAX_NOTES} characters"
        )));
    }
    Ok(notes)
}

/// Who proposes a task.
#[derive(Clone, Debug)]
pub struct Proposer {
    /// The agent.
    pub agent_id: String,
    /// The run.
    pub run_id: Uuid,
}

impl Desk {
    /// Creates a task at the top of the list. An agent's task is a proposal.
    pub async fn create_task(
        &self,
        task: NewTask,
        proposer: Option<Proposer>,
    ) -> Result<Task, DeskError> {
        let title = required_text("title", &task.title, MAX_TITLE)?;
        let notes = check_notes(task.notes)?;
        let priority = check_priority(task.priority)?;
        if let Some(source) = &task.source {
            source.validate()?;
        }
        let (status, origin) = match proposer {
            Some(_) => (TaskStatus::Proposed, Origin::Agent),
            None => (TaskStatus::Open, Origin::Owner),
        };
        let source = task.source.as_ref();
        let query = format!(
            "INSERT INTO tasks (id, tenant_id, title, notes, due_on, priority, status, position, \
             source_kind, source_ref, source_label, source_url, origin, agent_id, run_id) \
             SELECT $1, $2, $3, $4, $5, $6, $7, COALESCE(MIN(position), 0) - 1, $8, $9, $10, $11, \
             $12, $13, $14 FROM tasks WHERE tenant_id = $2 AND status <> 'done' \
             RETURNING {COLUMNS}"
        );
        let row: TaskRow = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(title)
            .bind(notes)
            .bind(task.due_on)
            .bind(priority)
            .bind(status.as_str())
            .bind(source.map(|link| link.kind.clone()))
            .bind(source.map(|link| link.reference.clone()))
            .bind(source.and_then(|link| link.label.clone()))
            .bind(source.and_then(|link| link.url.clone()))
            .bind(origin.as_str())
            .bind(proposer.as_ref().map(|who| who.agent_id.clone()))
            .bind(proposer.as_ref().map(|who| who.run_id))
            .fetch_one(self.pool())
            .await?;
        Ok(row.into())
    }

    /// Returns a task.
    pub async fn task(&self, id: Uuid) -> Result<Option<Task>, DeskError> {
        let query = format!("SELECT {COLUMNS} FROM tasks WHERE tenant_id = $1 AND id = $2");
        let row: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Lists tasks of a view. `today` is the current date of the owner.
    pub async fn tasks(
        &self,
        view: TaskView,
        today: NaiveDate,
        limit: i64,
    ) -> Result<Vec<Task>, DeskError> {
        let filter = match view {
            TaskView::All => "status IN ('open', 'proposed') ORDER BY position, created_at",
            TaskView::Today => "status = 'open' AND due_on <= $2 ORDER BY due_on, position",
            TaskView::Upcoming => {
                "status = 'open' AND (due_on > $2 OR due_on IS NULL) \
                 ORDER BY due_on NULLS LAST, position"
            }
            TaskView::Done => "status = 'done' ORDER BY done_at DESC",
            TaskView::Proposed => "status = 'proposed' ORDER BY created_at DESC",
        };
        let query = format!(
            "SELECT {COLUMNS} FROM tasks WHERE tenant_id = $1 AND ($2::date IS NOT NULL) AND {filter} LIMIT $3"
        );
        let rows: Vec<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(today)
            .bind(limit)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Changes a task. Returns `None` for an unknown task.
    pub async fn update_task(&self, id: Uuid, patch: TaskPatch) -> Result<Option<Task>, DeskError> {
        let Some(mut task) = self.task(id).await? else {
            return Ok(None);
        };
        if let Some(title) = patch.title {
            task.title = required_text("title", &title, MAX_TITLE)?;
        }
        if let Some(notes) = patch.notes {
            task.notes = check_notes(notes)?;
        }
        if let Some(due_on) = patch.due_on {
            task.due_on = due_on;
        }
        if let Some(priority) = patch.priority {
            task.priority = check_priority(priority)?;
        }
        if let Some(source) = patch.source {
            if let Some(source) = &source {
                source.validate()?;
            }
            task.source = source;
        }
        let source = task.source.as_ref();
        let query = format!(
            "UPDATE tasks SET title = $3, notes = $4, due_on = $5, priority = $6, source_kind = $7, \
             source_ref = $8, source_label = $9, source_url = $10, updated_at = now() \
             WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        let row: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(&task.title)
            .bind(&task.notes)
            .bind(task.due_on)
            .bind(task.priority)
            .bind(source.map(|link| link.kind.clone()))
            .bind(source.map(|link| link.reference.clone()))
            .bind(source.and_then(|link| link.label.clone()))
            .bind(source.and_then(|link| link.url.clone()))
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    async fn move_task(
        &self,
        id: Uuid,
        from: &[TaskStatus],
        to: TaskStatus,
    ) -> Result<Option<Task>, DeskError> {
        let from: Vec<&str> = from.iter().map(|status| status.as_str()).collect();
        let query = format!(
            "UPDATE tasks SET status = $3, updated_at = now(), \
             done_at = CASE WHEN $3 = 'done' THEN now() ELSE NULL END \
             WHERE tenant_id = $1 AND id = $2 AND status = ANY($4) RETURNING {COLUMNS}"
        );
        let row: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(to.as_str())
            .bind(from)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Marks a task as done. Returns `None` if it is unknown or already done.
    pub async fn complete_task(&self, id: Uuid) -> Result<Option<Task>, DeskError> {
        self.move_task(
            id,
            &[TaskStatus::Open, TaskStatus::Proposed],
            TaskStatus::Done,
        )
        .await
    }

    /// Puts a done task back on the list.
    pub async fn reopen_task(&self, id: Uuid) -> Result<Option<Task>, DeskError> {
        self.move_task(id, &[TaskStatus::Done], TaskStatus::Open)
            .await
    }

    /// Accepts the proposal of an agent.
    pub async fn accept_task(&self, id: Uuid) -> Result<Option<Task>, DeskError> {
        self.move_task(id, &[TaskStatus::Proposed], TaskStatus::Open)
            .await
    }

    /// Deletes a task. Returns whether it existed.
    pub async fn delete_task(&self, id: Uuid) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM tasks WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Puts the given tasks in this order. Other tasks keep their place after them.
    pub async fn reorder_tasks(&self, ids: &[Uuid]) -> Result<u64, DeskError> {
        let positions: Vec<f64> = (0..ids.len()).map(|index| index as f64).collect();
        let mut transaction = self.pool().begin().await?;
        sqlx::query(
            "UPDATE tasks SET position = $2 + rest.rank FROM \
             (SELECT id, row_number() OVER (ORDER BY position, created_at) AS rank FROM tasks \
              WHERE tenant_id = $1 AND NOT (id = ANY($3))) AS rest \
             WHERE tasks.id = rest.id",
        )
        .bind(self.tenant())
        .bind(ids.len() as f64)
        .bind(ids)
        .execute(&mut *transaction)
        .await?;
        let result = sqlx::query(
            "UPDATE tasks SET position = ordered.position, updated_at = now() \
             FROM UNNEST($2::uuid[], $3::float8[]) AS ordered(id, position) \
             WHERE tasks.tenant_id = $1 AND tasks.id = ordered.id",
        )
        .bind(self.tenant())
        .bind(ids)
        .bind(positions)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    async fn desk() -> Desk {
        Desk::new(fresh_store().await, TenantId::SINGLE)
    }

    fn day(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn new(title: &str, due: Option<&str>) -> NewTask {
        NewTask {
            title: title.to_owned(),
            due_on: due.map(day),
            ..NewTask::default()
        }
    }

    #[tokio::test]
    async fn views_split_today_upcoming_done_and_proposed() {
        let desk = desk().await;
        let today = day("2026-03-10");
        let overdue = desk
            .create_task(new("Overdue", Some("2026-03-09")), None)
            .await
            .unwrap();
        desk.create_task(new("Today", Some("2026-03-10")), None)
            .await
            .unwrap();
        desk.create_task(new("Later", Some("2026-03-12")), None)
            .await
            .unwrap();
        desk.create_task(new("Someday", None), None).await.unwrap();
        let proposer = Proposer {
            agent_id: "inbox".to_owned(),
            run_id: Uuid::now_v7(),
        };
        let proposal = desk
            .create_task(new("Reply to the offer", None), Some(proposer))
            .await
            .unwrap();
        assert_eq!(proposal.status, TaskStatus::Proposed);
        assert_eq!(proposal.origin, Origin::Agent);

        let titles = |tasks: Vec<Task>| tasks.into_iter().map(|t| t.title).collect::<Vec<_>>();
        let list = |view| desk.tasks(view, today, 50);
        assert_eq!(
            titles(list(TaskView::Today).await.unwrap()),
            ["Overdue", "Today"]
        );
        assert_eq!(
            titles(list(TaskView::Upcoming).await.unwrap()),
            ["Later", "Someday"]
        );
        assert_eq!(
            titles(list(TaskView::Proposed).await.unwrap()),
            ["Reply to the offer"]
        );
        assert_eq!(
            titles(list(TaskView::All).await.unwrap()),
            ["Reply to the offer", "Someday", "Later", "Today", "Overdue"],
            "new tasks go to the top"
        );

        let done = desk.complete_task(overdue.id).await.unwrap().unwrap();
        assert!(done.done_at.is_some());
        assert!(desk.complete_task(overdue.id).await.unwrap().is_none());
        assert_eq!(titles(list(TaskView::Done).await.unwrap()), ["Overdue"]);
        let reopened = desk.reopen_task(overdue.id).await.unwrap().unwrap();
        assert_eq!(reopened.status, TaskStatus::Open);
        assert!(reopened.done_at.is_none());

        let accepted = desk.accept_task(proposal.id).await.unwrap().unwrap();
        assert_eq!(accepted.status, TaskStatus::Open);
        assert!(desk.accept_task(proposal.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn patches_change_clear_and_validate() {
        let desk = desk().await;
        let source = SourceLink {
            kind: "issue".to_owned(),
            reference: "ENG-1".to_owned(),
            label: Some("Fix login".to_owned()),
            url: None,
        };
        let task = desk
            .create_task(
                NewTask {
                    title: " Review ".to_owned(),
                    notes: Some("first".to_owned()),
                    priority: 2,
                    source: Some(source.clone()),
                    ..NewTask::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(task.title, "Review");
        assert_eq!(task.source, Some(source));

        let patch: TaskPatch =
            serde_json::from_str(r#"{"notes": null, "due_on": "2026-04-01", "source": null}"#)
                .unwrap();
        let changed = desk.update_task(task.id, patch).await.unwrap().unwrap();
        assert_eq!(changed.title, "Review");
        assert_eq!(changed.notes, None);
        assert_eq!(changed.due_on, Some(day("2026-04-01")));
        assert_eq!(changed.priority, 2);
        assert_eq!(changed.source, None);

        let bad = TaskPatch {
            priority: Some(7),
            ..TaskPatch::default()
        };
        assert!(matches!(
            desk.update_task(task.id, bad).await,
            Err(DeskError::Invalid(_))
        ));
        assert!(desk.create_task(new(" ", None), None).await.is_err());
        assert!(
            desk.update_task(Uuid::nil(), TaskPatch::default())
                .await
                .unwrap()
                .is_none()
        );
        assert!(desk.delete_task(task.id).await.unwrap());
        assert!(!desk.delete_task(task.id).await.unwrap());
    }

    #[tokio::test]
    async fn reordering_puts_the_given_tasks_first() {
        let desk = desk().await;
        let today = day("2026-03-10");
        let a = desk.create_task(new("A", None), None).await.unwrap();
        let b = desk.create_task(new("B", None), None).await.unwrap();
        let c = desk.create_task(new("C", None), None).await.unwrap();
        assert_eq!(desk.reorder_tasks(&[a.id, c.id]).await.unwrap(), 2);
        let order: Vec<_> = desk
            .tasks(TaskView::All, today, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect();
        assert_eq!(order, [a.id, c.id, b.id]);
        assert_eq!("done".parse(), Ok(TaskStatus::Done));
        assert!("nope".parse::<TaskStatus>().is_err());
    }
}
