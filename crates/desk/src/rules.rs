//! Standing rules the owner gives the assistant, such as "always ask before
//! booking anything".
//!
//! Enabled rules reach every run of their agent, or of every agent when they
//! are global, as owner instructions in the system prompt.

use crate::{Desk, DeskError, double_option, invalid, required_text};
use chrono::{DateTime, Utc};
use lighthouse_core::agent::slugify;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_TEXT: usize = 2_000;
const MAX_CATEGORY: usize = 100;
const COLUMNS: &str =
    "id, text, agent_id, category, enabled, created_at, updated_at, applied_count, last_applied_at";

/// A standing rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Rule {
    /// Rule ID.
    pub id: Uuid,
    /// The rule in the owner's words.
    pub text: String,
    /// The agent it applies to. `None` means every agent.
    pub agent_id: Option<String>,
    /// Optional grouping for the apps, such as `work` or `tech`.
    pub category: Option<String>,
    /// Whether runs see it.
    pub enabled: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// How many runs it reached.
    pub applied_count: i32,
    /// When a run last got it.
    pub last_applied_at: Option<DateTime<Utc>>,
}

/// A rule to create.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct NewRule {
    /// The rule.
    pub text: String,
    /// The agent it applies to. Leave out for every agent.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional grouping.
    #[serde(default)]
    pub category: Option<String>,
    /// Whether runs see it. Defaults to `true`.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// Changes to a rule. Missing fields stay, `null` clears.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct RulePatch {
    /// New text.
    #[serde(default)]
    pub text: Option<String>,
    /// New agent, or `null` for every agent.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub agent_id: Option<Option<String>>,
    /// New grouping.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub category: Option<Option<String>>,
    /// Enable or disable.
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn check_agent(agent: Option<String>) -> Result<Option<String>, DeskError> {
    match agent.map(|id| id.trim().to_owned()) {
        Some(id) if id.is_empty() => Ok(None),
        Some(id) if slugify(&id) != id => Err(invalid(format!(
            "`{id}` is not an agent ID of lowercase letters, digits and dashes"
        ))),
        other => Ok(other),
    }
}

fn check_category(category: Option<String>) -> Result<Option<String>, DeskError> {
    category
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .map(|text| required_text("category", &text, MAX_CATEGORY))
        .transpose()
}

impl Desk {
    /// Creates a rule.
    pub async fn create_rule(&self, rule: NewRule) -> Result<Rule, DeskError> {
        let text = required_text("text", &rule.text, MAX_TEXT)?;
        let query = format!(
            "INSERT INTO rules (id, tenant_id, text, agent_id, category, enabled) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(text)
            .bind(check_agent(rule.agent_id)?)
            .bind(check_category(rule.category)?)
            .bind(rule.enabled)
            .fetch_one(self.pool())
            .await?)
    }

