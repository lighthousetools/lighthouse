//! Posts, the posting plan, pillars, the archive and the owner's brand profile.

use crate::connected::{ActionError, ActionResult};
use crate::{ApiError, ApiState, desk_error, internal, not_found};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use lighthouse_desk::{
    BrandProfile, CalendarSlot, ImportedPost, NewPost, Pillar, PillarInput, Post, PostCheck,
    PostFilter, PostPatch, PostSlot, check_post, parse_csv,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_CALENDAR_DAYS: i64 = 92;

/// Publishes posts. Every call waits for the owner's approval.
#[async_trait]
pub trait Publisher: Send + Sync {
    /// Asks to publish a post.
    async fn publish(&self, post: &Post) -> Result<ActionResult, ActionError>;

    /// The network posts go to, such as `linkedin`, if it is configured.
    fn service(&self) -> Option<String> {
        None
    }
}

/// Writes texts for the owner, such as rewrites of posts and decks.
#[async_trait]
pub trait Writer: Send + Sync {
    /// Writes a text from instructions and material. The error is safe to show.
    async fn write(&self, instructions: &str, material: &str) -> Result<Written, String>;
}

/// A text the writer proposes.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Written {
    /// The run that wrote it, with its cost.
    pub run_id: Uuid,
    /// The text.
    pub text: String,
}

/// How to rewrite a post.
#[derive(Deserialize, ToSchema)]
pub struct RewriteRequest {
    /// What to change, such as "shorter" or "start with the number". Defaults to a clearer, tighter version.
    #[serde(default)]
    pub instruction: Option<String>,
    /// Also keep the proposal as a variant of this name.
    #[serde(default)]
    pub save_as: Option<String>,
}

/// Text to check.
#[derive(Deserialize, ToSchema)]
pub struct CheckRequest {
    /// The text of a post.
    pub text: String,
}

/// Published posts to add to the archive, as a list or as CSV with a header row.
#[derive(Deserialize, ToSchema)]
pub struct ImportRequest {
    /// Posts as objects.
    #[serde(default)]
    pub posts: Vec<ImportedPost>,
    /// Posts as CSV.
    #[serde(default)]
    pub csv: Option<String>,
}

/// Result of an import.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ImportResult {
    /// Posts stored or updated.
    pub imported: usize,
}

/// Range of the posting calendar.
#[derive(Deserialize)]
pub(crate) struct CalendarQuery {
    from: Option<NaiveDate>,
    days: Option<i64>,
}

fn changed(state: &ApiState, id: Option<Uuid>) {
    state.events.emit("posts.changed", json!({"id": id}));
}

async fn check_images(state: &ApiState, images: &[Uuid]) -> Result<(), ApiError> {
    for image in images {
        let found = state.brain.get(*image).await.map_err(internal)?;
        if found.is_none() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("no Brain entry {image} for the image"),
            ));
        }
    }
    Ok(())
}

/// Lists posts: planned and published ones by time, newest first.
#[utoipa::path(
    get,
    operation_id = "list_posts",
    path = "/posts",
    params(
        ("status" = Option<String>, Query, description = "idea, draft, scheduled, publishing or published"),
        ("org" = Option<String>, Query, description = "Only this organization"),
        ("pillar" = Option<String>, Query, description = "Only this pillar"),
        ("q" = Option<String>, Query, description = "Text search"),
        ("from" = Option<chrono::DateTime<Utc>>, Query, description = "Planned or published at or after"),
        ("to" = Option<chrono::DateTime<Utc>>, Query, description = "Planned or published before"),
        ("limit" = Option<i64>, Query, description = "Page size")
    ),
    responses((status = 200, body = Vec<Post>)),
    security(("bearer" = []))
)]
pub(crate) async fn list(
    State(state): State<ApiState>,
    Query(filter): Query<PostFilter>,
) -> Result<Json<Vec<Post>>, ApiError> {
    Ok(Json(state.desk.posts(&filter).await.map_err(desk_error)?))
}

