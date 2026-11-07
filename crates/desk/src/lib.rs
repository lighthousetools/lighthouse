//! The owner's desk: tasks, rules, memory, alerts and briefing documents.
//!
//! Everything here belongs to the owner and stays on the instance. Agents can
//! only propose tasks and facts. A proposal changes nothing until the owner
//! accepts it.

pub mod alerts;
pub mod automations;
pub mod branding;
pub mod documents;
pub mod facts;
pub mod post_check;
pub mod posts;
pub mod ready;
pub mod rules;
mod settings;
pub mod talk;
pub mod tasks;

pub use alerts::{Alert, AlertCounts, AlertEvent, AlertView, FAILURES, NewAlert, Raised, Severity};
pub use branding::BrandProfile;
pub use documents::{
    BriefingContent, BriefingDocument, Connection, DocumentItem, DocumentSection, ItemNumber,
    ItemSource, MORNING_BRIEF, ProjectWork, WEEKLY_RECAP, Weather, WeekDay, extract_document,
};
pub use facts::{Fact, FactFilter, FactPatch, FactSource, NewFact, ProposedFact};
pub use post_check::{CheckIssue, PostCheck, check_post};
pub use posts::{
    CalendarSlot, ImportedPost, NewPost, Pillar, PillarInput, Post, PostFilter, PostPatch,
    PostSlot, Variant, parse_csv,
};
pub use rules::{NewRule, Rule, RulePatch};
pub use talk::{TalkMessage, TalkThread, transcript};
pub use tasks::{NewTask, Proposer, Task, TaskPatch, TaskStatus, TaskView};

use lighthouse_core::TenantId;
use lighthouse_store::Store;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

/// Errors from the desk.
#[derive(Debug, thiserror::Error)]
pub enum DeskError {
    /// A database operation failed.
    #[error("desk storage error: {0}")]
    Database(#[from] sqlx::Error),
    /// The input is invalid. The text is safe to show.
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> DeskError {
    DeskError::Invalid(message.into())
}

/// Who created something.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The owner.
    Owner,
    /// An agent, as a proposal.
    Agent,
    /// The pipeline suggestions, as a proposal.
    Pipeline,
    /// The reminders of holdings, as a proposal.
    Holdings,
}

impl Origin {
    /// Returns the stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Agent => "agent",
            Self::Pipeline => "pipeline",
            Self::Holdings => "holdings",
        }
    }

    /// Reads a stored name. Unknown names count as the owner's.
    pub fn parse(value: &str) -> Self {
        [Self::Agent, Self::Pipeline, Self::Holdings]
            .into_iter()
            .find(|origin| origin.as_str() == value)
            .unwrap_or(Self::Owner)
    }
}

/// A link to the thing something came from, such as a run, an approval, a mail
/// or an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SourceLink {
    /// What it points to, such as `run`, `approval`, `mail`, `issue` or `brain`.
    pub kind: String,
    /// ID within that kind, such as a run ID or an issue key.
    pub reference: String,
    /// Display text.
    #[serde(default)]
    pub label: Option<String>,
    /// Address to open it, if it lives outside Lighthouse.
    #[serde(default)]
    pub url: Option<String>,
}

impl SourceLink {
    fn from_columns(
        kind: Option<String>,
        reference: Option<String>,
        label: Option<String>,
        url: Option<String>,
    ) -> Option<Self> {
        Some(Self {
            kind: kind?,
            reference: reference?,
            label,
            url,
        })
    }

    fn validate(&self) -> Result<(), DeskError> {
        if self.kind.trim().is_empty() || self.reference.trim().is_empty() {
            return Err(invalid("a source needs a kind and a reference"));
        }
        Ok(())
    }
}

/// Deserializes a field that may be missing, `null` or a value, so a patch can
/// tell "leave as is" from "clear".
pub(crate) fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Checks and trims a required text.
pub(crate) fn required_text(name: &str, value: &str, max: usize) -> Result<String, DeskError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid(format!("`{name}` must not be empty")));
    }
    if value.chars().count() > max {
        return Err(invalid(format!("`{name}` is longer than {max} characters")));
    }
    Ok(value.to_owned())
}

/// The desk of one tenant.
#[derive(Clone, Debug)]
pub struct Desk {
    store: Store,
    tenant: TenantId,
}

impl Desk {
    /// Creates the desk.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self { store, tenant }
    }

    fn pool(&self) -> &sqlx::PgPool {
        self.store.pool()
    }

    fn tenant(&self) -> uuid::Uuid {
        self.tenant.as_uuid()
    }

    /// Returns the agent of a run, for proposals that name their source.
    pub async fn agent_of_run(&self, run: uuid::Uuid) -> Result<Option<String>, DeskError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT agent_id FROM runs WHERE tenant_id = $1 AND id = $2")
                .bind(self.tenant())
                .bind(run)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(agent,)| agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_trimmed_and_bounded() {
        assert_eq!(required_text("title", "  Call  ", 10).unwrap(), "Call");
        assert!(required_text("title", "   ", 10).is_err());
        assert!(required_text("title", "far too long", 5).is_err());
    }

    #[test]
    fn sources_need_kind_and_reference() {
        let link = SourceLink {
            kind: "issue".to_owned(),
            reference: " ".to_owned(),
            label: None,
            url: None,
        };
        assert!(link.validate().is_err());
        assert_eq!(
            SourceLink::from_columns(Some("run".to_owned()), None, None, None),
            None
        );
        assert_eq!(Origin::Agent.as_str(), "agent");
        assert_eq!(Origin::parse("pipeline"), Origin::Pipeline);
        assert_eq!(Origin::parse("holdings"), Origin::Holdings);
        assert_eq!(Origin::parse("other"), Origin::Owner);
    }

    #[test]
    fn double_options_tell_missing_from_null() {
        #[derive(Deserialize)]
        struct Patch {
            #[serde(default, deserialize_with = "double_option")]
            notes: Option<Option<String>>,
        }
        let missing: Patch = serde_json::from_str("{}").unwrap();
        let cleared: Patch = serde_json::from_str(r#"{"notes": null}"#).unwrap();
        let set: Patch = serde_json::from_str(r#"{"notes": "x"}"#).unwrap();
        assert_eq!(missing.notes, None);
        assert_eq!(cleared.notes, Some(None));
        assert_eq!(set.notes, Some(Some("x".to_owned())));
    }
}
