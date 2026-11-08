//! What the assistant knows about the owner, such as "prefers short answers" or
//! "works from home on Fridays".
//!
//! A fact the owner states is confirmed at once. A fact an agent learned is a
//! proposal with a confidence until the owner confirms it. Only confirmed facts
//! that are not paused reach the prompts of runs.

use crate::{Desk, DeskError, Proposer, double_option, invalid, required_text};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_TEXT: usize = 1_000;
const MAX_CATEGORY: usize = 100;
/// Facts in one prompt. The newest confirmed facts win.
pub const PROMPT_FACTS: i64 = 100;
const COLUMNS: &str = "id, text, category, status, paused, confidence, source_kind, source_ref, \
                       source_label, agent_id, run_id, created_at, updated_at, confirmed_at";

/// Where a fact came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactSource {
    /// `owner` when the owner stated it, `agent` when a run learned it.
    pub kind: String,
    /// ID within that kind, such as the run ID.
    pub reference: Option<String>,
    /// What it was learned from, in a few words.
    pub label: Option<String>,
}

/// A fact about the owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Fact {
    /// Fact ID.
    pub id: Uuid,
    /// The fact, as one sentence.
    pub text: String,
    /// Optional grouping, such as `work`, `writing` or `people`.
    pub category: Option<String>,
    /// `proposed` or `confirmed`.
    pub status: String,
    /// Whether the owner paused it. Paused facts stay out of prompts.
    pub paused: bool,
    /// From 0 to 1. Facts of the owner have 1.
    pub confidence: f64,
    /// Where it came from.
    pub source: FactSource,
    /// The agent that learned it.
    pub agent_id: Option<String>,
    /// The run that learned it.
    pub run_id: Option<Uuid>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// When the owner confirmed it.
    pub confirmed_at: Option<DateTime<Utc>>,
}

#[derive(FromRow)]
struct FactRow {
    id: Uuid,
    text: String,
    category: Option<String>,
    status: String,
    paused: bool,
    confidence: f64,
    source_kind: String,
    source_ref: Option<String>,
    source_label: Option<String>,
    agent_id: Option<String>,
    run_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    confirmed_at: Option<DateTime<Utc>>,
}

impl From<FactRow> for Fact {
    fn from(row: FactRow) -> Self {
        Self {
            id: row.id,
            text: row.text,
            category: row.category,
            status: row.status,
            paused: row.paused,
            confidence: row.confidence,
            source: FactSource {
                kind: row.source_kind,
                reference: row.source_ref,
                label: row.source_label,
            },
            agent_id: row.agent_id,
            run_id: row.run_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
            confirmed_at: row.confirmed_at,
        }
    }
}

/// A fact the owner states.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct NewFact {
    /// The fact.
    pub text: String,
    /// Optional grouping.
    #[serde(default)]
    pub category: Option<String>,
}

/// A fact an agent learned.
#[derive(Clone, Debug, Default)]
pub struct ProposedFact {
    /// The fact.
    pub text: String,
    /// Optional grouping.
    pub category: Option<String>,
    /// From 0 to 1.
    pub confidence: f64,
    /// What it was learned from, in a few words.
    pub evidence: Option<String>,
}

/// Changes to a fact. Missing fields stay, `null` clears.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct FactPatch {
    /// New text. Rewording a fact confirms it.
    #[serde(default)]
    pub text: Option<String>,
    /// New grouping.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub category: Option<Option<String>>,
    /// Pause or resume.
    #[serde(default)]
    pub paused: Option<bool>,
}

/// Which facts to list.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FactFilter {
    /// `proposed` or `confirmed`.
    pub status: Option<String>,
    /// Only this grouping.
    pub category: Option<String>,
}

fn check_category(category: Option<String>) -> Result<Option<String>, DeskError> {
    category
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .map(|text| required_text("category", &text, MAX_CATEGORY))
        .transpose()
}