/// Writes a new post. With `scheduled_at` it is planned, never published by itself.
#[utoipa::path(
    post,
    operation_id = "create_post",
    path = "/posts",
    request_body = NewPost,
    responses((status = 201, body = Post), (status = 400, description = "Invalid post")),
    security(("bearer" = []))
)]
pub(crate) async fn create(
    State(state): State<ApiState>,
    Json(post): Json<NewPost>,
) -> Result<(StatusCode, Json<Post>), ApiError> {
    check_images(&state, &post.images).await?;
    let post = state.desk.create_post(post).await.map_err(desk_error)?;
    changed(&state, Some(post.id));
    Ok((StatusCode::CREATED, Json(post)))
}

/// Returns a post.
#[utoipa::path(
    get,
    operation_id = "get_post",
    path = "/posts/{id}",
    params(("id" = Uuid, Path, description = "Post ID")),
    responses((status = 200, body = Post), (status = 404, description = "Unknown post")),
    security(("bearer" = []))
)]
pub(crate) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Post>, ApiError> {
    let post = state.desk.post(id).await.map_err(desk_error)?;
    post.map(Json).ok_or_else(|| not_found("post"))
}

/// Changes a post, plans it, or fills in its analytics.
#[utoipa::path(
    patch,
    operation_id = "update_post",
    path = "/posts/{id}",
    params(("id" = Uuid, Path, description = "Post ID")),
    request_body = PostPatch,
    responses(
        (status = 200, body = Post),
        (status = 400, description = "Invalid change"),
        (status = 404, description = "Unknown post")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn update(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(patch): Json<PostPatch>,
) -> Result<Json<Post>, ApiError> {
    if let Some(images) = &patch.images {
        check_images(&state, images).await?;
    }
    let post = state
        .desk
        .update_post(id, patch)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("post"))?;
    changed(&state, Some(id));
    Ok(Json(post))
}

/// Deletes a post.
#[utoipa::path(
    delete,
    operation_id = "delete_post",
    path = "/posts/{id}",
    params(("id" = Uuid, Path, description = "Post ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown post")),
    security(("bearer" = []))
)]
pub(crate) async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_post(id).await.map_err(desk_error)? {
        return Err(not_found("post"));
    }
    changed(&state, Some(id));
    Ok(StatusCode::NO_CONTENT)
}

/// Checks a text: length, hook and hashtags.
#[utoipa::path(
    post,
    operation_id = "check_text",
    path = "/posts/check",
    request_body = CheckRequest,
    responses((status = 200, body = PostCheck)),
    security(("bearer" = []))
)]
pub(crate) async fn check(Json(request): Json<CheckRequest>) -> Json<PostCheck> {
    Json(check_post(&request.text))
}

/// Asks to publish a post. It goes out only after the owner approves the call.
#[utoipa::path(
    post,
    operation_id = "publish_post",
    path = "/posts/{id}/publish",
    params(("id" = Uuid, Path, description = "Post ID")),
    responses(
        (status = 202, body = ActionResult),
        (status = 404, description = "Unknown post"),
        (status = 409, description = "Published already or waiting for approval"),
        (status = 422, description = "The check found errors, or publishing is not set up")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn publish(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<ActionResult>), ApiError> {
    let post = state
        .desk
        .post(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("post"))?;
    if matches!(post.status.as_str(), "published" | "publishing") {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("the post is {} already", post.status),
        ));
    }
    let checked = check_post(&post.body);
    if !checked.ok {
        let reasons: Vec<String> = checked
            .issues
            .into_iter()
            .map(|issue| issue.message)
            .collect();
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            reasons.join(" "),
        ));
    }
    let result = state
        .publisher
        .publish(&post)
        .await
        .map_err(|error| match error {
            ActionError::Rejected(reason) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason),
            ActionError::Failed(reason) => internal(reason),
        })?;
    match result.status.as_str() {
        "proposed" => state
            .desk
            .post_waits(id, result.run_id)
            .await
            .map_err(desk_error)?,
        "done" => {
            state
                .desk
                .post_published(id, true, None)
                .await
                .map_err(desk_error)?;
        }
        _ => {}
    }
    changed(&state, Some(id));
    Ok((StatusCode::ACCEPTED, Json(result)))
}

