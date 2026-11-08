//! Posts: drafts, the posting plan, pillars and the archive of published posts.
//!
//! Lighthouse never publishes by itself. A post goes out only when the owner
//! asks for it and approves the call. Analytics come from the owner or from an
//! import, never from scraping.

use crate::{Desk, DeskError, double_option, invalid, required_text};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::FromRow;
use std::collections::HashSet;
use utoipa::ToSchema;
use uuid::Uuid;

const COLUMNS: &str = "id, org, channel, kind, pillar, topic, title, body, status, scheduled_at, published_at, \
                       url, external_id, images, metrics, origin, publish_run, created_at, updated_at, \
                       variants, first_comment, reason, \
                       (CASE WHEN status = 'published' AND metrics->>'impressions' ~ '^[0-9]+(\\.[0-9]+)?$' \
                        THEN (metrics->>'impressions')::float8 / NULLIF((SELECT percentile_cont(0.5) \
                        WITHIN GROUP (ORDER BY (p.metrics->>'impressions')::float8) FROM posts p \
                        WHERE p.tenant_id = posts.tenant_id AND p.channel = posts.channel \
                        AND p.status = 'published' AND p.metrics->>'impressions' ~ '^[0-9]+(\\.[0-9]+)?$'), 0) \
                        END) AS x_factor";
const MAX_BODY: usize = 10_000;
/// Analytics fields an import or the owner may set.
pub const METRICS: [&str; 7] = [
    "impressions",
    "reactions",
    "comments",
    "shares",
    "clicks",
    "followers",
    "icp_share",
];
/// States of a post.
pub const STATUSES: [&str; 5] = ["idea", "draft", "scheduled", "publishing", "published"];

/// A post.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Post {
    /// Post ID.
    pub id: Uuid,
    /// Organization it is for, if any.
    pub org: Option<String>,
    /// Channel, such as `linkedin`.
    pub channel: String,
    /// Free kind, such as `opinion` or `report`.
    pub kind: Option<String>,
    /// Content pillar.
    pub pillar: Option<String>,
    /// Topic within the pillar.
    pub topic: Option<String>,
    /// Working title.
    pub title: Option<String>,
    /// The text.
    pub body: String,
    /// `idea`, `draft`, `scheduled`, `publishing` or `published`.
    pub status: String,
    /// Planned time.
    pub scheduled_at: Option<DateTime<Utc>>,
    /// When it went out.
    pub published_at: Option<DateTime<Utc>>,
    /// Address of the published post.
    pub url: Option<String>,
    /// ID in the channel.
    pub external_id: Option<String>,
    /// Images, as IDs of Brain entries.
    pub images: Vec<Uuid>,
    /// Analytics such as `impressions`, `reactions`, `comments`, `shares`, `clicks`.
    pub metrics: Value,
    /// `owner` or `import`.
    pub origin: String,
    /// The run that waits for approval to publish it.
    pub publish_run: Option<Uuid>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// Alternative texts, each with a `name` and a `text`.
    pub variants: Value,
    /// The comment to post first, for example with the link.
    pub first_comment: Option<String>,
    /// Why an agent proposed it.
    pub reason: Option<String>,
    /// For a published post with `impressions`: how it did against the median
    /// of published posts on the same channel, such as `2.0` for twice as many.
    pub x_factor: Option<f64>,
}

/// An alternative text of a post.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Variant {
    /// Short name, such as `shorter` or `story`.
    pub name: String,
    /// The text.
    pub text: String,
}

fn variants_value(variants: &[Variant]) -> Result<Value, DeskError> {
    if variants.len() > 10 {
        return Err(invalid("at most 10 variants"));
    }
    for variant in variants {
        required_text("name", &variant.name, 100)?;
        check_body(&variant.text)?;
    }
    Ok(serde_json::to_value(variants).unwrap_or(Value::Array(Vec::new())))
}

