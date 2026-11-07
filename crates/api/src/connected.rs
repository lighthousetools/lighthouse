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
    org: Option<String>,
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
    /// Also items snoozed until later.
    snoozed: Option<bool>,
}

impl ListQuery {
    fn filter(&self, domain: Option<&str>, kinds: &[&str], open_by_default: bool) -> ItemFilter {
        ItemFilter {
            domain: domain.map(str::to_owned).or_else(|| self.domain.clone()),
            kinds: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            source: self.source.clone(),
            org: self.org.clone(),
            include_snoozed: self.snoozed.unwrap_or(false),
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
                ("org" = Option<String>, Query, description = "Only sources of this organization"),
                ("state" = Option<String>, Query, description = "Only this state"),
                ("flags" = Option<String>, Query, description = "Comma-separated flags the items must have"),
                ("open" = Option<bool>, Query, description = "Leave out closed and done items"),
                ("from" = Option<DateTime<Utc>>, Query, description = "Items ending or happening at or after this time"),
                ("to" = Option<DateTime<Utc>>, Query, description = "Items starting or happening before this time"),
                ("snoozed" = Option<bool>, Query, description = "Also items snoozed until later"),
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
    deals,
    "/deals",
    "list_deals",
    "Lists deals, latest change first. Open deals by default.",
    Some("deals"),
    &["deal"],
    true,
    Order::Recent
);
list_endpoint!(
    accounts,
    "/bank/accounts",
    "list_bank_accounts",
    "Lists bank accounts with their balances.",
    Some("bank"),
    &["account"],
    false,
    Order::Recent
);
list_endpoint!(
    transactions,
    "/bank/transactions",
    "list_transactions",
    "Lists bank transactions, latest first.",
    Some("bank"),
    &["transaction"],
    false,
    Order::Recent
);
list_endpoint!(
    invoices,
    "/invoices",
    "list_invoices",
    "Lists issued and received invoices, latest first. Filter with `flags=overdue`, `receivable` or `payable`.",
    Some("invoices"),
    &["invoice"],
    false,
    Order::Recent
);
list_endpoint!(
    contracts,
    "/contracts",
    "list_contracts",
    "Lists contracts and documents, latest first.",
    Some("documents"),
    &["contract", "document"],
    false,
    Order::Recent
);
list_endpoint!(
    rooms,
    "/rooms",
    "list_rooms",
    "Lists deal rooms, latest activity first.",
    Some("rooms"),
    &["room"],
    false,
    Order::Recent
);
list_endpoint!(
    holdings,
    "/holdings",
    "list_holdings",
    "Lists holdings of the portfolio. Data only.",
    Some("holdings"),
    &["holding"],
    false,
    Order::Recent
);
list_endpoint!(
    quotes,
    "/holdings/quotes",
    "list_quotes",
    "Lists market quotes. Data only.",
    Some("holdings"),
    &["quote"],
    false,
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
        ("org" = Option<String>, Query, description = "Only sources of this organization"),
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

/// Parameters of the key numbers.
#[derive(Deserialize)]
pub(crate) struct NumbersQuery {
    org: Option<String>,
    months: Option<i32>,
}

/// Computes key numbers from the cache: pipeline, cash, burn, runway, open and
/// overdue invoices, revenue per month and holdings, per currency.
#[utoipa::path(
    get,
    operation_id = "numbers",
    path = "/numbers",
    params(
        ("org" = Option<String>, Query, description = "Only sources of this organization"),
        ("months" = Option<i32>, Query, description = "Months of revenue, default 12, at most 36")
    ),
    responses((status = 200, body = lighthouse_sync::Numbers)),
    security(("bearer" = []))
)]
pub(crate) async fn numbers(
    State(state): State<ApiState>,
    Query(query): Query<NumbersQuery>,
) -> Result<Json<lighthouse_sync::Numbers>, ApiError> {
    let months = query.months.unwrap_or(12).clamp(1, 36);
    let numbers = state
        .sync
        .numbers(query.org.as_deref(), months, Utc::now())
        .await
        .map_err(internal)?;
    Ok(Json(numbers))
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

/// When to show an item again.
#[derive(Deserialize, ToSchema)]
pub struct ItemSnooze {
    /// Hide it until this time.
    pub until: DateTime<Utc>,
}

/// Hides an item from lists and the inbox until a time ("later"). Nothing
/// changes in the service.
#[utoipa::path(
    post,
    operation_id = "snooze_item",
    path = "/connected/items/{id}/snooze",
    params(("id" = Uuid, Path, description = "Item ID")),
    request_body = ItemSnooze,
    responses((status = 204, description = "Hidden until then"), (status = 404, description = "Unknown item")),
    security(("bearer" = []))
)]
pub(crate) async fn snooze(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
    Json(snooze): Json<ItemSnooze>,
) -> Result<StatusCode, ApiError> {
    if !state
        .sync
        .snooze(id, Some(snooze.until))
        .await
        .map_err(internal)?
    {
        return Err(not_found("item"));
    }
    state
        .events
        .emit("connected.changed", serde_json::json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// Shows a snoozed item again.
#[utoipa::path(
    delete,
    operation_id = "unsnooze_item",
    path = "/connected/items/{id}/snooze",
    params(("id" = Uuid, Path, description = "Item ID")),
    responses((status = 204, description = "Shown again"), (status = 404, description = "Unknown item")),
    security(("bearer" = []))
)]
pub(crate) async fn unsnooze(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !state.sync.snooze(id, None).await.map_err(internal)? {
        return Err(not_found("item"));
    }
    state
        .events
        .emit("connected.changed", serde_json::json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the splits, the saved filters of the lists.
#[utoipa::path(
    get,
    operation_id = "list_splits",
    path = "/connected/splits",
    responses((status = 200, body = Vec<lighthouse_sync::Split>)),
    security(("bearer" = []))
)]
pub(crate) async fn splits(
    State(state): State<ApiState>,
) -> Result<Json<Vec<lighthouse_sync::Split>>, ApiError> {
    Ok(Json(state.sync.splits().await.map_err(internal)?))
}

