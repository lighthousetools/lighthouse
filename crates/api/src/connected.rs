//! Mail, calendar, issues and pull requests from the connected services.

use crate::{ApiError, ApiState, internal, not_found};
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use lighthouse_sync::{ItemFilter, ItemPage, Order, SyncStatus, SyncedItem};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
/// Flags that put an item into the inbox.
const INBOX_FLAGS: [&str; 5] = [
    "important",
    "mention",
    "review_requested",
    "assigned",
    "unread",
];
const INBOX_KINDS: [&str; 5] = ["thread", "message", "issue", "comment", "pull_request"];

/// A configured source with its freshness.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SourceInfo {
    /// Source name.
    pub name: String,
    /// Connector it calls.
    pub connector: String,
    /// `mail`, `calendar`, `issues` or `code`.
    pub domain: String,
    /// Refresh interval in seconds.
    pub interval_seconds: u64,
    /// Actions the owner can ask for.
    pub actions: Vec<String>,
    /// Last refreshes, if there were any.
    pub status: Option<SyncStatus>,
    /// Whether the last success is older than two intervals, or missing.
    pub stale: bool,
}

/// What a refresh did.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RefreshResult {
    /// Source name.
    pub source: String,
    /// Items fetched.
    pub items: usize,
    /// Events sent to automations.
    pub events: usize,
    /// Errors of single collections.
    pub errors: Vec<String>,
}

/// An action the owner asks for, such as a reply or a comment.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct ActionRequest {
    /// Values for the action's `{{input.<field>}}` placeholders, such as `body`.
    #[serde(default)]
    pub input: Value,
}

/// What became of an action.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ActionResult {
    /// `proposed` when it waits for approval, `done` or `failed` when it ran at once.
    pub status: String,
    /// The run that holds it.
    pub run_id: Uuid,
    /// The approval to decide, when it was proposed.
    pub approval_id: Option<Uuid>,
    /// Qualified tool name.
    pub tool: String,
    /// Tool output, when it ran at once.
    pub output: Option<String>,
}

/// An item with its children, such as a thread with messages or a pull request with checks and files.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ItemDetail {
    /// The item.
    pub item: SyncedItem,
    /// Its children.
    pub children: Vec<SyncedItem>,
    /// The item as the tool returned it, if it was small enough.
    pub raw: Option<Value>,
}

/// A busy block in the calendar.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BusyBlock {
    /// Start.
    pub starts_at: DateTime<Utc>,
    /// End.
    pub ends_at: DateTime<Utc>,
}

/// Why an action could not be asked for.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    /// Unknown source or action, or an item that does not fit.
    #[error("{0}")]
    Rejected(String),
    /// The server failed.
    #[error("{0}")]
    Failed(String),
}

/// Refreshes sources and carries out actions.
#[async_trait]
pub trait Connected: Send + Sync {
    /// Lists the sources with their freshness.
    async fn sources(&self) -> Result<Vec<SourceInfo>, String>;
    /// Refreshes one source, or all when `None`. Unknown sources give an empty list.
    async fn refresh(&self, source: Option<&str>) -> Result<Vec<RefreshResult>, String>;
    /// Asks for an action. A `write` tool waits for the owner's approval.
    async fn act(
        &self,
        source: &str,
        action: &str,
        item: Option<&SyncedItem>,
        input: Value,
    ) -> Result<ActionResult, ActionError>;
}

/// Paging and search.
#[derive(Deserialize)]
pub(crate) struct ListQuery {
    q: Option<String>,
    source: Option<String>,
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// Comma-separated flags the items must have.
    flags: Option<String>,
    /// Leave out closed and done items. Defaults to `true` for issues and pull requests.
    open: Option<bool>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    domain: Option<String>,
}