/// A post to create.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct NewPost {
    /// The text.
    #[serde(default)]
    pub body: String,
    /// Working title.
    #[serde(default)]
    pub title: Option<String>,
    /// Organization.
    #[serde(default)]
    pub org: Option<String>,
    /// Channel. Defaults to `linkedin`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Free kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// Pillar.
    #[serde(default)]
    pub pillar: Option<String>,
    /// Topic.
    #[serde(default)]
    pub topic: Option<String>,
    /// `idea` or `draft`. Defaults to `draft`.
    #[serde(default)]
    pub status: Option<String>,
    /// Planned time. Makes the post `scheduled`.
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
    /// Images as Brain entry IDs.
    #[serde(default)]
    pub images: Vec<Uuid>,
    /// Alternative texts.
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// The comment to post first.
    #[serde(default)]
    pub first_comment: Option<String>,
    /// Why an agent proposes it.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Changes to a post. Missing fields stay, `null` clears.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct PostPatch {
    /// New text.
    #[serde(default)]
    pub body: Option<String>,
    /// New title.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub title: Option<Option<String>>,
    /// New organization.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub org: Option<Option<String>>,
    /// New kind.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub kind: Option<Option<String>>,
    /// New pillar.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub pillar: Option<Option<String>>,
    /// New topic.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub topic: Option<Option<String>>,
    /// `idea` or `draft`. Use `scheduled_at` to schedule.
    #[serde(default)]
    pub status: Option<String>,
    /// New planned time. `null` unschedules.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<DateTime<Utc>>)]
    pub scheduled_at: Option<Option<DateTime<Utc>>>,
    /// New images.
    #[serde(default)]
    pub images: Option<Vec<Uuid>>,
    /// Analytics to merge, such as `{"impressions": 1200}`.
    #[serde(default)]
    pub metrics: Option<Value>,
    /// Address, once published elsewhere.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub url: Option<Option<String>>,
    /// New alternative texts, replacing the old ones.
    #[serde(default)]
    pub variants: Option<Vec<Variant>>,
    /// New first comment.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub first_comment: Option<Option<String>>,
}

/// Which posts to list.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PostFilter {
    /// Only this status.
    pub status: Option<String>,
    /// Only this organization.
    pub org: Option<String>,
    /// Only this pillar.
    pub pillar: Option<String>,
    /// Text search in title and body.
    pub q: Option<String>,
    /// Scheduled or published at or after.
    pub from: Option<DateTime<Utc>>,
    /// Scheduled or published before.
    pub to: Option<DateTime<Utc>>,
    /// Page size.
    pub limit: Option<i64>,
}

/// A published post to add to the archive.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct ImportedPost {
    /// ID in the channel. Either this or `url` identifies the post.
    #[serde(default)]
    pub external_id: Option<String>,
    /// Address.
    #[serde(default)]
    pub url: Option<String>,
    /// The text.
    #[serde(default, alias = "text")]
    pub body: String,
    /// When it went out.
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    /// Channel. Defaults to `linkedin`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Pillar.
    #[serde(default)]
    pub pillar: Option<String>,
    /// Topic.
    #[serde(default)]
    pub topic: Option<String>,
    /// Organization.
    #[serde(default)]
    pub org: Option<String>,
    /// Analytics.
    #[serde(default)]
    pub metrics: Map<String, Value>,
}

/// A weekly slot of the posting plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct PostSlot {
    /// Slot ID.
    #[serde(default = "Uuid::now_v7")]
    pub id: Uuid,
    /// ISO weekday, 1 for Monday to 7 for Sunday.
    pub weekday: i16,
    /// Local time as `HH:MM`.
    pub time: String,
    /// Pillar planned for it.
    #[serde(default)]
    pub pillar: Option<String>,
    /// Organization.
    #[serde(default)]
    pub org: Option<String>,
}

/// A content pillar with its topics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Pillar {
    /// Pillar ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// What it is about.
    pub description: Option<String>,
    /// Color as `#rrggbb`.
    pub color: Option<String>,
    /// Topics.
    pub topics: Vec<String>,
}

/// A pillar to create or replace.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct PillarInput {
    /// Name.
    pub name: String,
    /// What it is about.
    #[serde(default)]
    pub description: Option<String>,
    /// Color.
    #[serde(default)]
    pub color: Option<String>,
    /// Topics.
    #[serde(default)]
    pub topics: Vec<String>,
}

/// One occurrence of a slot in the calendar, with the posts planned for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CalendarSlot {
    /// When it is.
    pub at: DateTime<Utc>,
    /// The slot.
    pub slot: PostSlot,
    /// Posts scheduled within an hour of it.
    pub posts: Vec<Uuid>,
    /// Why nothing should go out that day, if the owner blocked it.
    pub blocked: Option<String>,
}

/// A day without posts, such as a holiday.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BlockedDay {
    /// The day.
    pub date: NaiveDate,
    /// Why, such as "Holiday".
    #[serde(default)]
    pub reason: Option<String>,
}

/// The key of the blocked days setting.
const BLOCKED_DAYS: &str = "post_blocked_days";
/// Blocked days kept at most.
const MAX_BLOCKED_DAYS: usize = 366;

fn check_status(status: &str) -> Result<(), DeskError> {
    if ["idea", "draft"].contains(&status) {
        Ok(())
    } else {
        Err(invalid(
            "`status` can be set to `idea` or `draft`; schedule with `scheduled_at`",
        ))
    }
}

fn check_body(body: &str) -> Result<String, DeskError> {
    if body.chars().count() > MAX_BODY {
        return Err(invalid(format!(
            "`body` is longer than {MAX_BODY} characters"
        )));
    }
    Ok(body.to_owned())
}

