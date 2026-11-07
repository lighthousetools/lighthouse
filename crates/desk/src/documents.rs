//! Structured briefing documents, such as the morning brief and the weekly recap.
//!
//! A briefing agent ends its Markdown report with a fenced `lighthouse-document`
//! block of JSON. The runtime takes the block out of the report, checks it and
//! stores it next to the run. The format is described in `docs/documents.md`.

use crate::{Desk, DeskError, invalid};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

/// Language tag of the fenced block.
pub const FENCE: &str = "lighthouse-document";
/// Kind of the daily brief.
pub const MORNING_BRIEF: &str = "morning_brief";
/// Kind of the weekly recap.
pub const WEEKLY_RECAP: &str = "weekly_recap";
/// Days a brief looks ahead, at most.
const MAX_WEEK_DAYS: usize = 14;
const MAX_KIND: usize = 40;
const MAX_SECTIONS: usize = 20;
const MAX_ITEMS: usize = 100;
const COLUMNS: &str = "id, run_id, agent_id, kind, period, title, content, created_at";

/// Where an item comes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ItemSource {
    /// Service name, such as `linear` or `github`.
    pub service: String,
    /// ID within the service, such as an issue key or a pull request number.
    #[serde(default)]
    pub reference: Option<String>,
    /// Address to open it.
    #[serde(default)]
    pub url: Option<String>,
}

/// A number of an item, such as merged pull requests this week.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ItemNumber {
    /// What is counted.
    pub label: String,
    /// The value.
    pub value: f64,
    /// Unit, such as `EUR` or `%`.
    #[serde(default)]
    pub unit: Option<String>,
    /// The value of the period before, for comparison.
    #[serde(default)]
    pub previous: Option<f64>,
}

/// One line of a section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DocumentItem {
    /// The statement, one sentence.
    pub text: String,
    /// A short title for a card, such as "Globex offer".
    #[serde(default)]
    pub title: Option<String>,
    /// A line under the title, such as "Mail from John Roe · yesterday".
    #[serde(default)]
    pub subtitle: Option<String>,
    /// More detail, if needed.
    #[serde(default)]
    pub detail: Option<String>,
    /// Where it comes from.
    #[serde(default)]
    pub source: Option<ItemSource>,
    /// Numbers that belong to it.
    #[serde(default)]
    pub numbers: Vec<ItemNumber>,
}

/// A section, such as "Needs you".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DocumentSection {
    /// Stable key for the apps, such as `needs_you`.
    #[serde(default)]
    pub key: Option<String>,
    /// Heading.
    pub title: String,
    /// Its lines.
    #[serde(default)]
    pub items: Vec<DocumentItem>,
}

/// The structured result of a briefing run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BriefingContent {
    /// `morning_brief`, `weekly_recap` or another lowercase kind.
    pub kind: String,
    /// Title.
    pub title: String,
    /// A few sentences on the whole.
    #[serde(default)]
    pub summary: Option<String>,
    /// The sections in order.
    pub sections: Vec<DocumentSection>,
    /// Sources that could not be read.
    #[serde(default)]
    pub unavailable: Vec<String>,
    /// How things connect: short stories that link items across services, such
    /// as a mail, the deal it is about and the meeting where it is decided.
    #[serde(default)]
    pub connections: Vec<Connection>,
    /// The days ahead, for a brief: what each day holds.
    #[serde(default)]
    pub week: Vec<WeekDay>,
    /// The weather of the day, filled by Lighthouse when `[weather]` is set.
    #[serde(default)]
    pub weather: Option<Weather>,
    /// Work grouped by project, for a recap.
    #[serde(default)]
    pub projects: Vec<ProjectWork>,
}

/// A story that links items across services.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Connection {
    /// One line, such as "The Globex renewal hangs on one answer".
    pub title: String,
    /// Two or three sentences on how the items connect.
    pub text: String,
    /// The items it links, in the order of the story.
    #[serde(default)]
    pub items: Vec<ItemSource>,
}

/// One day ahead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WeekDay {
    /// The date, `YYYY-MM-DD`.
    pub date: String,
    /// A word on the day, such as "Busy" or "Focus day".
    #[serde(default)]
    pub label: Option<String>,
    /// What it holds.
    #[serde(default)]
    pub items: Vec<DocumentItem>,
}