impl ListQuery {
    fn filter(&self, domain: Option<&str>, kinds: &[&str], open_by_default: bool) -> ItemFilter {
        ItemFilter {
            domain: domain.map(str::to_owned).or_else(|| self.domain.clone()),
            kinds: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            source: self.source.clone(),
            q: self.q.clone(),
            flags: self
                .flags
                .as_deref()
                .map(|flags| {
                    flags
                        .split(',')
                        .map(str::trim)
                        .filter(|flag| !flag.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            state: self.state.clone(),
            open: self.open.unwrap_or(open_by_default),
            from: self.from,
            to: self.to,
            limit: self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500),
            offset: self.offset.unwrap_or(0).max(0),
            ..ItemFilter::default()
        }
    }
}

async fn page(state: &ApiState, filter: ItemFilter) -> Result<Json<ItemPage>, ApiError> {
    Ok(Json(state.sync.items(&filter).await.map_err(internal)?))
}

macro_rules! list_endpoint {
    ($name:ident, $path:literal, $id:literal, $doc:literal, $domain:expr, $kinds:expr, $open:expr, $order:expr) => {
        #[doc = $doc]
        #[utoipa::path(
            get,
            operation_id = $id,
            path = $path,
            params(
                ("q" = Option<String>, Query, description = "Full-text search"),
                ("source" = Option<String>, Query, description = "Only this source"),
                ("state" = Option<String>, Query, description = "Only this state"),
                ("flags" = Option<String>, Query, description = "Comma-separated flags the items must have"),
                ("open" = Option<bool>, Query, description = "Leave out closed and done items"),
                ("from" = Option<DateTime<Utc>>, Query, description = "Items ending or happening at or after this time"),
                ("to" = Option<DateTime<Utc>>, Query, description = "Items starting or happening before this time"),
                ("limit" = Option<i64>, Query, description = "Page size"),
                ("offset" = Option<i64>, Query, description = "Items to skip")
            ),
            responses((status = 200, body = ItemPage)),
            security(("bearer" = []))
        )]
        pub(crate) async fn $name(
            State(state): State<ApiState>,
            Query(query): Query<ListQuery>,
        ) -> Result<Json<ItemPage>, ApiError> {
            let mut filter = query.filter($domain, $kinds, $open);
            filter.order = $order;
            page(&state, filter).await
        }
    };
}

list_endpoint!(
    mail,
    "/mail/threads",
    "list_mail",
    "Lists mail threads, latest first.",
    Some("mail"),
    &["thread"],
    false,
    Order::Recent
);
list_endpoint!(
    events,
    "/calendar/events",
    "list_events",
    "Lists calendar events, earliest first.",
    Some("calendar"),
    &["event"],
    false,
    Order::Start
);
list_endpoint!(
    issues,
    "/issues",
    "list_issues",
    "Lists issues, latest change first.",
    Some("issues"),
    &["issue"],
    true,
    Order::Recent
);
list_endpoint!(
    projects,
    "/issues/projects",
    "list_projects",
    "Lists projects.",
    Some("issues"),
    &["project"],
    true,
    Order::Recent
);
list_endpoint!(
    cycles,
    "/issues/cycles",
    "list_cycles",
    "Lists cycles, earliest first.",
    Some("issues"),
    &["cycle"],
    false,
    Order::Start
);
list_endpoint!(
    pulls,
    "/pulls",
    "list_pulls",
    "Lists pull requests, latest change first.",
    Some("code"),
    &["pull_request"],
    true,
    Order::Recent
);
list_endpoint!(
    search,
    "/connected/search",
    "search_connected",
    "Searches every cached item; filter with `domain`.",
    None,
    &[],
    false,
    Order::Recent
);