impl Desk {
    async fn insert_fact(
        &self,
        text: &str,
        category: Option<String>,
        confidence: f64,
        source: FactSource,
        proposer: Option<&Proposer>,
    ) -> Result<Fact, DeskError> {
        let text = required_text("text", text, MAX_TEXT)?;
        let status = if proposer.is_some() {
            "proposed"
        } else {
            "confirmed"
        };
        let query = format!(
            "INSERT INTO facts (id, tenant_id, text, category, status, confidence, source_kind, \
             source_ref, source_label, agent_id, run_id, confirmed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
             CASE WHEN $5 = 'confirmed' THEN now() END) RETURNING {COLUMNS}"
        );
        let row: FactRow = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(text)
            .bind(check_category(category)?)
            .bind(status)
            .bind(confidence)
            .bind(source.kind)
            .bind(source.reference)
            .bind(source.label)
            .bind(proposer.and_then(|who| who.agent_id.clone()))
            .bind(proposer.and_then(|who| who.run_id))
            .fetch_one(self.pool())
            .await?;
        Ok(row.into())
    }

    /// Stores a fact the owner states. It is confirmed at once.
    pub async fn create_fact(&self, fact: NewFact) -> Result<Fact, DeskError> {
        let source = FactSource {
            kind: "owner".to_owned(),
            reference: None,
            label: None,
        };
        self.insert_fact(&fact.text, fact.category, 1.0, source, None)
            .await
    }

    /// Stores what an agent learned, as a proposal.
    pub async fn propose_fact(
        &self,
        fact: ProposedFact,
        proposer: &Proposer,
    ) -> Result<Fact, DeskError> {
        if !(0.0..=1.0).contains(&fact.confidence) {
            return Err(invalid("`confidence` must be between 0 and 1"));
        }
        let source = FactSource {
            kind: "agent".to_owned(),
            reference: proposer.run_id.map(|run| run.to_string()),
            label: fact
                .evidence
                .map(|text| text.trim().chars().take(MAX_TEXT).collect::<String>())
                .filter(|text| !text.is_empty()),
        };
        self.insert_fact(
            &fact.text,
            fact.category,
            fact.confidence,
            source,
            Some(proposer),
        )
        .await
    }

    /// Lists facts, newest first.
    pub async fn facts(&self, filter: &FactFilter) -> Result<Vec<Fact>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM facts WHERE tenant_id = $1 \
             AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR category = $3) \
             ORDER BY updated_at DESC, id"
        );
        let rows: Vec<FactRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(&filter.status)
            .bind(&filter.category)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Returns a fact.
    pub async fn fact(&self, id: Uuid) -> Result<Option<Fact>, DeskError> {
        let query = format!("SELECT {COLUMNS} FROM facts WHERE tenant_id = $1 AND id = $2");
        let row: Option<FactRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Returns the confirmed facts that are not paused, for prompts.
    pub async fn known_facts(&self) -> Result<Vec<Fact>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM facts WHERE tenant_id = $1 AND status = 'confirmed' \
             AND NOT paused ORDER BY confirmed_at DESC, id LIMIT $2"
        );
        let rows: Vec<FactRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(PROMPT_FACTS)
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Changes a fact. Rewording confirms it, since the owner wrote it.
    pub async fn update_fact(&self, id: Uuid, patch: FactPatch) -> Result<Option<Fact>, DeskError> {
        let Some(mut fact) = self.fact(id).await? else {
            return Ok(None);
        };
        let reworded = patch.text.is_some();
        if let Some(text) = patch.text {
            fact.text = required_text("text", &text, MAX_TEXT)?;
        }
        if let Some(category) = patch.category {
            fact.category = check_category(category)?;
        }
        if let Some(paused) = patch.paused {
            fact.paused = paused;
        }
        let query = format!(
            "UPDATE facts SET text = $3, category = $4, paused = $5, updated_at = now(), \
             status = CASE WHEN $6 THEN 'confirmed' ELSE status END, \
             confirmed_at = CASE WHEN $6 THEN COALESCE(confirmed_at, now()) ELSE confirmed_at END \
             WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        let row: Option<FactRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(&fact.text)
            .bind(&fact.category)
            .bind(fact.paused)
            .bind(reworded)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Confirms a proposed fact. Returns `None` if it is unknown or confirmed already.
    pub async fn confirm_fact(&self, id: Uuid) -> Result<Option<Fact>, DeskError> {
        let query = format!(
            "UPDATE facts SET status = 'confirmed', confirmed_at = now(), updated_at = now() \
             WHERE tenant_id = $1 AND id = $2 AND status = 'proposed' RETURNING {COLUMNS}"
        );
        let row: Option<FactRow> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(Into::into))
    }

    /// Forgets a fact. Returns whether it existed.
    pub async fn delete_fact(&self, id: Uuid) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM facts WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;

    fn proposer() -> Proposer {
        Proposer::agent("inbox".to_owned(), Uuid::now_v7())
    }

    #[tokio::test]
    async fn only_confirmed_and_active_facts_are_known() {
        let desk = Desk::new(fresh_store().await, TenantId::DEFAULT);
        let stated = desk
            .create_fact(NewFact {
                text: "Prefers short answers".to_owned(),
                category: Some("writing".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(stated.status, "confirmed");
        assert_eq!(stated.source.kind, "owner");
        assert!(stated.confirmed_at.is_some());
        let learned = desk
            .propose_fact(
                ProposedFact {
                    text: "Works from home on Fridays".to_owned(),
                    confidence: 0.7,
                    evidence: Some(" Out-of-office replies ".to_owned()),
                    ..ProposedFact::default()
                },
                &proposer(),
            )
            .await
            .unwrap();
        assert_eq!(learned.status, "proposed");
        assert_eq!(
            learned.source.label.as_deref(),
            Some("Out-of-office replies")
        );
        let known = |desk: Desk| async move {
            desk.known_facts()
                .await
                .unwrap()
                .into_iter()
                .map(|fact| fact.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(known(desk.clone()).await, ["Prefers short answers"]);

        assert!(desk.confirm_fact(learned.id).await.unwrap().is_some());
        assert!(desk.confirm_fact(learned.id).await.unwrap().is_none());
        assert_eq!(known(desk.clone()).await.len(), 2);

        let pause = FactPatch {
            paused: Some(true),
            ..FactPatch::default()
        };
        desk.update_fact(stated.id, pause).await.unwrap().unwrap();
        assert_eq!(known(desk.clone()).await, ["Works from home on Fridays"]);

        let filter = FactFilter {
            category: Some("writing".to_owned()),
            ..FactFilter::default()
        };
        assert_eq!(desk.facts(&filter).await.unwrap().len(), 1);
        assert_eq!(desk.facts(&FactFilter::default()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rewording_confirms_and_bad_input_is_refused() {
        let desk = Desk::new(fresh_store().await, TenantId::DEFAULT);
        let bad = ProposedFact {
            text: "Likes tea".to_owned(),
            confidence: 1.5,
            ..ProposedFact::default()
        };
        assert!(desk.propose_fact(bad, &proposer()).await.is_err());
        let fact = desk
            .propose_fact(
                ProposedFact {
                    text: "Likes tea".to_owned(),
                    confidence: 0.4,
                    ..ProposedFact::default()
                },
                &proposer(),
            )
            .await
            .unwrap();
        let patch: FactPatch =
            serde_json::from_str(r#"{"text": "Likes green tea", "category": null}"#).unwrap();
        let changed = desk.update_fact(fact.id, patch).await.unwrap().unwrap();
        assert_eq!(changed.status, "confirmed");
        assert_eq!(changed.text, "Likes green tea");
        assert!(desk.create_fact(NewFact::default()).await.is_err());
        assert!(
            desk.update_fact(Uuid::nil(), FactPatch::default())
                .await
                .unwrap()
                .is_none()
        );
        assert!(desk.delete_fact(fact.id).await.unwrap());
        assert!(!desk.delete_fact(fact.id).await.unwrap());
        assert!(desk.fact(fact.id).await.unwrap().is_none());
    }
}