fn clean_metrics(metrics: &Map<String, Value>) -> Result<Map<String, Value>, DeskError> {
    let mut clean = Map::new();
    for (key, value) in metrics {
        if !METRICS.contains(&key.as_str()) {
            return Err(invalid(format!("unknown metric `{key}`")));
        }
        let number = match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().replace(['.', ','], "").parse::<f64>().ok(),
            _ => None,
        }
        .filter(|number| *number >= 0.0)
        .ok_or_else(|| invalid(format!("metric `{key}` needs a number")))?;
        clean.insert(key.clone(), Value::from(number));
    }
    Ok(clean)
}

fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text, "%H:%M").ok()
}

impl Desk {
    /// Creates a post.
    pub async fn create_post(&self, post: NewPost) -> Result<Post, DeskError> {
        let status = match (&post.status, post.scheduled_at) {
            (_, Some(_)) => "scheduled".to_owned(),
            (Some(status), None) => {
                check_status(status)?;
                status.clone()
            }
            (None, None) => "draft".to_owned(),
        };
        let query = format!(
            "INSERT INTO posts (id, tenant_id, org, channel, kind, pillar, topic, title, body, status, \
             scheduled_at, images, origin, variants, first_comment) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'owner', $13, $14) \
             RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(&post.org)
            .bind(post.channel.as_deref().unwrap_or("linkedin"))
            .bind(&post.kind)
            .bind(&post.pillar)
            .bind(&post.topic)
            .bind(&post.title)
            .bind(check_body(&post.body)?)
            .bind(status)
            .bind(post.scheduled_at)
            .bind(&post.images)
            .bind(variants_value(&post.variants)?)
            .bind(&post.first_comment)
            .fetch_one(self.pool())
            .await?)
    }

    /// Stores a post an agent proposes, as an `idea`. It never schedules or
    /// publishes; the owner decides what becomes of it.
    pub async fn propose_post(&self, post: NewPost) -> Result<Post, DeskError> {
        if post.body.trim().is_empty() {
            return Err(invalid("`body` must not be empty"));
        }
        // An idea with the same title, or the same text, that the owner has not
        // taken up yet gets the newer text instead of a second idea.
        let query = format!(
            "UPDATE posts SET body = $3, reason = COALESCE($4, reason), updated_at = now() \
             WHERE id = (SELECT id FROM posts WHERE tenant_id = $1 AND origin = 'agent' AND status = 'idea' \
             AND (lower(title) = lower($2) OR body = $3) ORDER BY created_at DESC LIMIT 1) \
             RETURNING {COLUMNS}"
        );
        let existing: Option<Post> = sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(&post.title)
            .bind(check_body(&post.body)?)
            .bind(&post.reason)
            .fetch_optional(self.pool())
            .await?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let query = format!(
            "INSERT INTO posts (id, tenant_id, org, channel, kind, pillar, topic, title, body, status, origin, \
             reason) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'idea', 'agent', $10) RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(&post.org)
            .bind(post.channel.as_deref().unwrap_or("linkedin"))
            .bind(&post.kind)
            .bind(&post.pillar)
            .bind(&post.topic)
            .bind(&post.title)
            .bind(check_body(&post.body)?)
            .bind(&post.reason)
            .fetch_one(self.pool())
            .await?)
    }