/// The unified inbox: important and unread mail, mentions, review requests and
/// assigned issues that are still open, most important first.
#[utoipa::path(
    get,
    operation_id = "inbox",
    path = "/inbox",
    params(
        ("q" = Option<String>, Query, description = "Full-text search"),
        ("source" = Option<String>, Query, description = "Only this source"),
        ("limit" = Option<i64>, Query, description = "Page size"),
        ("offset" = Option<i64>, Query, description = "Items to skip")
    ),
    responses((status = 200, body = ItemPage)),
    security(("bearer" = []))
)]
pub(crate) async fn inbox(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ItemPage>, ApiError> {
    let mut filter = query.filter(None, &INBOX_KINDS, true);
    filter.any_flags = INBOX_FLAGS.iter().map(|flag| (*flag).to_owned()).collect();
    filter.order = Order::Importance;
    page(&state, filter).await
}

/// Returns an item with its children, such as the messages of a thread or the
/// checks and files of a pull request.
#[utoipa::path(
    get,
    operation_id = "get_connected_item",
    path = "/connected/items/{id}",
    params(("id" = Uuid, Path, description = "Item ID")),
    responses((status = 200, body = ItemDetail), (status = 404, description = "Unknown item")),
    security(("bearer" = []))
)]
pub(crate) async fn item(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ItemDetail>, ApiError> {
    let item = state
        .sync
        .item(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found("item"))?;
    let children = state.sync.children(&item).await.map_err(internal)?;
    let raw = state.sync.raw(id).await.map_err(internal)?;
    Ok(Json(ItemDetail {
        item,
        children,
        raw,
    }))
}

/// Range of the free/busy query.
#[derive(Deserialize)]
pub(crate) struct RangeQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

/// Returns the busy blocks of the calendar, merged. Defaults to the next seven days.
#[utoipa::path(
    get,
    operation_id = "free_busy",
    path = "/calendar/freebusy",
    params(
        ("from" = Option<DateTime<Utc>>, Query, description = "Start, defaults to now"),
        ("to" = Option<DateTime<Utc>>, Query, description = "End, defaults to seven days later")
    ),
    responses((status = 200, body = Vec<BusyBlock>)),
    security(("bearer" = []))
)]
pub(crate) async fn free_busy(
    State(state): State<ApiState>,
    Query(range): Query<RangeQuery>,
) -> Result<Json<Vec<BusyBlock>>, ApiError> {
    let from = range.from.unwrap_or_else(Utc::now);
    let to = range.to.unwrap_or(from + Duration::days(7));
    let busy = state.sync.busy(from, to).await.map_err(internal)?;
    Ok(Json(
        busy.into_iter()
            .map(|(starts_at, ends_at)| BusyBlock { starts_at, ends_at })
            .collect(),
    ))
}

/// Lists the sources with their freshness.
#[utoipa::path(
    get,
    operation_id = "list_sources",
    path = "/connected/sources",
    responses((status = 200, body = Vec<SourceInfo>)),
    security(("bearer" = []))
)]
pub(crate) async fn sources(
    State(state): State<ApiState>,
) -> Result<Json<Vec<SourceInfo>>, ApiError> {
    Ok(Json(state.connected.sources().await.map_err(internal)?))
}

fn refreshed(state: &ApiState, results: &[RefreshResult]) {
    for result in results {
        state.events.emit(
            "connected.changed",
            serde_json::json!({"source": result.source}),
        );
    }
}

/// Refreshes every source now.
#[utoipa::path(
    post,
    operation_id = "refresh_sources",
    path = "/connected/refresh",
    responses((status = 200, body = Vec<RefreshResult>)),
    security(("bearer" = []))
)]
pub(crate) async fn refresh_all(
    State(state): State<ApiState>,
) -> Result<Json<Vec<RefreshResult>>, ApiError> {
    let results = state.connected.refresh(None).await.map_err(internal)?;
    refreshed(&state, &results);
    Ok(Json(results))
}

/// Refreshes one source now.
#[utoipa::path(
    post,
    operation_id = "refresh_source",
    path = "/connected/sources/{name}/refresh",
    params(("name" = String, Path, description = "Source name")),
    responses((status = 200, body = RefreshResult), (status = 404, description = "Unknown source")),
    security(("bearer" = []))
)]
pub(crate) async fn refresh(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<Json<RefreshResult>, ApiError> {
    let results = state
        .connected
        .refresh(Some(&name))
        .await
        .map_err(internal)?;
    refreshed(&state, &results);
    results
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| not_found("source"))
}

fn action_error(error: ActionError) -> ApiError {
    match error {
        ActionError::Rejected(reason) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, reason),
        ActionError::Failed(reason) => internal(reason),
    }
}