/// The weather of a day.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Weather {
    /// Such as "Light rain".
    pub summary: String,
    /// The temperature now, in °C.
    #[serde(default)]
    pub temperature_c: Option<f64>,
    /// The highest of the day, in °C.
    #[serde(default)]
    pub high_c: Option<f64>,
    /// The lowest of the day, in °C.
    #[serde(default)]
    pub low_c: Option<f64>,
    /// The place, such as "Berlin".
    #[serde(default)]
    pub location: Option<String>,
    /// Where it comes from, such as "Open-Meteo".
    #[serde(default)]
    pub source: Option<String>,
}

/// The work of one project in a recap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectWork {
    /// Project name.
    pub name: String,
    /// What got done.
    #[serde(default)]
    pub done: Vec<DocumentItem>,
    /// What is still open or at risk.
    #[serde(default)]
    pub open: Vec<DocumentItem>,
    /// Its numbers, such as issues closed.
    #[serde(default)]
    pub numbers: Vec<ItemNumber>,
}

impl BriefingContent {
    fn validate(&self) -> Result<(), DeskError> {
        let kind_ok = !self.kind.is_empty()
            && self.kind.len() <= MAX_KIND
            && self
                .kind
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_');
        if !kind_ok {
            return Err(invalid("`kind` must be lowercase letters and underscores"));
        }
        if self.title.trim().is_empty() {
            return Err(invalid("`title` must not be empty"));
        }
        if self.sections.len() > MAX_SECTIONS {
            return Err(invalid(format!("at most {MAX_SECTIONS} sections")));
        }
        for section in &self.sections {
            if section.title.trim().is_empty() {
                return Err(invalid("every section needs a `title`"));
            }
            if section.items.len() > MAX_ITEMS {
                return Err(invalid(format!("at most {MAX_ITEMS} items per section")));
            }
            if section.items.iter().any(|item| item.text.trim().is_empty()) {
                return Err(invalid("every item needs a `text`"));
            }
        }
        if self.connections.len() > MAX_ITEMS
            || self.week.len() > MAX_WEEK_DAYS
            || self.projects.len() > MAX_SECTIONS
        {
            return Err(invalid(format!(
                "at most {MAX_ITEMS} connections, {MAX_WEEK_DAYS} days and {MAX_SECTIONS} projects"
            )));
        }
        if self.connections.iter().any(|connection| {
            connection.title.trim().is_empty() || connection.text.trim().is_empty()
        }) {
            return Err(invalid("every connection needs a `title` and a `text`"));
        }
        if self
            .week
            .iter()
            .any(|day| NaiveDate::parse_from_str(&day.date, "%Y-%m-%d").is_err())
        {
            return Err(invalid("every day of `week` needs a `date` as YYYY-MM-DD"));
        }
        if self.projects.iter().any(|project| {
            project.name.trim().is_empty() || project.done.len() + project.open.len() > MAX_ITEMS
        }) {
            return Err(invalid(format!(
                "every project needs a `name` and at most {MAX_ITEMS} items"
            )));
        }
        Ok(())
    }

    /// Returns the period a document of this kind covers on a day:
    /// the ISO week such as `2026-W45` for a weekly recap, the date otherwise.
    pub fn period(&self, day: NaiveDate) -> String {
        if self.kind == WEEKLY_RECAP {
            day.format("%G-W%V").to_string()
        } else {
            day.format("%Y-%m-%d").to_string()
        }
    }
}

/// Takes a `lighthouse-document` block out of a report.
///
/// Returns the report without the block and, if there was one, the checked
/// content or why it is invalid. Only the last block counts.
pub fn extract_document(report: &str) -> (String, Option<Result<BriefingContent, String>>) {
    let opening = format!("```{FENCE}");
    let Some(start) = report.rfind(&opening) else {
        return (report.to_owned(), None);
    };
    let body_start = start + opening.len();
    let Some(length) = report[body_start..].find("\n```") else {
        return (
            report.to_owned(),
            Some(Err("the document block is not closed".to_owned())),
        );
    };
    let json = &report[body_start..body_start + length];
    let end = body_start + length + "\n```".len();
    let markdown = format!("{}{}", &report[..start], &report[end..]);
    let parsed = serde_json::from_str::<BriefingContent>(json.trim())
        .map_err(|error| format!("the document is not valid: {error}"))
        .and_then(|content| {
            content
                .validate()
                .map(|()| content)
                .map_err(|error| error.to_string())
        });
    (markdown.trim().to_owned(), Some(parsed))
}