/// Creates or replaces a split.
#[utoipa::path(
    put,
    operation_id = "save_split",
    path = "/connected/splits/{id}",
    params(("id" = String, Path, description = "Split ID")),
    request_body = lighthouse_sync::SplitInput,
    responses((status = 200, body = lighthouse_sync::Split), (status = 400, description = "Invalid ID or name")),
    security(("bearer" = []))
)]
pub(crate) async fn save_split(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(input): Json<lighthouse_sync::SplitInput>,
) -> Result<Json<lighthouse_sync::Split>, ApiError> {
    let split = state
        .sync
        .save_split(&id, input)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "a split needs an ID of lowercase letters, digits and dashes, and a name"
                    .to_owned(),
            )
        })?;
    Ok(Json(split))
}

/// Deletes a split.
#[utoipa::path(
    delete,
    operation_id = "delete_split",
    path = "/connected/splits/{id}",
    params(("id" = String, Path, description = "Split ID")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown split")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_split(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !state.sync.delete_split(&id).await.map_err(internal)? {
        return Err(not_found("split"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the items of a split.
#[utoipa::path(
    get,
    operation_id = "split_items",
    path = "/connected/splits/{id}/items",
    params(
        ("id" = String, Path, description = "Split ID"),
        ("limit" = Option<i64>, Query, description = "Page size"),
        ("offset" = Option<i64>, Query, description = "Items to skip")
    ),
    responses((status = 200, body = ItemPage), (status = 404, description = "Unknown split")),
    security(("bearer" = []))
)]
pub(crate) async fn split_items(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ItemPage>, ApiError> {
    let split = state
        .sync
        .split(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found("split"))?;
    let filter = split.item_filter(
        query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500),
        query.offset.unwrap_or(0).max(0),
    );
    page(&state, filter).await
}

/// The same action on many items.
#[derive(Deserialize, ToSchema)]
pub struct BulkAction {
    /// Item IDs.
    pub ids: Vec<Uuid>,
    /// Action name, such as `archive`.
    pub action: String,
    /// Values for `{{input.<field>}}`.
    #[serde(default)]
    pub input: Value,
}

/// What became of one item of a bulk action.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct BulkOutcome {
    /// Item ID.
    pub id: Uuid,
    /// The result, if the action could be asked for.
    pub result: Option<ActionResult>,
    /// Why not.
    pub error: Option<String>,
}

/// Asks for the same action on many items, such as archiving a batch of mail.
/// Every item gets its own approval when the tool writes; decide them together
/// with `POST /approvals/decisions`.
#[utoipa::path(
    post,
    operation_id = "bulk_action",
    path = "/connected/actions/bulk",
    request_body = BulkAction,
    responses((status = 200, body = Vec<BulkOutcome>)),
    security(("bearer" = []))
)]
pub(crate) async fn bulk(
    State(state): State<ApiState>,
    Json(bulk): Json<BulkAction>,
) -> Result<Json<Vec<BulkOutcome>>, ApiError> {
    let mut outcomes = Vec::new();
    for id in bulk.ids.into_iter().take(200) {
        let outcome = match state.sync.item(id).await.map_err(internal)? {
            None => BulkOutcome {
                id,
                result: None,
                error: Some("unknown item".to_owned()),
            },
            Some(item) => match state
                .connected
                .act(&item.source, &bulk.action, Some(&item), bulk.input.clone())
                .await
            {
                Ok(result) => BulkOutcome {
                    id,
                    result: Some(result),
                    error: None,
                },
                Err(error) => BulkOutcome {
                    id,
                    result: None,
                    error: Some(error.to_string()),
                },
            },
        };
        outcomes.push(outcome);
    }
    state.events.emit(
        "connected.changed",
        serde_json::json!({"bulk": outcomes.len()}),
    );
    Ok(Json(outcomes))
}

