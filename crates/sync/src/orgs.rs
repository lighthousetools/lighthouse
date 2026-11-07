//! Organizations: the companies the owner runs.
//!
//! An organization names the sources that belong to it. Every cached item of
//! such a source carries the organization, and lists can be narrowed to one.

use crate::store::SyncStore;
use chrono::{DateTime, Utc};
use lighthouse_core::agent::slugify;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// An organization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Organization {
    /// Stable ID of lowercase letters, digits and dashes.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Color for the apps, as `#rrggbb`.
    pub color: Option<String>,
    /// Sources that belong to it.
    pub sources: Vec<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
}

/// An organization to create or replace.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct OrganizationInput {
    /// Display name.
    pub name: String,
    /// Color as `#rrggbb`.
    #[serde(default)]
    pub color: Option<String>,
    /// Sources that belong to it.
    #[serde(default)]
    pub sources: Vec<String>,
}

/// Why an organization is invalid.
#[derive(Debug, thiserror::Error)]
pub enum OrganizationError {
    /// The input is invalid. The text is safe to show.
    #[error("{0}")]
    Invalid(String),
    /// The database failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

const COLUMNS: &str = "id, name, color, sources, created_at, updated_at";

impl SyncStore {
    /// Lists the organizations by name.
    pub async fn organizations(&self) -> Result<Vec<Organization>, sqlx::Error> {
        let query =
            format!("SELECT {COLUMNS} FROM organizations WHERE tenant_id = $1 ORDER BY name, id");
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant_id())
            .fetch_all(self.pool())
            .await
    }

    /// Creates or replaces an organization.
    pub async fn save_organization(
        &self,
        id: &str,
        input: OrganizationInput,
    ) -> Result<Organization, OrganizationError> {
        if id.is_empty() || slugify(id) != id {
            return Err(OrganizationError::Invalid(
                "the ID must be lowercase letters, digits and dashes".to_owned(),
            ));
        }
        let name = input.name.trim();
        if name.is_empty() {
            return Err(OrganizationError::Invalid(
                "`name` must not be empty".to_owned(),
            ));
        }
        if input
            .color
            .as_deref()
            .is_some_and(|color| !valid_color(color))
        {
            return Err(OrganizationError::Invalid(
                "`color` must look like #1a2b3c".to_owned(),
            ));
        }
        let mut sources = input.sources;
        sources.sort();
        sources.dedup();
        let query = format!(
            "INSERT INTO organizations (id, tenant_id, name, color, sources) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant_id, id) DO UPDATE SET name = $3, color = $4, sources = $5, \
             updated_at = now() RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(id)
            .bind(self.tenant_id())
            .bind(name)
            .bind(&input.color)
            .bind(&sources)
            .fetch_one(self.pool())
            .await?)
    }

    /// Deletes an organization. Its items stay and lose the organization.
    pub async fn delete_organization(&self, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM organizations WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant_id())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