/// A stored document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BriefingDocument {
    /// Document ID.
    pub id: Uuid,
    /// The run that wrote it.
    pub run_id: Uuid,
    /// The agent of that run.
    pub agent_id: String,
    /// Kind, as in the content.
    pub kind: String,
    /// Covered period: `YYYY-MM-DD`, or `YYYY-Www` for weekly recaps.
    pub period: String,
    /// Title, as in the content.
    pub title: String,
    /// The structured content.
    pub content: BriefingContent,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct DocumentRow {
    id: Uuid,
    run_id: Uuid,
    agent_id: String,
    kind: String,
    period: String,
    title: String,
    content: sqlx::types::Json<BriefingContent>,
    created_at: DateTime<Utc>,
}

impl From<DocumentRow> for BriefingDocument {
    fn from(row: DocumentRow) -> Self {
        Self {
            id: row.id,
            run_id: row.run_id,
            agent_id: row.agent_id,
            kind: row.kind,
            period: row.period,
            title: row.title,
            content: row.content.0,
            created_at: row.created_at,
        }
    }
}

impl Desk {
    /// Stores the document of a run. A second one for the same run replaces it.
    pub async fn save_document(
        &self,
        run: Uuid,
        agent_id: &str,
        period: &str,
        content: &BriefingContent,
    ) -> Result<BriefingDocument, DeskError> {
        content.validate()?;
        let query = format!(
            "INSERT INTO run_documents (id, tenant_id, run_id, agent_id, kind, period, title, content) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (run_id) DO UPDATE SET \
             kind = EXCLUDED.kind, period = EXCLUDED.period, title = EXCLUDED.title, \
             content = EXCLUDED.content RETURNING {COLUMNS}"
        );
        let row: DocumentRow = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(run)
            .bind(agent_id)
            .bind(&content.kind)
            .bind(period)
            .bind(&content.title)
            .bind(sqlx::types::Json(content))
            .fetch_one(self.pool())
            .await?;
        Ok(row.into())
    }

    /// Lists documents, latest period first.
    pub async fn documents(
        &self,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<BriefingDocument>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM run_documents WHERE tenant_id = $1 \
             AND ($2::text IS NULL OR kind = $2) ORDER BY period DESC, created_at DESC LIMIT $3"
        );
        let rows: Vec<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(kind)
            .bind(limit)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Returns the latest document of a kind.
    pub async fn latest_document(&self, kind: &str) -> Result<Option<BriefingDocument>, DeskError> {
        Ok(self.documents(Some(kind), 1).await?.into_iter().next())
    }

