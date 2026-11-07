//! The cache in Postgres.

use crate::mapping::{CLOSED_STATES, MappedItem};
use chrono::{DateTime, Utc};
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

const COLUMNS: &str = "id, source, domain, kind, external_id, parent_id, title, summary, author, assignee, \
                       state, url, flags, importance, occurred_at, starts_at, ends_at, amount, currency, \
                       data, synced_at, snoozed_until, \
                       (SELECT o.id FROM organizations o WHERE o.tenant_id = synced_items.tenant_id \
                        AND synced_items.source = ANY(o.sources) ORDER BY o.id LIMIT 1) AS org";

/// A cached item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct SyncedItem {
    /// Item ID in Lighthouse.
    pub id: Uuid,
    /// Source name from the configuration.
    pub source: String,
    /// `mail`, `calendar`, `issues` or `code`.
    pub domain: String,
    /// Kind, such as `thread`, `event`, `issue`, `pull_request` or `check`.
    pub kind: String,
    /// ID in the service.
    pub external_id: String,
    /// ID of the parent item in the service, such as the pull request of a check.
    pub parent_id: Option<String>,
    /// Title or subject.
    pub title: Option<String>,
    /// Snippet, description or body.
    pub summary: Option<String>,
    /// Author or sender.
    pub author: Option<String>,
    /// Assignee.
    pub assignee: Option<String>,
    /// State in the service.
    pub state: Option<String>,
    /// Address to open it in the service.
    pub url: Option<String>,
    /// Flags such as `important`, `unread`, `assigned`, `review_requested`, `mine`, `mention` or `failed`.
    pub flags: Vec<String>,
    /// How much it asks for attention, from 0 to 100.
    pub importance: i16,
    /// When it last changed or arrived.
    pub occurred_at: Option<DateTime<Utc>>,
    /// Start, for events and cycles.
    pub starts_at: Option<DateTime<Utc>>,
    /// End, for events and cycles. The due date of an invoice, the expiry of a contract.
    pub ends_at: Option<DateTime<Utc>>,
    /// Value of a deal, balance, transaction, invoice or holding.
    pub amount: Option<f64>,
    /// Currency code.
    pub currency: Option<String>,
    /// Every mapped field.
    pub data: Value,
    /// When it was fetched.
    pub synced_at: DateTime<Utc>,
    /// Hidden until this time.
    pub snoozed_until: Option<DateTime<Utc>>,
    /// The organization whose sources include this item's source.
    pub org: Option<String>,
}

impl SyncedItem {
    /// Returns the item as a template context: `external_id`, `title`, `data.<field>` and so on.
    pub fn context(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// How to sort a list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// Latest change first.
    #[default]
    Recent,
    /// Most important first.
    Importance,
    /// Earliest start first.
    Start,
}

/// Which items to list.
#[derive(Clone, Debug, Default)]
pub struct ItemFilter {
    /// Only this domain.
    pub domain: Option<String>,
    /// Only these kinds.
    pub kinds: Vec<String>,
    /// Only this source.
    pub source: Option<String>,
    /// Only sources of this organization.
    pub org: Option<String>,
    /// Also items snoozed until later.
    pub include_snoozed: bool,
    /// Full-text search.
    pub q: Option<String>,
    /// Items must have all of these flags.
    pub flags: Vec<String>,
    /// Items must have at least one of these flags.
    pub any_flags: Vec<String>,
    /// Only this state, ignoring case.
    pub state: Option<String>,
    /// Leave out closed, merged, done and similar states.
    pub open: bool,
    /// Only children of this parent.
    pub parent: Option<String>,
    /// Items that end or happened at or after this time.
    pub from: Option<DateTime<Utc>>,
    /// Items that start or happened before this time.
    pub to: Option<DateTime<Utc>>,
    /// Sort order.
    pub order: Order,
    /// Page size.
    pub limit: i64,
    /// Items to skip.
    pub offset: i64,
}

/// A page of items.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ItemPage {
    /// Items on this page.
    pub items: Vec<SyncedItem>,
    /// Matching items in total.
    pub total: i64,
    /// Offset for the next page, if there is one.
    pub next_offset: Option<i64>,
}

/// Freshness of a source.
#[derive(Clone, Debug, Serialize, Deserialize, FromRow, ToSchema)]
pub struct SyncStatus {
    /// Source name.
    pub source: String,
    /// Last refresh attempt.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// Last successful refresh.
    pub last_success_at: Option<DateTime<Utc>>,
    /// Error of the last attempt, if it failed.
    pub last_error: Option<String>,
    /// Items fetched by the last successful refresh.
    pub items: i32,
}