    /// Lists all rules, oldest first.
    pub async fn rules(&self) -> Result<Vec<Rule>, DeskError> {
        let query =
            format!("SELECT {COLUMNS} FROM rules WHERE tenant_id = $1 ORDER BY created_at, id");
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .fetch_all(self.pool())
            .await?)
    }

    /// Returns a rule.
    pub async fn rule(&self, id: Uuid) -> Result<Option<Rule>, DeskError> {
        let query = format!("SELECT {COLUMNS} FROM rules WHERE tenant_id = $1 AND id = $2");
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Returns the enabled rules a run of this agent must follow, global ones first.
    pub async fn active_rules(&self, agent_id: &str) -> Result<Vec<Rule>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM rules WHERE tenant_id = $1 AND enabled \
             AND (agent_id IS NULL OR agent_id = $2) \
             ORDER BY agent_id NULLS FIRST, created_at, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(agent_id)
            .fetch_all(self.pool())
            .await?)
    }

    /// Like [`Desk::active_rules`], for a run that is about to start: counts
    /// each rule as applied.
    pub async fn apply_rules(&self, agent_id: &str) -> Result<Vec<Rule>, DeskError> {
        let query = format!(
            "UPDATE rules SET applied_count = applied_count + 1, last_applied_at = now() \
             WHERE tenant_id = $1 AND enabled AND (agent_id IS NULL OR agent_id = $2) \
             RETURNING {COLUMNS}"
        );
        let mut rules: Vec<Rule> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(agent_id)
            .fetch_all(self.pool())
            .await?;
        rules.sort_by(|left, right| {
            (left.agent_id.is_some(), left.created_at, left.id).cmp(&(
                right.agent_id.is_some(),
                right.created_at,
                right.id,
            ))
        });
        Ok(rules)
    }

    /// Changes a rule. Returns `None` for an unknown rule.
    pub async fn update_rule(&self, id: Uuid, patch: RulePatch) -> Result<Option<Rule>, DeskError> {
        let Some(mut rule) = self.rule(id).await? else {
            return Ok(None);
        };
        if let Some(text) = patch.text {
            rule.text = required_text("text", &text, MAX_TEXT)?;
        }
        if let Some(agent) = patch.agent_id {
            rule.agent_id = check_agent(agent)?;
        }
        if let Some(category) = patch.category {
            rule.category = check_category(category)?;
        }
        if let Some(enabled) = patch.enabled {
            rule.enabled = enabled;
        }
        let query = format!(
            "UPDATE rules SET text = $3, agent_id = $4, category = $5, enabled = $6, \
             updated_at = now() WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(&rule.text)
            .bind(&rule.agent_id)
            .bind(&rule.category)
            .bind(rule.enabled)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Deletes a rule. Returns whether it existed.
    pub async fn delete_rule(&self, id: Uuid) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM rules WHERE tenant_id = $1 AND id = $2")
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

    fn new(text: &str, agent: Option<&str>) -> NewRule {
        NewRule {
            text: text.to_owned(),
            agent_id: agent.map(str::to_owned),
            category: None,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn active_rules_are_enabled_global_or_for_the_agent() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        let global = desk
            .create_rule(new("Always ask before booking", None))
            .await
            .unwrap();
        desk.create_rule(new("Keep briefs short", Some("morning-brief")))
            .await
            .unwrap();
        desk.create_rule(new("Only for others", Some("weekly-report")))
            .await
            .unwrap();
        let texts = |rules: Vec<Rule>| rules.into_iter().map(|r| r.text).collect::<Vec<_>>();
        assert_eq!(
            texts(desk.active_rules("morning-brief").await.unwrap()),
            ["Always ask before booking", "Keep briefs short"]
        );

        let off = RulePatch {
            enabled: Some(false),
            ..RulePatch::default()
        };
        desk.update_rule(global.id, off).await.unwrap().unwrap();
        assert_eq!(
            texts(desk.active_rules("morning-brief").await.unwrap()),
            ["Keep briefs short"]
        );
        assert_eq!(desk.rules().await.unwrap().len(), 3);
        assert!(
            desk.rules()
                .await
                .unwrap()
                .iter()
                .all(|rule| rule.applied_count == 0)
        );
        let applied = desk.apply_rules("morning-brief").await.unwrap();
        assert_eq!(
            texts(applied.clone()),
            texts(desk.active_rules("morning-brief").await.unwrap())
        );
        assert!(
            applied
                .iter()
                .all(|rule| rule.applied_count == 1 && rule.last_applied_at.is_some())
        );
    }

    #[tokio::test]
    async fn rules_are_validated_changed_and_deleted() {
        let desk = Desk::new(fresh_store().await, TenantId::SINGLE);
        assert!(desk.create_rule(new(" ", None)).await.is_err());
        assert!(desk.create_rule(new("x", Some("Bad ID"))).await.is_err());
        let rule = desk
            .create_rule(NewRule {
                category: Some(" work ".to_owned()),
                ..new("Write in English", Some(""))
            })
            .await
            .unwrap();
        assert_eq!(rule.agent_id, None);
        assert_eq!(rule.category.as_deref(), Some("work"));

        let patch: RulePatch = serde_json::from_str(
            r#"{"text": "Write in plain English", "agent_id": "inbox", "category": null}"#,
        )
        .unwrap();
        let changed = desk.update_rule(rule.id, patch).await.unwrap().unwrap();
        assert_eq!(changed.text, "Write in plain English");
        assert_eq!(changed.agent_id.as_deref(), Some("inbox"));
        assert_eq!(changed.category, None);
        assert!(
            desk.update_rule(Uuid::nil(), RulePatch::default())
                .await
                .unwrap()
                .is_none()
        );
        assert!(desk.delete_rule(rule.id).await.unwrap());
        assert!(!desk.delete_rule(rule.id).await.unwrap());
        assert!(desk.rule(rule.id).await.unwrap().is_none());
    }
}