/// Proposes a rewrite of a post in the owner's voice. Nothing changes and
/// nothing is published, unless `save_as` keeps it as a variant.
#[utoipa::path(
    post,
    operation_id = "rewrite_post",
    path = "/posts/{id}/rewrite",
    params(("id" = Uuid, Path, description = "Post ID")),
    request_body = RewriteRequest,
    responses(
        (status = 200, body = Written),
        (status = 404, description = "Unknown post"),
        (status = 502, description = "The model failed")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn rewrite(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(request): Json<RewriteRequest>,
) -> Result<Json<Written>, ApiError> {
    let post = state
        .desk
        .post(id)
        .await
        .map_err(desk_error)?
        .ok_or_else(|| not_found("post"))?;
    let instruction = request
        .instruction
        .unwrap_or_else(|| "Make it clearer and tighter, with a strong first line.".to_owned());
    let instructions = format!(
        "Rewrite this post for the owner. {instruction} Keep its meaning and facts, and keep it under 3,000 characters."
    );
    let written = state
        .writer
        .write(&instructions, &post.body)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))?;
    if let Some(name) = request.save_as {
        let mut variants: Vec<lighthouse_desk::Variant> =
            serde_json::from_value(post.variants.clone()).unwrap_or_default();
        variants.push(lighthouse_desk::Variant {
            name,
            text: written.text.clone(),
        });
        let patch = PostPatch {
            variants: Some(variants),
            ..PostPatch::default()
        };
        state
            .desk
            .update_post(id, patch)
            .await
            .map_err(desk_error)?;
        changed(&state, Some(id));
    }
    Ok(Json(written))
}

/// Adds published posts to the archive, from a list or from CSV.
#[utoipa::path(
    post,
    operation_id = "import_posts",
    path = "/posts/import",
    request_body = ImportRequest,
    responses((status = 200, body = ImportResult), (status = 400, description = "Invalid data")),
    security(("bearer" = []))
)]
pub(crate) async fn import(
    State(state): State<ApiState>,
    Json(request): Json<ImportRequest>,
) -> Result<Json<ImportResult>, ApiError> {
    let mut posts = request.posts;
    if let Some(csv) = &request.csv {
        posts.extend(parse_csv(csv).map_err(desk_error)?);
    }
    let imported = state.desk.import_posts(&posts).await.map_err(desk_error)?;
    changed(&state, None);
    Ok(Json(ImportResult { imported }))
}

/// Lays the weekly slots over a range of days, with the posts planned for each.
#[utoipa::path(
    get,
    operation_id = "post_calendar",
    path = "/posts/calendar",
    params(
        ("from" = Option<NaiveDate>, Query, description = "First day, defaults to today"),
        ("days" = Option<i64>, Query, description = "Number of days, default 28, at most 92")
    ),
    responses((status = 200, body = Vec<CalendarSlot>)),
    security(("bearer" = []))
)]
pub(crate) async fn calendar(
    State(state): State<ApiState>,
    Query(query): Query<CalendarQuery>,
) -> Result<Json<Vec<CalendarSlot>>, ApiError> {
    let from = query
        .from
        .unwrap_or_else(|| Utc::now().with_timezone(&state.zone).date_naive());
    let days = query.days.unwrap_or(28).clamp(1, MAX_CALENDAR_DAYS);
    let calendar = state
        .desk
        .post_calendar(&state.zone, from, days)
        .await
        .map_err(desk_error)?;
    Ok(Json(calendar))
}

/// Returns the weekly slots of the posting plan.
#[utoipa::path(
    get,
    operation_id = "list_post_slots",
    path = "/posts/slots",
    responses((status = 200, body = Vec<PostSlot>)),
    security(("bearer" = []))
)]
pub(crate) async fn slots(State(state): State<ApiState>) -> Result<Json<Vec<PostSlot>>, ApiError> {
    Ok(Json(state.desk.post_slots().await.map_err(desk_error)?))
}

/// Replaces the weekly slots.
#[utoipa::path(
    put,
    operation_id = "set_post_slots",
    path = "/posts/slots",
    request_body = Vec<PostSlot>,
    responses((status = 200, body = Vec<PostSlot>), (status = 400, description = "Invalid slot")),
    security(("bearer" = []))
)]
pub(crate) async fn set_slots(
    State(state): State<ApiState>,
    Json(slots): Json<Vec<PostSlot>>,
) -> Result<Json<Vec<PostSlot>>, ApiError> {
    let slots = state
        .desk
        .set_post_slots(&slots)
        .await
        .map_err(desk_error)?;
    changed(&state, None);
    Ok(Json(slots))
}