/// A stored item and whether it is new.
#[derive(Clone, Debug)]
pub struct Stored {
    /// Item ID.
    pub id: Uuid,
    /// ID in the service.
    pub external_id: String,
    /// Whether the cache did not have it before.
    pub inserted: bool,
    /// Its flags before this refresh.
    pub previous_flags: Vec<String>,
}

/// The cache of one tenant.
#[derive(Clone, Debug)]
pub struct SyncStore {
    store: Store,
    tenant: TenantId,
}

fn closed_states() -> Vec<String> {
    CLOSED_STATES
        .iter()
        .map(|state| (*state).to_owned())
        .collect()
}

impl SyncStore {
    /// Creates the cache.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self { store, tenant }
    }

    pub(crate) fn pool(&self) -> &sqlx::PgPool {
        self.store.pool()
    }

    pub(crate) fn tenant_id(&self) -> Uuid {
        self.tenant.as_uuid()
    }

    /// Stores items of one kind and reports which are new.
    ///
    /// `managed` flags are the ones this refresh decides. Other flags of an
    /// item, set by other collections, stay.
    pub async fn upsert(
        &self,
        source: &str,
        domain: &str,
        kind: &str,
        items: &[MappedItem],
        managed: &[String],
    ) -> Result<Vec<Stored>, sqlx::Error> {
        let ids: Vec<String> = items.iter().map(|item| item.external_id.clone()).collect();
        let rows: Vec<(String, Vec<String>)> = sqlx::query_as(
            "SELECT external_id, flags FROM synced_items WHERE tenant_id = $1 AND source = $2 \
             AND kind = $3 AND external_id = ANY($4)",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .bind(&ids)
        .fetch_all(self.pool())
        .await?;
        let existing: HashMap<String, Vec<String>> = rows.into_iter().collect();
        let mut transaction = self.pool().begin().await?;
        let mut stored = Vec::new();
        for item in items {
            let previous = existing.get(&item.external_id).cloned();
            let mut flags: Vec<String> = previous
                .iter()
                .flatten()
                .filter(|flag| !managed.contains(flag))
                .cloned()
                .chain(item.flags.iter().cloned())
                .collect();
            flags.sort();
            flags.dedup();
            let (id,): (Uuid,) = sqlx::query_as(
                "INSERT INTO synced_items (id, tenant_id, source, domain, kind, external_id, parent_id, \
                 title, summary, author, assignee, state, url, flags, importance, occurred_at, \
                 starts_at, ends_at, data, raw, amount, currency) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
                 $18, $19, $20, $21, $22) \
                 ON CONFLICT (tenant_id, source, kind, external_id) DO UPDATE SET \
                 parent_id = EXCLUDED.parent_id, title = EXCLUDED.title, summary = EXCLUDED.summary, \
                 author = EXCLUDED.author, assignee = EXCLUDED.assignee, state = EXCLUDED.state, \
                 url = EXCLUDED.url, flags = EXCLUDED.flags, \
                 importance = EXCLUDED.importance, \
                 occurred_at = EXCLUDED.occurred_at, starts_at = EXCLUDED.starts_at, \
                 ends_at = EXCLUDED.ends_at, data = EXCLUDED.data, raw = EXCLUDED.raw, \
                 amount = EXCLUDED.amount, currency = EXCLUDED.currency, synced_at = now() \
                 RETURNING id",
            )
            .bind(Uuid::now_v7())
            .bind(self.tenant.as_uuid())
            .bind(source)
            .bind(domain)
            .bind(kind)
            .bind(&item.external_id)
            .bind(&item.parent_id)
            .bind(&item.title)
            .bind(&item.summary)
            .bind(&item.author)
            .bind(&item.assignee)
            .bind(&item.state)
            .bind(&item.url)
            .bind(&flags)
            .bind(importance_of(&flags, item))
            .bind(item.occurred_at)
            .bind(item.starts_at)
            .bind(item.ends_at)
            .bind(&item.data)
            .bind(&item.raw)
            .bind(item.amount)
            .bind(&item.currency)
            .fetch_one(&mut *transaction)
            .await?;
            stored.push(Stored {
                id,
                external_id: item.external_id.clone(),
                inserted: previous.is_none(),
                previous_flags: previous.unwrap_or_default(),
            });
        }
        transaction.commit().await?;
        Ok(stored)
    }

    /// Notes that a collection returned these items.
    pub async fn mark_seen(
        &self,
        source: &str,
        kind: &str,
        collection: &str,
        ids: &[String],
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE synced_items SET seen_by = array_append(seen_by, $4) WHERE tenant_id = $1 \
             AND source = $2 AND kind = $3 AND external_id = ANY($5) AND NOT ($4 = ANY(seen_by))",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .bind(collection)
        .bind(ids)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// After a full refresh of a collection, forgets it on items it did not
    /// return and removes items no collection returns anymore, with their
    /// children. Returns the IDs of removed items.
    pub async fn prune_vanished(
        &self,
        source: &str,
        kind: &str,
        collection: &str,
        seen: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        let mut transaction = self.pool().begin().await?;
        sqlx::query(
            "UPDATE synced_items SET seen_by = array_remove(seen_by, $4) WHERE tenant_id = $1 \
             AND source = $2 AND kind = $3 AND NOT (external_id = ANY($5)) AND $4 = ANY(seen_by)",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .bind(collection)
        .bind(seen)
        .execute(&mut *transaction)
        .await?;
        let removed: Vec<(String,)> = sqlx::query_as(
            "DELETE FROM synced_items WHERE tenant_id = $1 AND source = $2 AND kind = $3 \
             AND cardinality(seen_by) = 0 AND NOT (external_id = ANY($4)) RETURNING external_id",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .bind(seen)
        .fetch_all(&mut *transaction)
        .await?;
        let removed: Vec<String> = removed.into_iter().map(|(id,)| id).collect();
        sqlx::query(
            "DELETE FROM synced_items WHERE tenant_id = $1 AND source = $2 AND parent_id = ANY($3)",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(&removed)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(removed)
    }

    /// Hides an item until a time, or shows it again with `None`. Returns whether it exists.
    pub async fn snooze(
        &self,
        id: Uuid,
        until: Option<DateTime<Utc>>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE synced_items SET snoozed_until = $3 WHERE tenant_id = $1 AND id = $2",
        )
        .bind(self.tenant.as_uuid())
        .bind(id)
        .bind(until)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Removes flags from items of a kind that a full refresh did not return.
    pub async fn prune_flags(
        &self,
        source: &str,
        kind: &str,
        seen: &[String],
        flags: &[String],
    ) -> Result<u64, sqlx::Error> {
        if flags.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE synced_items SET flags = ARRAY(SELECT unnest(flags) EXCEPT SELECT unnest($5::text[]) \
             ORDER BY 1), synced_at = now() \
             WHERE tenant_id = $1 AND source = $2 AND kind = $3 AND NOT (external_id = ANY($4)) \
             AND flags && $5",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .bind(seen)
        .bind(flags)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Returns recently changed items of a kind, to fetch their children.
    pub async fn parents(
        &self,
        source: &str,
        kind: &str,
        limit: i64,
    ) -> Result<Vec<SyncedItem>, sqlx::Error> {
        let query = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND source = $2 AND kind = $3 \
             AND (state IS NULL OR lower(state) <> ALL($4)) \
             ORDER BY occurred_at DESC NULLS LAST, synced_at DESC LIMIT $5"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(source)
            .bind(kind)
            .bind(closed_states())
            .bind(limit)
            .fetch_all(self.pool())
            .await
    }

    /// Lists items.
    pub async fn items(&self, filter: &ItemFilter) -> Result<ItemPage, sqlx::Error> {
        let condition = "tenant_id = $1 AND ($2::text IS NULL OR domain = $2) \
             AND (cardinality($3::text[]) = 0 OR kind = ANY($3)) \
             AND ($4::text IS NULL OR source = $4) \
             AND ($5::text IS NULL OR search @@ websearch_to_tsquery('simple', $5)) \
             AND flags @> $6::text[] \
             AND (cardinality($7::text[]) = 0 OR flags && $7) \
             AND ($8::text IS NULL OR lower(state) = lower($8)) \
             AND (NOT $9 OR state IS NULL OR lower(state) <> ALL($10)) \
             AND ($11::text IS NULL OR parent_id = $11) \
             AND ($12::timestamptz IS NULL OR coalesce(ends_at, starts_at, occurred_at) >= $12) \
             AND ($13::timestamptz IS NULL OR coalesce(starts_at, occurred_at) < $13) \
             AND ($16::text IS NULL OR source IN (SELECT unnest(o.sources) FROM organizations o \
                  WHERE o.tenant_id = $1 AND o.id = $16)) \
             AND ($17 OR snoozed_until IS NULL OR snoozed_until <= now())";
        let order = match filter.order {
            Order::Recent => "occurred_at DESC NULLS LAST, synced_at DESC, id",
            Order::Importance => "importance DESC, occurred_at DESC NULLS LAST, id",
            Order::Start => "starts_at NULLS LAST, id",
        };
        let select = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE {condition} ORDER BY {order} LIMIT $14 OFFSET $15"
        );
        let count = format!(
            "SELECT count(*) FROM synced_items WHERE {condition} \
             AND $14::bigint IS NOT NULL AND $15::bigint IS NOT NULL"
        );
        macro_rules! bind_filter {
            ($query:expr) => {
                $query
                    .bind(self.tenant.as_uuid())
                    .bind(&filter.domain)
                    .bind(&filter.kinds)
                    .bind(&filter.source)
                    .bind(filter.q.as_deref().filter(|q| !q.trim().is_empty()))
                    .bind(&filter.flags)
                    .bind(&filter.any_flags)
                    .bind(&filter.state)
                    .bind(filter.open)
                    .bind(closed_states())
                    .bind(&filter.parent)
                    .bind(filter.from)
                    .bind(filter.to)
            };
        }
        macro_rules! bind_page {
            ($query:expr) => {
                bind_filter!($query)
                    .bind(filter.limit)
                    .bind(filter.offset)
                    .bind(&filter.org)
                    .bind(filter.include_snoozed)
            };
        }
        let items: Vec<SyncedItem> = bind_page!(sqlx::query_as(sqlx::AssertSqlSafe(select)))
            .fetch_all(self.pool())
            .await?;
        let (total,): (i64,) = bind_page!(sqlx::query_as(sqlx::AssertSqlSafe(count)))
            .fetch_one(self.pool())
            .await?;
        let next = filter.offset + items.len() as i64;
        Ok(ItemPage {
            next_offset: (next < total).then_some(next),
            items,
            total,
        })
    }

    /// Returns an item.
    pub async fn item(&self, id: Uuid) -> Result<Option<SyncedItem>, sqlx::Error> {
        let query = format!("SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND id = $2");
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(id)
            .fetch_optional(self.pool())
            .await
    }

    /// Returns an item by its ID in the service.
    pub async fn item_by_external(
        &self,
        source: &str,
        kind: &str,
        external_id: &str,
    ) -> Result<Option<SyncedItem>, sqlx::Error> {
        let query = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND source = $2 AND kind = $3 \
             AND external_id = $4"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(source)
            .bind(kind)
            .bind(external_id)
            .fetch_optional(self.pool())
            .await
    }

    /// Counts the cached items of a kind in a source.
    pub async fn count(&self, source: &str, kind: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM synced_items WHERE tenant_id = $1 AND source = $2 AND kind = $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(kind)
        .fetch_one(self.pool())
        .await?;
        Ok(row.0)
    }

    /// Finds an item by its external ID, within one source or in any, the
    /// latest first.
    pub async fn find_external(
        &self,
        external_id: &str,
        source: Option<&str>,
    ) -> Result<Option<SyncedItem>, sqlx::Error> {
        let query = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND external_id = $2 \
             AND ($3::text IS NULL OR source = $3) ORDER BY occurred_at DESC NULLS LAST LIMIT 1"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(external_id)
            .bind(source)
            .fetch_optional(self.pool())
            .await
    }

    /// Returns won deals like this one, latest first: of the same source, and of
    /// the same `pipeline` or `category` field when the deal has one.
    pub async fn similar_won_deals(
        &self,
        deal: &SyncedItem,
        limit: i64,
    ) -> Result<Vec<SyncedItem>, sqlx::Error> {
        let (field, value) = ["pipeline", "category"]
            .iter()
            .find_map(|field| {
                deal.data
                    .get(*field)
                    .and_then(crate::mapping::text_of)
                    .map(|value| ((*field).to_owned(), value))
            })
            .map_or((None, None), |(field, value)| (Some(field), Some(value)));
        let query = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND source = $2 AND kind = 'deal' \
             AND lower(state) = 'won' AND amount IS NOT NULL AND id <> $3 \
             AND ($4::text IS NULL OR data->>$4 = $5) \
             ORDER BY occurred_at DESC NULLS LAST LIMIT $6"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(&deal.source)
            .bind(deal.id)
            .bind(field)
            .bind(value)
            .bind(limit)
            .fetch_all(self.pool())
            .await
    }

    /// Returns the item as the tool returned it.
    pub async fn raw(&self, id: Uuid) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<(Option<Value>,)> =
            sqlx::query_as("SELECT raw FROM synced_items WHERE tenant_id = $1 AND id = $2")
                .bind(self.tenant.as_uuid())
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.and_then(|(raw,)| raw))
    }

    /// Returns the children of an item, oldest first.
    pub async fn children(&self, item: &SyncedItem) -> Result<Vec<SyncedItem>, sqlx::Error> {
        let query = format!(
            "SELECT {COLUMNS} FROM synced_items WHERE tenant_id = $1 AND source = $2 AND parent_id = $3 \
             ORDER BY kind, occurred_at NULLS LAST, external_id"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant.as_uuid())
            .bind(&item.source)
            .bind(&item.external_id)
            .fetch_all(self.pool())
            .await
    }

    /// Records a refresh attempt.
    pub async fn record_sync(
        &self,
        source: &str,
        outcome: Result<usize, &str>,
    ) -> Result<(), sqlx::Error> {
        let (items, error) = match outcome {
            Ok(items) => (i32::try_from(items).unwrap_or(i32::MAX), None),
            Err(error) => (0, Some(error)),
        };
        sqlx::query(
            "INSERT INTO sync_state (tenant_id, source, last_attempt_at, last_success_at, last_error, items) \
             VALUES ($1, $2, now(), CASE WHEN $3::text IS NULL THEN now() END, $3, $4) \
             ON CONFLICT (tenant_id, source) DO UPDATE SET last_attempt_at = now(), \
             last_success_at = CASE WHEN $3::text IS NULL THEN now() ELSE sync_state.last_success_at END, \
             last_error = $3, \
             items = CASE WHEN $3::text IS NULL THEN $4 ELSE sync_state.items END",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(error)
        .bind(items)
        .execute(self.pool())
        .await?;
        sqlx::query(
            "INSERT INTO sync_history (id, tenant_id, source, ok, items, error) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(error.is_none())
        .bind(items)
        .bind(error)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Counts refresh attempts and failures of a source per day of the time
    /// zone, oldest first, with a row for every day.
    pub async fn daily_syncs(
        &self,
        source: &str,
        days: i32,
        zone: &str,
    ) -> Result<Vec<(String, i64, i64)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT to_char(day, 'YYYY-MM-DD'), count(h.id), count(h.id) FILTER (WHERE NOT h.ok) \
             FROM generate_series(date_trunc('day', now() AT TIME ZONE $4) - make_interval(days => $3 - 1), \
                                  date_trunc('day', now() AT TIME ZONE $4), interval '1 day') AS day \
             LEFT JOIN sync_history h ON h.tenant_id = $1 AND h.source = $2 \
                  AND (h.at AT TIME ZONE $4) >= day AND (h.at AT TIME ZONE $4) < day + interval '1 day' \
             GROUP BY day ORDER BY day",
        )
        .bind(self.tenant.as_uuid())
        .bind(source)
        .bind(days)
        .bind(zone)
        .fetch_all(self.pool())
        .await
    }

    /// Returns the freshness of every source that was ever refreshed.
    pub async fn statuses(&self) -> Result<Vec<SyncStatus>, sqlx::Error> {
        sqlx::query_as(
            "SELECT source, last_attempt_at, last_success_at, last_error, items FROM sync_state \
             WHERE tenant_id = $1 ORDER BY source",
        )
        .bind(self.tenant.as_uuid())
        .fetch_all(self.pool())
        .await
    }

    /// Returns busy blocks of calendar events between two times, merged.
    pub async fn busy(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>, sqlx::Error> {
        let rows: Vec<(DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT starts_at, coalesce(ends_at, starts_at) FROM synced_items \
             WHERE tenant_id = $1 AND domain = 'calendar' AND kind = 'event' \
             AND starts_at IS NOT NULL AND starts_at < $3 AND coalesce(ends_at, starts_at) > $2 \
             AND (state IS NULL OR lower(state) NOT IN ('cancelled', 'canceled', 'declined', 'free')) \
             AND NOT flags @> ARRAY['free'] ORDER BY starts_at",
        )
        .bind(self.tenant.as_uuid())
        .bind(from)
        .bind(to)
        .fetch_all(self.pool())
        .await?;
        let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
        for (start, end) in rows {
            let start = start.max(from);
            let end = end.min(to);
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Ok(merged)
    }
}

fn importance_of(flags: &[String], item: &MappedItem) -> i16 {
    let set = flags.iter().cloned().collect();
    crate::mapping::importance(&set, item.occurred_at, Utc::now())
}