    /// Returns the latest document of a kind for a period.
    pub async fn document_for(
        &self,
        kind: &str,
        period: &str,
    ) -> Result<Option<BriefingDocument>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM run_documents WHERE tenant_id = $1 AND kind = $2 \
             AND period = $3 ORDER BY created_at DESC LIMIT 1"
        );
        let row: Option<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(kind)
            .bind(period)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Adds the weather to a stored document.
    pub async fn set_document_weather(
        &self,
        id: Uuid,
        weather: &Weather,
    ) -> Result<Option<BriefingDocument>, DeskError> {
        let value = serde_json::to_value(weather).map_err(|error| invalid(error.to_string()))?;
        let query = format!(
            "UPDATE run_documents SET content = jsonb_set(content, '{{weather}}', $3) \
             WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        let row: Option<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(value)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Returns the document of a run.
    pub async fn run_document(&self, run: Uuid) -> Result<Option<BriefingDocument>, DeskError> {
        let query =
            format!("SELECT {COLUMNS} FROM run_documents WHERE tenant_id = $1 AND run_id = $2");
        let row: Option<DocumentRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(run)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    const BLOCK: &str = r#"{"kind": "weekly_recap", "title": "Week 45", "summary": "Calm week.",
        "sections": [{"key": "numbers", "title": "Numbers", "items": [
          {"text": "12 pull requests merged", "source": {"service": "github"},
           "numbers": [{"label": "Merged", "value": 12, "previous": 9}]}]}],
        "unavailable": ["grafana"], "extra": true}"#;

    fn report(block: &str) -> String {
        format!("# Week 45\n\nCalm week.\n\n```{FENCE}\n{block}\n```\n")
    }

    #[test]
    fn the_block_leaves_the_report_and_is_checked() {
        let (markdown, document) = extract_document(&report(BLOCK));
        assert_eq!(markdown, "# Week 45\n\nCalm week.");
        let content = document.unwrap().unwrap();
        assert_eq!(content.sections[0].items[0].numbers[0].previous, Some(9.0));
        assert_eq!(content.unavailable, ["grafana"]);
        let day = "2026-11-06".parse().unwrap();
        assert_eq!(content.period(day), "2026-W45");
        let brief = BriefingContent {
            kind: MORNING_BRIEF.to_owned(),
            ..content
        };
        assert_eq!(brief.period(day), "2026-11-06");

        let (markdown, document) = extract_document("Just text");
        assert_eq!(markdown, "Just text");
        assert!(document.is_none());
    }

    #[test]
    fn the_examples_in_the_bundled_skills_are_valid() {
        for (skill, kind) in [
            (
                include_str!("../../../skills/morning-brief/SKILL.md"),
                MORNING_BRIEF,
            ),
            (
                include_str!("../../../skills/weekly-report/SKILL.md"),
                WEEKLY_RECAP,
            ),
        ] {
            let content = extract_document(skill).1.unwrap().unwrap();
            assert_eq!(content.kind, kind);
            assert!(!content.sections.is_empty());
            if kind == MORNING_BRIEF {
                assert_eq!((content.connections.len(), content.week.len()), (1, 1));
                assert!(content.sections[0].items[0].title.is_some());
            } else {
                assert_eq!(content.projects[0].done.len(), 1);
            }
        }
        let docs = include_str!("../../../docs/documents.md");
        let content = extract_document(docs).1.unwrap().unwrap();
        assert_eq!(content.kind, MORNING_BRIEF);
    }

    #[test]
    fn invalid_blocks_are_reported_and_removed() {
        for block in [
            "not json",
            r#"{"kind": "Weekly", "title": "x", "sections": []}"#,
            r#"{"kind": "weekly_recap", "title": " ", "sections": []}"#,
            r#"{"kind": "weekly_recap", "title": "x", "sections": [{"title": ""}]}"#,
            r#"{"kind": "weekly_recap", "title": "x", "sections": [{"title": "A", "items": [{"text": ""}]}]}"#,
            r#"{"kind": "morning_brief", "title": "x", "sections": [], "connections": [{"title": "", "text": "y"}]}"#,
            r#"{"kind": "morning_brief", "title": "x", "sections": [], "week": [{"date": "Monday"}]}"#,
            r#"{"kind": "weekly_recap", "title": "x", "sections": [], "projects": [{"name": " "}]}"#,
        ] {
            let (markdown, document) = extract_document(&report(block));
            assert!(document.unwrap().is_err(), "{block}");
            assert!(!markdown.contains(FENCE));
        }
        let (markdown, document) = extract_document(&format!("Text\n```{FENCE}\n{{"));
        assert!(document.unwrap().is_err());
        assert!(markdown.contains("Text"));
    }

    #[tokio::test]
    async fn documents_are_stored_per_run_and_found_by_period() {
        let store = fresh_store().await;
        let desk = Desk::new(store.clone(), TenantId::SINGLE);
        let content = extract_document(&report(BLOCK)).1.unwrap().unwrap();
        let mut runs = Vec::new();
        for _ in 0..2 {
            let run = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO runs (id, tenant_id, agent_id, trigger, status) \
                 VALUES ($1, $2, 'weekly-report', 'cron', 'succeeded')",
            )
            .bind(run)
            .bind(TenantId::SINGLE.as_uuid())
            .execute(store.pool())
            .await
            .unwrap();
            runs.push(run);
        }
        desk.save_document(runs[0], "weekly-report", "2026-W44", &content)
            .await
            .unwrap();
        let latest = desk
            .save_document(runs[1], "weekly-report", "2026-W45", &content)
            .await
            .unwrap();
        assert_eq!(
            desk.latest_document(WEEKLY_RECAP)
                .await
                .unwrap()
                .unwrap()
                .id,
            latest.id
        );
        let older = desk
            .document_for(WEEKLY_RECAP, "2026-W44")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(older.run_id, runs[0]);
        assert!(
            desk.document_for(WEEKLY_RECAP, "2020-W01")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(desk.documents(None, 10).await.unwrap().len(), 2);
        assert!(desk.latest_document(MORNING_BRIEF).await.unwrap().is_none());
        assert_eq!(
            desk.run_document(runs[1]).await.unwrap().unwrap().period,
            "2026-W45"
        );
        let mut bad = content.clone();
        bad.kind = "BAD".to_owned();
        assert!(desk.save_document(runs[0], "x", "p", &bad).await.is_err());
    }
}