/// Lists the content pillars with their topics.
#[utoipa::path(
    get,
    operation_id = "list_pillars",
    path = "/posts/pillars",
    responses((status = 200, body = Vec<Pillar>)),
    security(("bearer" = []))
)]
pub(crate) async fn pillars(State(state): State<ApiState>) -> Result<Json<Vec<Pillar>>, ApiError> {
    Ok(Json(state.desk.pillars().await.map_err(desk_error)?))
}

/// Creates or replaces a pillar.
#[utoipa::path(
    put,
    operation_id = "save_pillar",
    path = "/posts/pillars/{id}",
    params(("id" = String, Path, description = "Pillar ID")),
    request_body = PillarInput,
    responses((status = 200, body = Pillar), (status = 400, description = "Invalid pillar")),
    security(("bearer" = []))
)]
pub(crate) async fn save_pillar(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(pillar): Json<PillarInput>,
) -> Result<Json<Pillar>, ApiError> {
    let pillar = state
        .desk
        .save_pillar(&id, pillar)
        .await
        .map_err(desk_error)?;
    changed(&state, None);
    Ok(Json(pillar))
}

/// Deletes a pillar.
#[utoipa::path(
    delete,
    operation_id = "delete_pillar",
    path = "/posts/pillars/{id}",
    params(("id" = String, Path, description = "Pillar ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown pillar")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_pillar(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.delete_pillar(&id).await.map_err(desk_error)? {
        return Err(not_found("pillar"));
    }
    changed(&state, None);
    Ok(StatusCode::NO_CONTENT)
}

/// Returns the owner's brand profile.
#[utoipa::path(
    get,
    operation_id = "get_branding",
    path = "/branding",
    responses((status = 200, body = BrandProfile)),
    security(("bearer" = []))
)]
pub(crate) async fn branding(
    State(state): State<ApiState>,
) -> Result<Json<BrandProfile>, ApiError> {
    Ok(Json(state.desk.brand_profile().await.map_err(desk_error)?))
}

/// Replaces the owner's brand profile.
#[utoipa::path(
    put,
    operation_id = "save_branding",
    path = "/branding",
    request_body = BrandProfile,
    responses((status = 200, body = BrandProfile)),
    security(("bearer" = []))
)]
pub(crate) async fn save_branding(
    State(state): State<ApiState>,
    Json(profile): Json<BrandProfile>,
) -> Result<Json<BrandProfile>, ApiError> {
    let profile = state
        .desk
        .save_brand_profile(&profile)
        .await
        .map_err(desk_error)?;
    state.events.emit("branding.changed", json!({}));
    Ok(Json(profile))
}

