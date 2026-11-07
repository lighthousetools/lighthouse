//! Splits: saved filters of the connected lists, such as "Customers" or "Newsletters".

use crate::store::{ItemFilter, Order, SyncStore};
use lighthouse_core::agent::slugify;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What a split shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct SplitFilter {
    /// Full-text search.
    pub q: Option<String>,
    /// Item kinds, such as `thread`.
    pub kinds: Vec<String>,
    /// Only this source.
    pub source: Option<String>,
    /// Only sources of this organization.
    pub org: Option<String>,
    /// Flags the items must all have.
    pub flags: Vec<String>,
    /// Flags of which the items must have one.
    pub any_flags: Vec<String>,
    /// Only this state.
    pub state: Option<String>,
    /// Leave out closed and done items.
    pub open: bool,
}

/// A split.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Split {
    /// Split ID.
    pub id: String,
    /// Name shown in the apps.
    pub name: String,
    /// Domain, such as `mail`.
    pub domain: Option<String>,
    /// What it shows.
    pub filter: SplitFilter,
    /// Sort key, lowest first.
    pub position: i32,
}

/// A split to create or replace.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct SplitInput {
    /// Name.
    pub name: String,
    /// Domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// What it shows.
    #[serde(default)]
    pub filter: SplitFilter,
    /// Sort key.
    #[serde(default)]
    pub position: i32,
}

impl Split {
    /// Returns the item filter of a page of this split.
    pub fn item_filter(&self, limit: i64, offset: i64) -> ItemFilter {
        ItemFilter {
            domain: self.domain.clone(),
            kinds: self.filter.kinds.clone(),
            source: self.filter.source.clone(),
            org: self.filter.org.clone(),
            q: self.filter.q.clone(),
            flags: self.filter.flags.clone(),
            any_flags: self.filter.any_flags.clone(),
            state: self.filter.state.clone(),
            open: self.filter.open,
            order: Order::Recent,
            limit,
            offset,
            ..ItemFilter::default()
        }
    }
}

type SplitRow = (
    String,
    String,
    Option<String>,
    sqlx::types::Json<SplitFilter>,
    i32,
);

fn split(row: SplitRow) -> Split {
    Split {
        id: row.0,
        name: row.1,
        domain: row.2,
        filter: row.3.0,
        position: row.4,
    }
}

impl SyncStore {
    /// Lists the splits.
    pub async fn splits(&self) -> Result<Vec<Split>, sqlx::Error> {
        let rows: Vec<SplitRow> = sqlx::query_as(
            "SELECT id, name, domain, filter, position FROM saved_filters WHERE tenant_id = $1 \
             ORDER BY position, name",
        )
        .bind(self.tenant_id())
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(split).collect())
    }

    /// Returns a split.
    pub async fn split(&self, id: &str) -> Result<Option<Split>, sqlx::Error> {
        let row: Option<SplitRow> = sqlx::query_as(
            "SELECT id, name, domain, filter, position FROM saved_filters WHERE tenant_id = $1 AND id = $2",
        )
        .bind(self.tenant_id())
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(split))
    }

    /// Creates or replaces a split. Returns `None` for an invalid ID or an empty name.
    pub async fn save_split(
        &self,
        id: &str,
        input: SplitInput,
    ) -> Result<Option<Split>, sqlx::Error> {
        if id.is_empty() || slugify(id) != id || input.name.trim().is_empty() {
            return Ok(None);
        }
        let row: SplitRow = sqlx::query_as(
            "INSERT INTO saved_filters (id, tenant_id, name, domain, filter, position) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (tenant_id, id) DO UPDATE SET \
             name = $3, domain = $4, filter = $5, position = $6 \
             RETURNING id, name, domain, filter, position",
        )
        .bind(id)
        .bind(self.tenant_id())
        .bind(input.name.trim())
        .bind(&input.domain)
        .bind(sqlx::types::Json(&input.filter))
        .bind(input.position)
        .fetch_one(self.pool())
        .await?;
        Ok(Some(split(row)))
    }

    /// Deletes a split. Returns whether it existed.
    pub async fn delete_split(&self, id: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM saved_filters WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant_id())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
