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
                       source_label, source_url, origin, agent_id, run_id, created_at, updated_at, done_at, \
                       delegated_agent, delegated_run, result, result_status";

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
    /// The agent the task was handed to.
    pub delegated_agent: Option<String>,
    /// The run that works on it.
    pub delegated_run: Option<Uuid>,
    /// The report of that run, once it ended.
    pub result: Option<String>,
    /// How that run ended, such as `succeeded` or `failed`.
    pub result_status: Option<String>,
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
    delegated_agent: Option<String>,
    delegated_run: Option<Uuid>,
    result: Option<String>,
    result_status: Option<String>,
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
            origin: Origin::parse(&row.origin),
            agent_id: row.agent_id,
            run_id: row.run_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
            done_at: row.done_at,
            delegated_agent: row.delegated_agent,
            delegated_run: row.delegated_run,
            result: row.result,
            result_status: row.result_status,
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

/// Who proposes a task or a fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposer {
    /// Where the proposal comes from.
    pub origin: Origin,
    /// The agent, for proposals of agents.
    pub agent_id: Option<String>,
    /// The run, for proposals of agents.
    pub run_id: Option<Uuid>,
}

impl Proposer {
    /// A proposal of an agent run.
    pub fn agent(agent_id: impl Into<String>, run_id: Uuid) -> Self {
        Self {
            origin: Origin::Agent,
            agent_id: Some(agent_id.into()),
            run_id: Some(run_id),
        }
    }

    /// A proposal of Lighthouse itself, such as the pipeline suggestions.
    pub fn system(origin: Origin) -> Self {
        Self {
            origin,
            agent_id: None,
            run_id: None,
        }
    }
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
        let (status, origin) = match &proposer {
            Some(proposer) => (TaskStatus::Proposed, proposer.origin),
            None => (TaskStatus::Open, Origin::Owner),
        };
        let source = task.source.as_ref();
        if proposer.is_some() {
            // A proposal equal to one still open, by its source or its title,
            // refreshes that one instead of adding a second.
            let query = format!(
                "UPDATE tasks SET notes = COALESCE($3, notes), due_on = COALESCE($4, due_on), \
                 priority = GREATEST(priority, $5), updated_at = now() \
                 WHERE id = (SELECT id FROM tasks WHERE tenant_id = $1 AND status = 'proposed' \
                 AND ((source_kind = $6 AND source_ref = $7) OR lower(title) = lower($2)) \
                 ORDER BY created_at DESC LIMIT 1) RETURNING {COLUMNS}"
            );
            let existing: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
                .bind(self.tenant())
                .bind(&title)
                .bind(&notes)
                .bind(task.due_on)
                .bind(priority)
                .bind(source.map(|link| link.kind.clone()))
                .bind(source.map(|link| link.reference.clone()))
                .fetch_optional(self.pool())
                .await?;
            if let Some(row) = existing {
                return Ok(row.into());
            }
        }
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
            .bind(proposer.as_ref().and_then(|who| who.agent_id.clone()))
            .bind(proposer.as_ref().and_then(|who| who.run_id))
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

    /// Notes that an agent run works on a task.
    pub async fn delegate_task(
        &self,
        id: Uuid,
        agent: &str,
        run: Uuid,
    ) -> Result<Option<Task>, DeskError> {
        let query = format!(
            "UPDATE tasks SET delegated_agent = $3, delegated_run = $4, result = NULL, \
             result_status = NULL, updated_at = now() WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        let row: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(agent)
            .bind(run)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Records the end of a run on the task it works on, if any.
    pub async fn finish_delegation(
        &self,
        run: Uuid,
        status: &str,
        result: Option<&str>,
    ) -> Result<Option<Task>, DeskError> {
        let query = format!(
            "UPDATE tasks SET result = $3, result_status = $4, updated_at = now() \
             WHERE tenant_id = $1 AND delegated_run = $2 RETURNING {COLUMNS}"
        );
        let row: Option<TaskRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(run)
            .bind(result)
            .bind(status)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
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
        let proposer = Proposer::agent("inbox".to_owned(), Uuid::now_v7());
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
    async fn a_delegated_task_gets_the_result_of_its_run() {
        let desk = desk().await;
        let task = desk
            .create_task(new("Draft the offer", None), None)
            .await
            .unwrap();
        let run = Uuid::now_v7();
        let delegated = desk
            .delegate_task(task.id, "writer", run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delegated.delegated_agent.as_deref(), Some("writer"));
        assert!(
            desk.finish_delegation(Uuid::now_v7(), "succeeded", None)
                .await
                .unwrap()
                .is_none()
        );
        let done = desk
            .finish_delegation(run, "succeeded", Some("Here is the draft"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.result.as_deref(), Some("Here is the draft"));
        assert_eq!(
            done.status,
            TaskStatus::Open,
            "the owner decides when it is done"
        );
        assert!(
            desk.delegate_task(Uuid::nil(), "writer", run)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_equal_open_proposal_is_refreshed_instead_of_repeated() {
        let desk = desk().await;
        let today = day("2026-01-01");
        let proposer = || Some(Proposer::agent("inbox", Uuid::now_v7()));
        let mut from_mail = new("Answer John", None);
        from_mail.source = Some(SourceLink {
            kind: "mail".to_owned(),
            reference: "t1".to_owned(),
            label: None,
            url: None,
        });
        let first = desk
            .create_task(from_mail.clone(), proposer())
            .await
            .unwrap();
        from_mail.title = "Reply to John".to_owned();
        from_mail.notes = Some("He asked twice".to_owned());
        let second = desk.create_task(from_mail, proposer()).await.unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.notes.as_deref(), Some("He asked twice"));
        let titled = desk
            .create_task(new("answer john", None), proposer())
            .await
            .unwrap();
        assert_eq!(titled.id, first.id, "same title, any case");
        desk.create_task(new("Something else", None), proposer())
            .await
            .unwrap();
        desk.create_task(new("Answer John", None), None)
            .await
            .unwrap();
        assert_eq!(
            desk.tasks(TaskView::Proposed, today, 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn old_placeholder_proposals_get_their_origin() {
        let desk = desk().await;
        sqlx::query(
            "INSERT INTO tasks (id, tenant_id, title, status, position, origin, agent_id, run_id) \
             VALUES ($1, $2, 'Old', 'proposed', 0, 'agent', 'pipeline', '00000000-0000-0000-0000-000000000000')",
        )
        .bind(Uuid::now_v7())
        .bind(lighthouse_core::TenantId::SINGLE.as_uuid())
        .execute(desk.pool())
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../store/migrations/0024_proposal_origin.sql"
        ))
        .execute(desk.pool())
        .await
        .unwrap();
        let today = day("2026-01-01");
        let task = desk
            .tasks(TaskView::Proposed, today, 5)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            (task.origin, task.agent_id, task.run_id),
            (Origin::Pipeline, None, None)
        );
        let system = desk
            .create_task(new("New", None), Some(Proposer::system(Origin::Holdings)))
            .await
            .unwrap();
        assert_eq!(
            (system.origin, system.status),
            (Origin::Holdings, TaskStatus::Proposed)
        );
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