/// Parameters of scheduling suggestions.
#[derive(Deserialize)]
pub(crate) struct SuggestionQuery {
    minutes: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    day_start: Option<String>,
    day_end: Option<String>,
    weekends: Option<bool>,
    limit: Option<usize>,
}

fn working_hours(
    query: &SuggestionQuery,
) -> Result<lighthouse_sync::scheduling::WorkingHours, ApiError> {
    let time = |text: Option<&str>, default: &str| {
        chrono::NaiveTime::parse_from_str(text.unwrap_or(default), "%H:%M")
            .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "times look like 09:00".to_owned()))
    };
    Ok(lighthouse_sync::scheduling::WorkingHours {
        start: time(query.day_start.as_deref(), "09:00")?,
        end: time(query.day_end.as_deref(), "18:00")?,
        weekends: query.weekends.unwrap_or(false),
    })
}

async fn suggest(state: &ApiState, query: &SuggestionQuery) -> Result<Vec<BusyBlock>, ApiError> {
    let from = query.from.unwrap_or_else(Utc::now);
    let to = query.to.unwrap_or(from + Duration::days(7));
    let minutes = query.minutes.unwrap_or(60).clamp(15, 8 * 60);
    let busy = state.sync.busy(from, to).await.map_err(internal)?;
    let slots = lighthouse_sync::scheduling::free_slots(
        &busy,
        from,
        to,
        Duration::minutes(minutes),
        working_hours(query)?,
        &state.zone,
        query.limit.unwrap_or(5).clamp(1, 50),
    );
    Ok(slots
        .into_iter()
        .map(|(starts_at, ends_at)| BusyBlock { starts_at, ends_at })
        .collect())
}

/// Suggests free slots from the calendar, within working hours in the owner's time zone.
#[utoipa::path(
    get,
    operation_id = "suggest_times",
    path = "/calendar/suggestions",
    params(
        ("minutes" = Option<i64>, Query, description = "Length, default 60"),
        ("from" = Option<DateTime<Utc>>, Query, description = "Start, defaults to now"),
        ("to" = Option<DateTime<Utc>>, Query, description = "End, defaults to seven days later"),
        ("day_start" = Option<String>, Query, description = "Start of working hours, default 09:00"),
        ("day_end" = Option<String>, Query, description = "End of working hours, default 18:00"),
        ("weekends" = Option<bool>, Query, description = "Also Saturday and Sunday"),
        ("limit" = Option<usize>, Query, description = "Most suggestions, default 5")
    ),
    responses((status = 200, body = Vec<BusyBlock>)),
    security(("bearer" = []))
)]
pub(crate) async fn suggestions(
    State(state): State<ApiState>,
    Query(query): Query<SuggestionQuery>,
) -> Result<Json<Vec<BusyBlock>>, ApiError> {
    Ok(Json(suggest(&state, &query).await?))
}

/// A focus block to put into the calendar.
#[derive(Deserialize, ToSchema)]
pub struct FocusBlock {
    /// Calendar source with a `create` action.
    pub source: String,
    /// Title. Defaults to "Focus".
    #[serde(default)]
    pub title: Option<String>,
    /// Start. Without it, the first free slot is taken.
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    /// Length in minutes, default 90.
    #[serde(default)]
    pub minutes: Option<i64>,
}