#[cfg(test)]
mod tests {
    use crate::tests::{TOKEN, call, harness, send};
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn posts_are_planned_checked_imported_and_published_through_approval() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let good = format!(
            "Most invoices are late for one reason.\n\n{} #finance",
            "Here is why. ".repeat(30)
        );
        let (status, body) = send(
            app,
            "POST",
            "/posts",
            Some(TOKEN),
            Some(json!({"body": good, "pillar": "finance"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let post: Value = serde_json::from_str(&body).unwrap();
        let id = post["id"].as_str().unwrap().to_owned();
        let missing_image = json!({"body": "x", "images": [uuid::Uuid::nil()]});
        assert_eq!(
            send(app, "POST", "/posts", Some(TOKEN), Some(missing_image))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );

        let (_, body) = send(
            app,
            "POST",
            "/posts/check",
            Some(TOKEN),
            Some(json!({"text": "short"})),
        )
        .await;
        assert!(body.contains("\"short\""));

        let uri = format!("/posts/{id}");
        let plan = json!({"scheduled_at": "2030-01-07T08:30:00Z"});
        let (_, body) = send(app, "PATCH", &uri, Some(TOKEN), Some(plan)).await;
        assert!(body.contains("\"scheduled\""));
        assert_eq!(call(app, "GET", &uri, Some(TOKEN)).await.0, StatusCode::OK);
        let slots = json!([{"weekday": 1, "time": "09:30", "pillar": "finance"}]);
        let (status, _) = send(app, "PUT", "/posts/slots", Some(TOKEN), Some(slots)).await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(
            app,
            "GET",
            "/posts/calendar?from=2030-01-07&days=7",
            Some(TOKEN),
        )
        .await;
        let calendar: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(calendar[0]["posts"][0], id.as_str(), "{body}");
        assert!(
            call(app, "GET", "/posts/slots", Some(TOKEN))
                .await
                .1
                .contains("09:30")
        );

        let publish = format!("{uri}/publish");
        let (status, body) = call(app, "POST", &publish, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (_, body) = call(app, "GET", &uri, Some(TOKEN)).await;
        assert!(body.contains("\"publishing\""));
        assert_eq!(
            call(app, "POST", &publish, Some(TOKEN)).await.0,
            StatusCode::CONFLICT
        );
        let (_, body) = send(
            app,
            "POST",
            "/posts",
            Some(TOKEN),
            Some(json!({"body": " "})),
        )
        .await;
        let empty: Value = serde_json::from_str(&body).unwrap();
        let empty_publish = format!("/posts/{}/publish", empty["id"].as_str().unwrap());
        assert_eq!(
            call(app, "POST", &empty_publish, Some(TOKEN)).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let import = json!({"csv": "external_id,published_at,text,impressions\nurn:9,2026-10-01,Old post,900\n",
                            "posts": [{"url": "https://example.com/p/1", "body": "Another", "metrics": {"reactions": 4}}]});
        let (status, body) = send(app, "POST", "/posts/import", Some(TOKEN), Some(import)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"imported\":2"));
        let (_, body) = call(app, "GET", "/posts?status=published", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<Vec<Value>>(&body).unwrap().len(), 2);
        let bad = json!({"posts": [{"body": "no key"}]});
        assert_eq!(
            send(app, "POST", "/posts/import", Some(TOKEN), Some(bad))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );

        let pillar = json!({"name": "Finance", "topics": ["Invoices"]});
        assert_eq!(
            send(
                app,
                "PUT",
                "/posts/pillars/finance",
                Some(TOKEN),
                Some(pillar)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(
            call(app, "GET", "/posts/pillars", Some(TOKEN))
                .await
                .1
                .contains("Invoices")
        );
        assert_eq!(
            call(app, "DELETE", "/posts/pillars/finance", Some(TOKEN))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", "/posts/pillars/finance", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "POST", &publish, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(app, "PATCH", &uri, Some(TOKEN), Some(json!({})))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn rewrites_are_proposed_and_kept_as_variants_on_request() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let (_, body) = send(
            app,
            "POST",
            "/posts",
            Some(TOKEN),
            Some(json!({"body": "A long post"})),
        )
        .await;
        let post: Value = serde_json::from_str(&body).unwrap();
        let uri = format!("/posts/{}/rewrite", post["id"].as_str().unwrap());
        let (status, body) = send(app, "POST", &uri, Some(TOKEN), Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("rewritten: A long post"));
        let (_, body) = call(
            app,
            "GET",
            &format!("/posts/{}", post["id"].as_str().unwrap()),
            Some(TOKEN),
        )
        .await;
        assert!(
            body.contains("\"variants\":[]"),
            "a proposal changes nothing"
        );
        send(
            app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"instruction": "shorter", "save_as": "short"})),
        )
        .await;
        let (_, body) = call(
            app,
            "GET",
            &format!("/posts/{}", post["id"].as_str().unwrap()),
            Some(TOKEN),
        )
        .await;
        assert!(body.contains("\"name\":\"short\""));
        let (_, body) = send(
            app,
            "POST",
            "/posts",
            Some(TOKEN),
            Some(json!({"body": "fail"})),
        )
        .await;
        let failing: Value = serde_json::from_str(&body).unwrap();
        let uri = format!("/posts/{}/rewrite", failing["id"].as_str().unwrap());
        assert_eq!(
            send(app, "POST", &uri, Some(TOKEN), Some(json!({})))
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
        let missing = format!("/posts/{}/rewrite", uuid::Uuid::nil());
        assert_eq!(
            send(app, "POST", &missing, Some(TOKEN), Some(json!({})))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn the_brand_profile_is_kept() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        let (_, body) = call(app, "GET", "/branding", Some(TOKEN)).await;
        assert!(body.contains("\"topics\":[]"));
        let profile = json!({"headline": "Builds tools", "voice": "Plain and direct", "topics": ["Automation"], "avoid": []});
        let (status, body) = send(app, "PUT", "/branding", Some(TOKEN), Some(profile)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Plain and direct"));
    }
}