fn acted(state: &ApiState, result: ActionResult) -> (StatusCode, Json<ActionResult>) {
    state.events.emit(
        "connected.changed",
        serde_json::json!({"run_id": result.run_id}),
    );
    let status = if result.status == "proposed" {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (status, Json(result))
}

/// Asks for an action on an item, such as `reply`, `archive`, `comment` or `approve`.
///
/// A tool with external effect waits for the owner's approval, and the answer
/// is `202` with the approval. A `read` or `draft` tool runs at once.
#[utoipa::path(
    post,
    operation_id = "act_on_item",
    path = "/connected/items/{id}/actions/{action}",
    params(
        ("id" = Uuid, Path, description = "Item ID"),
        ("action" = String, Path, description = "Action name from the source")
    ),
    request_body = ActionRequest,
    responses(
        (status = 202, body = ActionResult),
        (status = 200, body = ActionResult),
        (status = 404, description = "Unknown item"),
        (status = 422, description = "Unknown action, or the item does not fit it")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn act_on_item(
    State(state): State<ApiState>,
    Path((id, action)): Path<(Uuid, String)>,
    request: Option<Json<ActionRequest>>,
) -> Result<(StatusCode, Json<ActionResult>), ApiError> {
    let item = state
        .sync
        .item(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found("item"))?;
    let input = request
        .map(|Json(request)| request.input)
        .unwrap_or(Value::Null);
    let result = state
        .connected
        .act(&item.source, &action, Some(&item), input)
        .await
        .map_err(action_error)?;
    Ok(acted(&state, result))
}

/// Asks for an action of a source without an item, such as `create` or `send`.
#[utoipa::path(
    post,
    operation_id = "act_on_source",
    path = "/connected/sources/{name}/actions/{action}",
    params(
        ("name" = String, Path, description = "Source name"),
        ("action" = String, Path, description = "Action name from the source")
    ),
    request_body = ActionRequest,
    responses(
        (status = 202, body = ActionResult),
        (status = 200, body = ActionResult),
        (status = 422, description = "Unknown source or action")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn act_on_source(
    State(state): State<ApiState>,
    Path((name, action)): Path<(String, String)>,
    request: Option<Json<ActionRequest>>,
) -> Result<(StatusCode, Json<ActionResult>), ApiError> {
    let input = request
        .map(|Json(request)| request.input)
        .unwrap_or(Value::Null);
    let result = state
        .connected
        .act(&name, &action, None, input)
        .await
        .map_err(action_error)?;
    Ok(acted(&state, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_sync::MappedItem;
    use serde_json::json;

    fn item(id: &str, title: &str, flags: &[&str], state: &str) -> MappedItem {
        MappedItem {
            external_id: id.to_owned(),
            title: Some(title.to_owned()),
            state: Some(state.to_owned()),
            flags: flags.iter().map(|flag| (*flag).to_owned()).collect(),
            occurred_at: Some(Utc::now()),
            data: json!({}),
            ..MappedItem::default()
        }
    }

    #[tokio::test]
    async fn domains_list_search_and_page_and_the_inbox_sorts_by_importance() {
        let harness = harness(Some(TOKEN)).await;
        let sync = &harness.sync;
        sync.upsert(
            "mail",
            "mail",
            "thread",
            &[
                item("t1", "Offer request", &["important"], "inbox"),
                item("t2", "Newsletter", &[], "inbox"),
            ],
            &[],
        )
        .await
        .unwrap();
        sync.upsert(
            "github",
            "code",
            "pull_request",
            &[
                item("p1", "Fix login", &["review_requested"], "open"),
                item("p2", "Old change", &["review_requested"], "merged"),
            ],
            &[],
        )
        .await
        .unwrap();
        let mut check = item("p1/ci", "ci", &["failed"], "failure");
        check.parent_id = Some("p1".to_owned());
        sync.upsert("github", "code", "check", &[check], &[])
            .await
            .unwrap();
        sync.upsert(
            "linear",
            "issues",
            "issue",
            &[item("i1", "Plan release", &["assigned"], "todo")],
            &[],
        )
        .await
        .unwrap();
        let start = Utc::now() + Duration::hours(1);
        let mut event = item("e1", "Review", &[], "confirmed");
        event.starts_at = Some(start);
        event.ends_at = Some(start + Duration::hours(1));
        sync.upsert("cal", "calendar", "event", &[event], &[])
            .await
            .unwrap();

        let app = &harness.app;
        let get = |uri: String| async move {
            let (status, body) = call(app, "GET", &uri, Some(TOKEN)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            body
        };
        let page: ItemPage =
            serde_json::from_str(&get("/mail/threads?limit=1".into()).await).unwrap();
        assert_eq!((page.total, page.next_offset), (2, Some(1)));
        let page: ItemPage =
            serde_json::from_str(&get("/mail/threads?q=offer".into()).await).unwrap();
        assert_eq!(page.items[0].external_id, "t1");
        let page: ItemPage = serde_json::from_str(&get("/pulls".into()).await).unwrap();
        assert_eq!(
            page.total, 1,
            "merged pull requests are left out by default"
        );
        let page: ItemPage =
            serde_json::from_str(&get("/pulls?open=false&flags=review_requested".into()).await)
                .unwrap();
        assert_eq!(page.total, 2);
        let page: ItemPage = serde_json::from_str(&get("/issues".into()).await).unwrap();
        assert_eq!(page.total, 1);
        let page: ItemPage = serde_json::from_str(&get("/calendar/events".into()).await).unwrap();
        assert_eq!(page.total, 1);
        let busy: Vec<BusyBlock> =
            serde_json::from_str(&get("/calendar/freebusy".into()).await).unwrap();
        assert_eq!(busy.len(), 1);
        let page: ItemPage =
            serde_json::from_str(&get("/connected/search?q=review&domain=calendar".into()).await)
                .unwrap();
        assert_eq!(page.total, 1);
        for uri in ["/issues/projects", "/issues/cycles"] {
            get(uri.into()).await;
        }

        let inbox: ItemPage = serde_json::from_str(&get("/inbox".into()).await).unwrap();
        let order: Vec<_> = inbox
            .items
            .iter()
            .map(|item| item.external_id.as_str())
            .collect();
        assert_eq!(order.len(), 3, "{order:?}");
        assert_eq!(order[0], "p1", "a review request ranks first");

        let pr = &inbox.items[0];
        let detail: ItemDetail =
            serde_json::from_str(&get(format!("/connected/items/{}", pr.id)).await).unwrap();
        assert_eq!(detail.children[0].external_id, "p1/ci");
        let unknown = format!("/connected/items/{}", Uuid::nil());
        assert_eq!(
            call(app, "GET", &unknown, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn sources_refresh_and_actions_go_through_the_service() {
        let harness = harness(Some(TOKEN)).await;
        harness
            .sync
            .upsert(
                "github",
                "code",
                "pull_request",
                &[item("p1", "Fix", &[], "open")],
                &[],
            )
            .await
            .unwrap();
        let app = &harness.app;
        let (_, body) = call(app, "GET", "/connected/sources", Some(TOKEN)).await;
        assert!(body.contains("approve"));
        let mut events = harness.events.subscribe();
        let (status, body) = call(app, "POST", "/connected/refresh", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("github"));
        assert_eq!(events.recv().await.unwrap().name, "connected.changed");
        assert_eq!(
            call(
                app,
                "POST",
                "/connected/sources/github/refresh",
                Some(TOKEN)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(app, "POST", "/connected/sources/nope/refresh", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let (_, body) = call(app, "GET", "/pulls", Some(TOKEN)).await;
        let page: ItemPage = serde_json::from_str(&body).unwrap();
        let uri = format!("/connected/items/{}/actions/approve", page.items[0].id);
        let (status, body) = send(
            app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"input": {"body": "Looks good"}})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let result: ActionResult = serde_json::from_str(&body).unwrap();
        assert_eq!(result.status, "proposed");
        let uri = format!("/connected/items/{}/actions/nope", page.items[0].id);
        assert_eq!(
            call(app, "POST", &uri, Some(TOKEN)).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let uri = format!("/connected/items/{}/actions/approve", Uuid::nil());
        assert_eq!(
            call(app, "POST", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = send(
            app,
            "POST",
            "/connected/sources/mail/actions/draft",
            Some(TOKEN),
            Some(json!({"input": {"to": "a"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("done"));
    }
}