/// Blocks focus time through the source's `create` action. The input has
/// `title`, `start`, `end` and `focus: true`; a writing tool waits for approval.
#[utoipa::path(
    post,
    operation_id = "block_focus",
    path = "/calendar/focus",
    request_body = FocusBlock,
    responses(
        (status = 202, body = ActionResult),
        (status = 200, body = ActionResult),
        (status = 409, description = "No free slot in the next seven days"),
        (status = 422, description = "The source has no create action")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn focus(
    State(state): State<ApiState>,
    Json(block): Json<FocusBlock>,
) -> Result<(StatusCode, Json<ActionResult>), ApiError> {
    let minutes = block.minutes.unwrap_or(90).clamp(15, 8 * 60);
    let starts_at = match block.starts_at {
        Some(start) => start,
        None => {
            let query = SuggestionQuery {
                minutes: Some(minutes),
                from: None,
                to: None,
                day_start: None,
                day_end: None,
                weekends: None,
                limit: Some(1),
            };
            suggest(&state, &query)
                .await?
                .first()
                .map(|slot| slot.starts_at)
                .ok_or_else(|| {
                    ApiError(
                        StatusCode::CONFLICT,
                        "no free slot in the next seven days".to_owned(),
                    )
                })?
        }
    };
    let input = serde_json::json!({
        "title": block.title.unwrap_or_else(|| "Focus".to_owned()),
        "start": starts_at,
        "end": starts_at + Duration::minutes(minutes),
        "focus": true,
    });
    let result = state
        .connected
        .act(&block.source, "create", None, input)
        .await
        .map_err(action_error)?;
    Ok(acted(&state, result))
}

#[cfg(test)]
mod workflow_tests {
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
    async fn items_snooze_splits_bulk_and_scheduling() {
        let harness = harness(Some(TOKEN)).await;
        let app = &harness.app;
        harness
            .sync
            .upsert(
                "mail",
                "mail",
                "thread",
                &[
                    item("t1", "Weekly newsletter", &["unread"], "inbox"),
                    item("t2", "Offer", &["important"], "inbox"),
                ],
                &[],
            )
            .await
            .unwrap();
        let (_, body) = call(app, "GET", "/mail/threads", Some(TOKEN)).await;
        let page: ItemPage = serde_json::from_str(&body).unwrap();
        let id = page
            .items
            .iter()
            .find(|item| item.external_id == "t1")
            .unwrap()
            .id;
        let uri = format!("/connected/items/{id}/snooze");
        let until = json!({"until": Utc::now() + Duration::hours(4)});
        assert_eq!(
            send(app, "POST", &uri, Some(TOKEN), Some(until.clone()))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, body) = call(app, "GET", "/mail/threads", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<ItemPage>(&body).unwrap().total, 1);
        let (_, body) = call(app, "GET", "/mail/threads?snoozed=true", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<ItemPage>(&body).unwrap().total, 2);
        assert_eq!(
            call(app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        let missing = format!("/connected/items/{}/snooze", Uuid::nil());
        assert_eq!(
            send(app, "POST", &missing, Some(TOKEN), Some(until))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", &missing, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );

        let split = json!({"name": "Newsletters", "domain": "mail", "filter": {"q": "newsletter"}});
        assert_eq!(
            send(
                app,
                "PUT",
                "/connected/splits/newsletters",
                Some(TOKEN),
                Some(split)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            send(
                app,
                "PUT",
                "/connected/splits/Bad",
                Some(TOKEN),
                Some(json!({"name": "x"}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, body) = call(
            app,
            "GET",
            "/connected/splits/newsletters/items",
            Some(TOKEN),
        )
        .await;
        assert_eq!(serde_json::from_str::<ItemPage>(&body).unwrap().total, 1);
        assert!(
            call(app, "GET", "/connected/splits", Some(TOKEN))
                .await
                .1
                .contains("Newsletters")
        );
        assert_eq!(
            call(app, "GET", "/connected/splits/nope/items", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "DELETE", "/connected/splits/newsletters", Some(TOKEN))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(app, "DELETE", "/connected/splits/newsletters", Some(TOKEN))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let ids: Vec<Uuid> = page
            .items
            .iter()
            .map(|item| item.id)
            .chain([Uuid::nil()])
            .collect();
        let bulk = json!({"ids": ids, "action": "archive"});
        let (status, body) = send(
            app,
            "POST",
            "/connected/actions/bulk",
            Some(TOKEN),
            Some(bulk),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let outcomes: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0]["result"]["status"], "proposed");
        assert_eq!(outcomes[2]["error"], "unknown item");
        let failing = json!({"ids": [page.items[0].id], "action": "nope"});
        let (_, body) = send(
            app,
            "POST",
            "/connected/actions/bulk",
            Some(TOKEN),
            Some(failing),
        )
        .await;
        assert!(body.contains("has no nope"));

        let (status, body) = call(
            app,
            "GET",
            "/calendar/suggestions?minutes=30&weekends=true&limit=2",
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Vec<BusyBlock>>(&body).unwrap().len(),
            2
        );
        assert_eq!(
            call(
                app,
                "GET",
                "/calendar/suggestions?day_start=nine",
                Some(TOKEN)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let focus = json!({"source": "cal", "starts_at": Utc::now(), "minutes": 60});
        let (status, body) = send(app, "POST", "/calendar/focus", Some(TOKEN), Some(focus)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let auto = json!({"source": "cal"});
        let (status, _) = send(app, "POST", "/calendar/focus?x=1", Some(TOKEN), Some(auto)).await;
        assert!(status == StatusCode::ACCEPTED || status == StatusCode::CONFLICT);
    }
}