    /// Lists the posts agents proposed that the owner has not taken up yet.
    pub async fn proposed_posts(&self) -> Result<Vec<Post>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM posts WHERE tenant_id = $1 AND origin = 'agent' AND status = 'idea' \
             ORDER BY created_at DESC LIMIT 200"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .fetch_all(self.pool())
            .await?)
    }

    /// Returns a post.
    pub async fn post(&self, id: Uuid) -> Result<Option<Post>, DeskError> {
        let query = format!("SELECT {COLUMNS} FROM posts WHERE tenant_id = $1 AND id = $2");
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Lists posts: planned ones by time, then the rest by last change.
    pub async fn posts(&self, filter: &PostFilter) -> Result<Vec<Post>, DeskError> {
        let query = format!(
            "SELECT {COLUMNS} FROM posts WHERE tenant_id = $1 AND ($2::text IS NULL OR status = $2) \
             AND ($3::text IS NULL OR org = $3) AND ($4::text IS NULL OR pillar = $4) \
             AND ($5::text IS NULL OR (coalesce(title, '') || ' ' || body) ILIKE '%' || $5 || '%') \
             AND ($6::timestamptz IS NULL OR coalesce(published_at, scheduled_at) >= $6) \
             AND ($7::timestamptz IS NULL OR coalesce(published_at, scheduled_at) < $7) \
             ORDER BY coalesce(published_at, scheduled_at) DESC NULLS LAST, updated_at DESC LIMIT $8"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(&filter.status)
            .bind(&filter.org)
            .bind(&filter.pillar)
            .bind(filter.q.as_deref().filter(|q| !q.trim().is_empty()))
            .bind(filter.from)
            .bind(filter.to)
            .bind(filter.limit.unwrap_or(100).clamp(1, 1000))
            .fetch_all(self.pool())
            .await?)
    }

    /// Changes a post. A published post keeps its text; only analytics, URL and grouping change.
    pub async fn update_post(&self, id: Uuid, patch: PostPatch) -> Result<Option<Post>, DeskError> {
        let Some(mut post) = self.post(id).await? else {
            return Ok(None);
        };
        let published = post.status == "published";
        if published
            && (patch.body.is_some() || patch.scheduled_at.is_some() || patch.status.is_some())
        {
            return Err(invalid("a published post keeps its text, time and status"));
        }
        if post.status == "publishing" && (patch.body.is_some() || patch.images.is_some()) {
            return Err(invalid(
                "the post waits for approval to publish; decide first",
            ));
        }
        if let Some(body) = patch.body {
            post.body = check_body(&body)?;
        }
        for (field, value) in [
            (&mut post.title, patch.title),
            (&mut post.org, patch.org),
            (&mut post.kind, patch.kind),
            (&mut post.pillar, patch.pillar),
            (&mut post.topic, patch.topic),
            (&mut post.url, patch.url),
        ] {
            if let Some(value) = value {
                *field = value;
            }
        }
        if let Some(status) = patch.status {
            check_status(&status)?;
            post.status = status;
            post.scheduled_at = None;
        }
        if let Some(scheduled_at) = patch.scheduled_at {
            post.scheduled_at = scheduled_at;
            post.status = if scheduled_at.is_some() {
                "scheduled"
            } else {
                "draft"
            }
            .to_owned();
        }
        if let Some(images) = patch.images {
            post.images = images;
        }
        if let Some(variants) = patch.variants {
            post.variants = variants_value(&variants)?;
        }
        if let Some(first_comment) = patch.first_comment {
            post.first_comment = first_comment;
        }
        if let Some(metrics) = patch.metrics {
            let Value::Object(metrics) = metrics else {
                return Err(invalid("`metrics` must be an object"));
            };
            let mut merged = post.metrics.as_object().cloned().unwrap_or_default();
            merged.extend(clean_metrics(&metrics)?);
            post.metrics = Value::Object(merged);
        }
        let query = format!(
            "UPDATE posts SET org = $3, kind = $4, pillar = $5, topic = $6, title = $7, body = $8, \
             status = $9, scheduled_at = $10, url = $11, images = $12, metrics = $13, variants = $14, \
             first_comment = $15, updated_at = now() \
             WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(&post.org)
            .bind(&post.kind)
            .bind(&post.pillar)
            .bind(&post.topic)
            .bind(&post.title)
            .bind(&post.body)
            .bind(&post.status)
            .bind(post.scheduled_at)
            .bind(&post.url)
            .bind(&post.images)
            .bind(&post.metrics)
            .bind(&post.variants)
            .bind(&post.first_comment)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Deletes a post. Returns whether it existed.
    pub async fn delete_post(&self, id: Uuid) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM posts WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Notes that a run waits for approval to publish a post.
    pub async fn post_waits(&self, id: Uuid, run: Uuid) -> Result<(), DeskError> {
        sqlx::query(
            "UPDATE posts SET status = 'publishing', publish_run = $3, updated_at = now() \
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(self.tenant())
        .bind(id)
        .bind(run)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Records the outcome of publishing. A failed or denied attempt returns the
    /// post to `scheduled` or `draft`.
    pub async fn post_published(
        &self,
        id: Uuid,
        published: bool,
        external_id: Option<&str>,
    ) -> Result<Option<Post>, DeskError> {
        let query = format!(
            "UPDATE posts SET publish_run = NULL, updated_at = now(), \
             status = CASE WHEN $3 THEN 'published' WHEN scheduled_at IS NULL THEN 'draft' ELSE 'scheduled' END, \
             published_at = CASE WHEN $3 THEN now() ELSE published_at END, \
             external_id = CASE WHEN $3 THEN coalesce($4, external_id) ELSE external_id END \
             WHERE tenant_id = $1 AND id = $2 RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(query))
            .bind(self.tenant())
            .bind(id)
            .bind(published)
            .bind(external_id)
            .fetch_optional(self.pool())
            .await?)
    }

    /// Adds published posts to the archive. A post with a known `external_id`
    /// or `url` is updated. Returns the number of stored posts.
    pub async fn import_posts(&self, posts: &[ImportedPost]) -> Result<usize, DeskError> {
        let mut transaction = self.pool().begin().await?;
        for (index, post) in posts.iter().enumerate() {
            let key = post
                .external_id
                .clone()
                .or_else(|| post.url.clone())
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| {
                    invalid(format!(
                        "post {} needs an `external_id` or a `url`",
                        index + 1
                    ))
                })?;
            let metrics = Value::Object(clean_metrics(&post.metrics)?);
            sqlx::query(
                "INSERT INTO posts (id, tenant_id, org, channel, pillar, topic, body, status, published_at, \
                 url, external_id, metrics, origin) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'published', $8, $9, $10, $11, 'import') \
                 ON CONFLICT (tenant_id, external_id) WHERE external_id IS NOT NULL DO UPDATE SET \
                 metrics = posts.metrics || EXCLUDED.metrics, \
                 body = CASE WHEN EXCLUDED.body = '' THEN posts.body ELSE EXCLUDED.body END, \
                 published_at = coalesce(EXCLUDED.published_at, posts.published_at), \
                 url = coalesce(EXCLUDED.url, posts.url), \
                 pillar = coalesce(EXCLUDED.pillar, posts.pillar), \
                 topic = coalesce(EXCLUDED.topic, posts.topic), updated_at = now()",
            )
            .bind(Uuid::now_v7())
            .bind(self.tenant())
            .bind(&post.org)
            .bind(post.channel.as_deref().unwrap_or("linkedin"))
            .bind(&post.pillar)
            .bind(&post.topic)
            .bind(check_body(&post.body)?)
            .bind(post.published_at)
            .bind(&post.url)
            .bind(key)
            .bind(metrics)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(posts.len())
    }

    /// Returns the weekly slots.
    pub async fn post_slots(&self) -> Result<Vec<PostSlot>, DeskError> {
        Ok(sqlx::query_as(
            "SELECT id, weekday, time, pillar, org FROM post_slots WHERE tenant_id = $1 \
             ORDER BY weekday, time",
        )
        .bind(self.tenant())
        .fetch_all(self.pool())
        .await?)
    }

    /// Replaces the weekly slots.
    pub async fn set_post_slots(&self, slots: &[PostSlot]) -> Result<Vec<PostSlot>, DeskError> {
        for slot in slots {
            if !(1..=7).contains(&slot.weekday) {
                return Err(invalid("`weekday` runs from 1 for Monday to 7 for Sunday"));
            }
            if parse_time(&slot.time).is_none() {
                return Err(invalid(format!("`{}` is not a time like 08:30", slot.time)));
            }
        }
        let mut transaction = self.pool().begin().await?;
        sqlx::query("DELETE FROM post_slots WHERE tenant_id = $1")
            .bind(self.tenant())
            .execute(&mut *transaction)
            .await?;
        for slot in slots {
            sqlx::query(
                "INSERT INTO post_slots (id, tenant_id, weekday, time, pillar, org) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(slot.id)
            .bind(self.tenant())
            .bind(slot.weekday)
            .bind(&slot.time)
            .bind(&slot.pillar)
            .bind(&slot.org)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.post_slots().await
    }

    /// The days without posts, in order.
    pub async fn blocked_days(&self) -> Result<Vec<BlockedDay>, DeskError> {
        Ok(self.setting(BLOCKED_DAYS).await?.unwrap_or_default())
    }

    /// Replaces the days without posts.
    pub async fn set_blocked_days(
        &self,
        mut days: Vec<BlockedDay>,
    ) -> Result<Vec<BlockedDay>, DeskError> {
        if days.len() > MAX_BLOCKED_DAYS {
            return Err(invalid(format!("at most {MAX_BLOCKED_DAYS} blocked days")));
        }
        days.sort_by_key(|day| day.date);
        days.dedup_by_key(|day| day.date);
        self.set_setting(BLOCKED_DAYS, &days).await?;
        Ok(days)
    }

    /// Lays the weekly slots over a range of days, with the posts planned for
    /// each, and marks the slots of blocked days.
    pub async fn post_calendar<Z: TimeZone>(
        &self,
        zone: &Z,
        from: NaiveDate,
        days: i64,
    ) -> Result<Vec<CalendarSlot>, DeskError> {
        let slots = self.post_slots().await?;
        let blocked = self.blocked_days().await?;
        let start = zone
            .from_local_datetime(&from.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .map(|time| time.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let end = start + Duration::days(days);
        let planned = self
            .posts(&PostFilter {
                from: Some(start - Duration::hours(1)),
                to: Some(end + Duration::hours(1)),
                limit: Some(1000),
                ..PostFilter::default()
            })
            .await?;
        let mut used = HashSet::new();
        let mut calendar = Vec::new();
        for offset in 0..days {
            let day = from + Duration::days(offset);
            let weekday = i16::try_from(day.weekday().number_from_monday()).unwrap_or(1);
            for slot in slots.iter().filter(|slot| slot.weekday == weekday) {
                let Some(time) = parse_time(&slot.time) else {
                    continue;
                };
                let Some(at) = zone.from_local_datetime(&day.and_time(time)).earliest() else {
                    continue;
                };
                let at = at.with_timezone(&Utc);
                let posts = planned
                    .iter()
                    .filter(|post| {
                        post.scheduled_at
                            .is_some_and(|time| (time - at).num_minutes().abs() <= 60)
                            && used.insert(post.id)
                    })
                    .map(|post| post.id)
                    .collect();
                calendar.push(CalendarSlot {
                    at,
                    slot: slot.clone(),
                    posts,
                    blocked: blocked
                        .iter()
                        .find(|blocked| blocked.date == day)
                        .map(|blocked| {
                            blocked
                                .reason
                                .clone()
                                .unwrap_or_else(|| "Blocked".to_owned())
                        }),
                });
            }
        }
        Ok(calendar)
    }

    /// Lists the pillars.
    pub async fn pillars(&self) -> Result<Vec<Pillar>, DeskError> {
        Ok(sqlx::query_as(
            "SELECT id, name, description, color, topics FROM post_pillars WHERE tenant_id = $1 ORDER BY name",
        )
        .bind(self.tenant())
        .fetch_all(self.pool())
        .await?)
    }

    /// Creates or replaces a pillar.
    pub async fn save_pillar(&self, id: &str, pillar: PillarInput) -> Result<Pillar, DeskError> {
        if id.is_empty() || lighthouse_core::agent::slugify(id) != id {
            return Err(invalid(
                "the ID must be lowercase letters, digits and dashes",
            ));
        }
        let name = required_text("name", &pillar.name, 100)?;
        Ok(sqlx::query_as(
            "INSERT INTO post_pillars (id, tenant_id, name, description, color, topics) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (tenant_id, id) DO UPDATE SET \
             name = $3, description = $4, color = $5, topics = $6 \
             RETURNING id, name, description, color, topics",
        )
        .bind(id)
        .bind(self.tenant())
        .bind(name)
        .bind(&pillar.description)
        .bind(&pillar.color)
        .bind(&pillar.topics)
        .fetch_one(self.pool())
        .await?)
    }

    /// Deletes a pillar. Returns whether it existed.
    pub async fn delete_pillar(&self, id: &str) -> Result<bool, DeskError> {
        let result = sqlx::query("DELETE FROM post_pillars WHERE tenant_id = $1 AND id = $2")
            .bind(self.tenant())
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Reads an archive export in CSV with a header row. Known columns are
/// `external_id`, `url`, `body` or `text`, `published_at`, `channel`, `pillar`,
/// `topic`, `org` and the metric names. Other columns are ignored.
pub fn parse_csv(text: &str) -> Result<Vec<ImportedPost>, DeskError> {
    let mut rows = csv_rows(text).into_iter();
    let Some(header) = rows.next() else {
        return Ok(Vec::new());
    };
    let header: Vec<String> = header
        .iter()
        .map(|name| name.trim().to_lowercase())
        .collect();
    let mut posts = Vec::new();
    for (line, row) in rows.enumerate() {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let mut post = ImportedPost::default();
        for (name, cell) in header.iter().zip(row) {
            let value = cell.trim().to_owned();
            if value.is_empty() {
                continue;
            }
            match name.as_str() {
                "external_id" | "id" => post.external_id = Some(value),
                "url" | "link" => post.url = Some(value),
                "body" | "text" => post.body = cell,
                "published_at" | "date" => {
                    post.published_at = Some(
                        DateTime::parse_from_rfc3339(&value)
                            .map(|time| time.with_timezone(&Utc))
                            .or_else(|_| {
                                NaiveDate::parse_from_str(&value, "%Y-%m-%d").map(|date| {
                                    Utc.from_utc_datetime(
                                        &date.and_hms_opt(12, 0, 0).unwrap_or_default(),
                                    )
                                })
                            })
                            .map_err(|_| {
                                invalid(format!("row {}: `{value}` is not a date", line + 2))
                            })?,
                    );
                }
                "channel" => post.channel = Some(value),
                "pillar" => post.pillar = Some(value),
                "topic" => post.topic = Some(value),
                "org" => post.org = Some(value),
                metric if METRICS.contains(&metric) => {
                    post.metrics.insert(metric.to_owned(), Value::String(value));
                }
                _ => {}
            }
        }
        posts.push(post);
    }
    Ok(posts)
}

/// Splits CSV into rows, with quoted fields that may hold commas, quotes and line breaks.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => row.push(std::mem::take(&mut cell)),
            ('\n', false) => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            ('\r', false) => {}
            (other, _) => cell.push(other),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use serde_json::json;

    async fn desk() -> Desk {
        Desk::new(fresh_store().await, TenantId::DEFAULT)
    }

    #[tokio::test]
    async fn drafts_are_written_scheduled_and_published_once() {
        let desk = desk().await;
        let draft = desk
            .create_post(NewPost {
                body: "Hook\n\nText".to_owned(),
                pillar: Some("finance".to_owned()),
                ..NewPost::default()
            })
            .await
            .unwrap();
        assert_eq!(
            (draft.status.as_str(), draft.channel.as_str()),
            ("draft", "linkedin")
        );
        let at = Utc::now() + Duration::days(1);
        let patch: PostPatch =
            serde_json::from_value(json!({"scheduled_at": at, "title": "Late invoices"})).unwrap();
        let scheduled = desk.update_post(draft.id, patch).await.unwrap().unwrap();
        assert_eq!(scheduled.status, "scheduled");
        let filter = PostFilter {
            status: Some("scheduled".to_owned()),
            q: Some("text".to_owned()),
            ..PostFilter::default()
        };
        assert_eq!(desk.posts(&filter).await.unwrap().len(), 1);

        let run = Uuid::now_v7();
        desk.post_waits(draft.id, run).await.unwrap();
        let body = PostPatch {
            body: Some("changed".to_owned()),
            ..PostPatch::default()
        };
        assert!(
            desk.update_post(draft.id, body.clone()).await.is_err(),
            "no edits while it waits"
        );
        let back = desk
            .post_published(draft.id, false, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (back.status.as_str(), back.publish_run),
            ("scheduled", None)
        );
        let published = desk
            .post_published(draft.id, true, Some("urn:1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(published.status, "published");
        assert!(published.published_at.is_some());
        assert!(
            desk.update_post(draft.id, body).await.is_err(),
            "published text stays"
        );
        let metrics: PostPatch =
            serde_json::from_value(json!({"metrics": {"impressions": 1200}})).unwrap();
        let measured = desk.update_post(draft.id, metrics).await.unwrap().unwrap();
        assert_eq!(measured.metrics["impressions"], 1200.0);
        let bad: PostPatch = serde_json::from_value(json!({"metrics": {"likes": 1}})).unwrap();
        assert!(desk.update_post(draft.id, bad).await.is_err());
        let variants: PostPatch = serde_json::from_value(json!({
            "variants": [{"name": "shorter", "text": "Hook"}], "first_comment": "Link below"}))
        .unwrap();
        let varied = desk.update_post(draft.id, variants).await.unwrap().unwrap();
        assert_eq!(varied.variants[0]["name"], "shorter");
        assert_eq!(varied.first_comment.as_deref(), Some("Link below"));
        let unnamed: PostPatch =
            serde_json::from_value(json!({"variants": [{"name": " ", "text": "x"}]})).unwrap();
        assert!(desk.update_post(draft.id, unnamed).await.is_err());
        assert!(desk.delete_post(draft.id).await.unwrap());
        assert!(!desk.delete_post(draft.id).await.unwrap());
        let bad_status = NewPost {
            status: Some("published".to_owned()),
            ..NewPost::default()
        };
        assert!(desk.create_post(bad_status).await.is_err());
    }

    #[tokio::test]
    async fn agents_propose_ideas_only() {
        let desk = desk().await;
        assert!(desk.propose_post(NewPost::default()).await.is_err());
        let idea = desk
            .propose_post(NewPost {
                body: "A thought".to_owned(),
                scheduled_at: Some(Utc::now()),
                ..NewPost::default()
            })
            .await
            .unwrap();
        assert_eq!(
            (idea.status.as_str(), idea.origin.as_str()),
            ("idea", "agent")
        );
        assert_eq!(idea.scheduled_at, None);
        assert_eq!(desk.proposed_posts().await.unwrap().len(), 1);
        let again = desk
            .propose_post(NewPost {
                body: "A thought".to_owned(),
                reason: Some("Still relevant".to_owned()),
                ..NewPost::default()
            })
            .await
            .unwrap();
        assert_eq!(again.id, idea.id, "an equal open idea is refreshed");
        assert_eq!(again.reason.as_deref(), Some("Still relevant"));
        let titled = |body: &str| NewPost {
            body: body.to_owned(),
            title: Some("Late invoices".to_owned()),
            ..NewPost::default()
        };
        let first = desk.propose_post(titled("One")).await.unwrap();
        let second = desk.propose_post(titled("Two")).await.unwrap();
        assert_eq!((second.id, second.body.as_str()), (first.id, "Two"));
        assert_eq!(desk.proposed_posts().await.unwrap().len(), 2);
        let taken: PostPatch = serde_json::from_value(json!({"status": "draft"})).unwrap();
        desk.update_post(idea.id, taken).await.unwrap();
        assert_eq!(desk.proposed_posts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn published_posts_get_an_x_factor_and_blocked_days_mark_the_calendar() {
        let desk = desk().await;
        let posts: Vec<ImportedPost> = [("a", 100), ("b", 200), ("c", 400)]
            .into_iter()
            .map(|(id, impressions)| ImportedPost {
                external_id: Some(id.to_owned()),
                body: format!("Post {id}"),
                published_at: Some(Utc::now()),
                metrics: [("impressions".to_owned(), json!(impressions))]
                    .into_iter()
                    .collect(),
                ..ImportedPost::default()
            })
            .collect();
        desk.import_posts(&posts).await.unwrap();
        let mut factors: Vec<f64> = desk
            .posts(&PostFilter::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|post| post.x_factor)
            .collect();
        factors.sort_by(f64::total_cmp);
        assert_eq!(factors, [0.5, 1.0, 2.0], "against the median of 200");
        let draft = desk
            .create_post(NewPost {
                body: "Not out yet".to_owned(),
                ..NewPost::default()
            })
            .await
            .unwrap();
        assert_eq!(draft.x_factor, None);

        let monday = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        desk.set_post_slots(&[PostSlot {
            id: Uuid::now_v7(),
            weekday: 1,
            time: "09:00".to_owned(),
            pillar: None,
            org: None,
        }])
        .await
        .unwrap();
        let days = desk
            .set_blocked_days(vec![
                BlockedDay {
                    date: monday,
                    reason: Some("Holiday".to_owned()),
                },
                BlockedDay {
                    date: monday,
                    reason: None,
                },
            ])
            .await
            .unwrap();
        assert_eq!(days.len(), 1);
        let calendar = desk.post_calendar(&Utc, monday, 8).await.unwrap();
        assert_eq!(calendar[0].blocked.as_deref(), Some("Holiday"));
        assert_eq!(calendar[1].blocked, None);
        let many = (0..400)
            .map(|offset| BlockedDay {
                date: monday + Duration::days(offset),
                reason: None,
            })
            .collect();
        assert!(desk.set_blocked_days(many).await.is_err());
    }

    #[tokio::test]
    async fn imports_merge_into_the_archive() {
        let desk = desk().await;
        let csv = "external_id,published_at,text,impressions,reactions,ignored\n\
                   urn:1,2026-10-01,\"Hook, with comma\nand a line\",1.200,34,x\n\
                   urn:2,2026-10-08T09:00:00Z,Second,500,5,y\n";
        let posts = parse_csv(csv).unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].body, "Hook, with comma\nand a line");
        assert_eq!(desk.import_posts(&posts).await.unwrap(), 2);
        let update = [ImportedPost {
            external_id: Some("urn:1".to_owned()),
            metrics: [("comments".to_owned(), json!(7))].into_iter().collect(),
            ..ImportedPost::default()
        }];
        desk.import_posts(&update).await.unwrap();
        let archive = desk
            .posts(&PostFilter {
                status: Some("published".to_owned()),
                ..PostFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(archive.len(), 2);
        let first = archive
            .iter()
            .find(|post| post.external_id.as_deref() == Some("urn:1"))
            .unwrap();
        assert_eq!(first.metrics["impressions"], 1200.0);
        assert_eq!(first.metrics["comments"], 7.0);
        assert_eq!(first.body, "Hook, with comma\nand a line");
        assert!(desk.import_posts(&[ImportedPost::default()]).await.is_err());
        assert!(parse_csv("published_at\nsoon").is_err());
        assert!(parse_csv("").unwrap().is_empty());
    }

    #[tokio::test]
    async fn slots_lay_out_a_calendar_with_planned_posts() {
        let desk = desk().await;
        let monday = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let slots = [
            PostSlot {
                id: Uuid::now_v7(),
                weekday: 2,
                time: "08:30".to_owned(),
                pillar: Some("finance".to_owned()),
                org: None,
            },
            PostSlot {
                id: Uuid::now_v7(),
                weekday: 4,
                time: "12:00".to_owned(),
                pillar: None,
                org: None,
            },
        ];
        assert_eq!(desk.set_post_slots(&slots).await.unwrap().len(), 2);
        let tuesday =
            Utc.from_utc_datetime(&monday.succ_opt().unwrap().and_hms_opt(8, 30, 0).unwrap());
        let post = desk
            .create_post(NewPost {
                body: "x".to_owned(),
                scheduled_at: Some(tuesday),
                ..NewPost::default()
            })
            .await
            .unwrap();
        let calendar = desk.post_calendar(&Utc, monday, 14).await.unwrap();
        assert_eq!(calendar.len(), 4);
        assert_eq!(calendar[0].posts, [post.id]);
        assert!(calendar[1].posts.is_empty());
        for bad in [
            PostSlot {
                id: Uuid::now_v7(),
                weekday: 8,
                time: "08:00".to_owned(),
                pillar: None,
                org: None,
            },
            PostSlot {
                id: Uuid::now_v7(),
                weekday: 1,
                time: "8 am".to_owned(),
                pillar: None,
                org: None,
            },
        ] {
            assert!(desk.set_post_slots(&[bad]).await.is_err());
        }
        let pillar = PillarInput {
            name: "Finance".to_owned(),
            topics: vec!["Invoices".to_owned()],
            ..PillarInput::default()
        };
        assert_eq!(
            desk.save_pillar("finance", pillar).await.unwrap().topics,
            ["Invoices"]
        );
        assert!(
            desk.save_pillar("Bad", PillarInput::default())
                .await
                .is_err()
        );
        assert_eq!(desk.pillars().await.unwrap().len(), 1);
        assert!(desk.delete_pillar("finance").await.unwrap());
        assert!(!desk.delete_pillar("finance").await.unwrap());
    }
}
