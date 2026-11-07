//! "Ready for you": everything that waits for the owner, in one call.

use crate::{ApiError, ApiState, desk_error, internal};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use lighthouse_desk::{AlertCounts, FactFilter, TaskView};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

/// Drafts older than this drop out of the feed.
const DRAFT_DAYS: i64 = 7;
const LIMIT: i64 = 200;
const DRAFT: &str = "draft";
/// Built-in draft tools whose results the owner reviews elsewhere.
const INTERNAL_CONNECTORS: [&str; 3] = ["brain", "tasks", "memory"];

/// One thing that waits for the owner.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ReadyItem {
    /// `approval`, `permission`, `draft`, `task` or `fact`.
    pub kind: String,
    /// `decisions`, `drafts` or `suggestions`.
    pub group: String,
    /// ID of the item within its kind.
    pub id: String,
    /// One line for the list.
    pub title: String,
    /// What the agent said about it, if anything.
    pub summary: Option<String>,
    /// The agent behind it.
    pub agent_id: Option<String>,
    /// The run behind it.
    pub run_id: Option<Uuid>,
    /// Since when it waits.
    pub created_at: DateTime<Utc>,
    /// When it lapses, if it does.
    pub expires_at: Option<DateTime<Utc>>,
    /// The full item, as its own endpoint returns it.
    pub detail: Value,
}

/// Everything that waits for the owner.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ReadyFeed {
    /// Number of items.
    pub total: usize,
    /// Items per kind.
    pub counts: BTreeMap<String, usize>,
    /// Items per group.
    pub groups: BTreeMap<String, usize>,
    /// Unresolved alerts per severity, shown beside the feed.
    pub alerts: AlertCounts,
    /// Decisions first, then drafts, then suggestions; oldest first within a group.
    pub items: Vec<ReadyItem>,
}

fn group_order(group: &str) -> u8 {
    match group {
        "decisions" => 0,
        "drafts" => 1,
        _ => 2,
    }
}

fn to_value(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

async fn draft_tools(state: &ApiState) -> Vec<String> {
    match state.catalog.connectors().await {
        Ok(connectors) => connectors
            .into_iter()
            .filter(|connector| !INTERNAL_CONNECTORS.contains(&connector.name.as_str()))
            .flat_map(|connector| {
                let name = connector.name;
                connector
                    .draft_tools
                    .into_iter()
                    .map(move |tool| format!("{name}.{tool}"))
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "cannot list draft tools");
            Vec::new()
        }
    }
}

/// Collects what waits for the owner: open approvals and permission requests,
/// drafts agents saved in external tools during the last seven days, and tasks
/// and facts agents proposed.
#[utoipa::path(
    get,
    operation_id = "ready_feed",
    path = "/ready",
    responses((status = 200, body = ReadyFeed)),
    security(("bearer" = []))
)]
pub(crate) async fn feed(State(state): State<ApiState>) -> Result<Json<ReadyFeed>, ApiError> {
    let mut items = Vec::new();
    for approval in state
        .audit
        .list_approvals(Some("pending"), LIMIT)
        .await
        .map_err(internal)?
    {
        items.push(ReadyItem {
            kind: "approval".to_owned(),
            group: "decisions".to_owned(),
            id: approval.id.to_string(),
            title: format!("{} proposes {}", approval.agent_id, approval.tool),
            summary: approval.reason.clone(),
            agent_id: Some(approval.agent_id.clone()),
            run_id: Some(approval.run_id),
            created_at: approval.created_at,
            expires_at: Some(approval.expires_at),
            detail: to_value(&approval),
        });
    }
    for request in state
        .sessions
        .requests(Some("pending"), LIMIT)
        .await
        .map_err(internal)?
    {
        items.push(ReadyItem {
            kind: "permission".to_owned(),
            group: "decisions".to_owned(),
            id: request.id.to_string(),
            title: format!(
                "Claude Code asks to use {} in {}",
                request.tool, request.project
            ),
            summary: None,
            agent_id: None,
            run_id: None,
            created_at: request.created_at,
            expires_at: Some(request.expires_at),
            detail: to_value(&request),
        });
    }
    let tools = draft_tools(&state).await;
    if !tools.is_empty() {
        let dismissed = state.desk.dismissed(DRAFT).await.map_err(desk_error)?;
        let since = Utc::now() - Duration::days(DRAFT_DAYS);
        for call in state
            .audit
            .tool_calls(&tools, since, LIMIT)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|call| !dismissed.contains(&call.id.to_string()))
        {
            items.push(ReadyItem {
                kind: DRAFT.to_owned(),
                group: "drafts".to_owned(),
                id: call.id.to_string(),
                title: format!("{} prepared {}", call.agent_id, call.tool),
                summary: None,
                agent_id: Some(call.agent_id.clone()),
                run_id: Some(call.run_id),
                created_at: call.created_at,
                expires_at: None,
                detail: to_value(&call),
            });
        }
    }
    let today = Utc::now().with_timezone(&state.zone).date_naive();
    for task in state
        .desk
        .tasks(TaskView::Proposed, today, LIMIT)
        .await
        .map_err(desk_error)?
    {
        items.push(ReadyItem {
            kind: "task".to_owned(),
            group: "suggestions".to_owned(),
            id: task.id.to_string(),
            title: task.title.clone(),
            summary: task.notes.clone(),
            agent_id: task.agent_id.clone(),
            run_id: task.run_id,
            created_at: task.created_at,
            expires_at: None,
            detail: to_value(&task),
        });
    }
    let proposed = FactFilter {
        status: Some("proposed".to_owned()),
        category: None,
    };
    for fact in state.desk.facts(&proposed).await.map_err(desk_error)? {
        items.push(ReadyItem {
            kind: "fact".to_owned(),
            group: "suggestions".to_owned(),
            id: fact.id.to_string(),
            title: fact.text.clone(),
            summary: fact.source.label.clone(),
            agent_id: fact.agent_id.clone(),
            run_id: fact.run_id,
            created_at: fact.created_at,
            expires_at: None,
            detail: to_value(&fact),
        });
    }
    for post in state.desk.proposed_posts().await.map_err(desk_error)? {
        items.push(ReadyItem {
            kind: "post".to_owned(),
            group: "suggestions".to_owned(),
            id: post.id.to_string(),
            title: post
                .title
                .clone()
                .unwrap_or_else(|| post.body.lines().next().unwrap_or_default().to_owned()),
            summary: None,
            agent_id: None,
            run_id: None,
            created_at: post.created_at,
            expires_at: None,
            detail: to_value(&post),
        });
    }
    let mut hidden = std::collections::HashMap::new();
    for kind in ["approval", "permission", "task", "fact", "post"] {
        hidden.insert(kind, state.desk.dismissed(kind).await.map_err(desk_error)?);
    }
    items.retain(|item| {
        hidden
            .get(item.kind.as_str())
            .is_none_or(|hidden| !hidden.contains(&item.id))
    });
    items.sort_by(|left, right| {
        (group_order(&left.group), left.created_at)
            .cmp(&(group_order(&right.group), right.created_at))
    });
    let mut counts = BTreeMap::new();
    let mut groups = BTreeMap::new();
    for kind in ["approval", "permission", DRAFT, "task", "fact", "post"] {
        counts.insert(kind.to_owned(), 0);
    }
    for group in ["decisions", "drafts", "suggestions"] {
        groups.insert(group.to_owned(), 0);
    }
    for item in &items {
        *counts.entry(item.kind.clone()).or_default() += 1;
        *groups.entry(item.group.clone()).or_default() += 1;
    }
    Ok(Json(ReadyFeed {
        total: items.len(),
        counts,
        groups,
        alerts: state.desk.alert_counts().await.map_err(desk_error)?,
        items,
    }))
}

/// Clears a draft from the feed once the owner has dealt with it in its tool.
/// Approvals, permissions, tasks and facts leave the feed through their own
/// decisions.
#[utoipa::path(
    post,
    operation_id = "dismiss_draft",
    path = "/ready/drafts/{id}/dismiss",
    params(("id" = Uuid, Path, description = "ID of the draft item")),
    responses((status = 204, description = "Cleared")),
    security(("bearer" = []))
)]
pub(crate) async fn dismiss_draft(
    State(state): State<ApiState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .desk
        .dismiss(DRAFT, &id.to_string())
        .await
        .map_err(desk_error)?;
    state.events.emit("ready.changed", json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// When to show an item again.
#[derive(Deserialize, ToSchema)]
pub struct Snooze {
    /// Hide it until this time.
    pub until: DateTime<Utc>,
}

/// Hides an item of the feed until a time, such as a draft for "later". Its
/// decision stays open; approvals still expire after 72 hours.
#[utoipa::path(
    post,
    operation_id = "snooze_ready_item",
    path = "/ready/{kind}/{id}/snooze",
    params(
        ("kind" = String, Path, description = "approval, permission, draft, task, fact or post"),
        ("id" = String, Path, description = "Item ID")
    ),
    request_body = Snooze,
    responses((status = 204, description = "Hidden until then"), (status = 400, description = "Unknown kind")),
    security(("bearer" = []))
)]
pub(crate) async fn snooze(
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
    Json(snooze): Json<Snooze>,
) -> Result<StatusCode, ApiError> {
    if !["approval", "permission", DRAFT, "task", "fact", "post"].contains(&kind.as_str()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown kind `{kind}`"),
        ));
    }
    state
        .desk
        .snooze(&kind, &id, snooze.until)
        .await
        .map_err(desk_error)?;
    state.events.emit("ready.changed", json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// Shows a snoozed or cleared item again.
#[utoipa::path(
    delete,
    operation_id = "unsnooze_ready_item",
    path = "/ready/{kind}/{id}/snooze",
    params(
        ("kind" = String, Path, description = "Item kind"),
        ("id" = String, Path, description = "Item ID")
    ),
    responses((status = 204, description = "Shown again"), (status = 404, description = "Not hidden")),
    security(("bearer" = []))
)]
pub(crate) async fn unsnooze(
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.unsnooze(&kind, &id).await.map_err(desk_error)? {
        return Err(crate::not_found("hidden item"));
    }
    state.events.emit("ready.changed", json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness};
    use lighthouse_audit::{NewApproval, NewStep};
    use lighthouse_core::RunId;
    use lighthouse_desk::{NewTask, ProposedFact, Proposer};
    use lighthouse_sessions::NewPermission;

    #[tokio::test]
    async fn the_feed_gathers_decisions_drafts_and_suggestions() {
        let harness = harness(Some(TOKEN)).await;
        let audit = &harness.audit;
        let run = RunId::new();
        audit.start_run(run, "inbox", "cron").await.unwrap();
        audit
            .record_step(
                run,
                &NewStep {
                    kind: "tool".to_owned(),
                    name: "linear.save_draft".to_owned(),
                    status: "ok".to_owned(),
                    detail: Some("{\"title\":\"Reply\"}".to_owned()),
                    ..NewStep::default()
                },
            )
            .await
            .unwrap();
        let proposal = NewApproval {
            id: Uuid::now_v7(),
            tool: "mail.send".to_owned(),
            input: json!({"to": "someone@example.com"}),
            quotes_external: true,
            reason: Some("The customer asked for a date".to_owned()),
            consulted: Vec::new(),
        };
        audit
            .pause_run(run, "inbox", &json!({}), &[proposal])
            .await
            .unwrap();
        let node = Uuid::now_v7();
        harness
            .sessions
            .report(
                node,
                &lighthouse_sessions::NodeReport {
                    name: "laptop".to_owned(),
                    sessions: Vec::new(),
                },
            )
            .await
            .unwrap();
        harness
            .sessions
            .request(
                node,
                &NewPermission {
                    session_id: "s1".to_owned(),
                    project: "website".to_owned(),
                    tool: "Bash".to_owned(),
                    input: json!({"command": "ls"}),
                },
            )
            .await
            .unwrap();
        let proposer = Proposer::agent("inbox".to_owned(), run.as_uuid());
        harness
            .desk
            .create_task(
                NewTask {
                    title: "Call back".to_owned(),
                    ..NewTask::default()
                },
                Some(proposer.clone()),
            )
            .await
            .unwrap();
        harness
            .desk
            .propose_fact(
                ProposedFact {
                    text: "Prefers calls".to_owned(),
                    confidence: 0.5,
                    ..ProposedFact::default()
                },
                &proposer,
            )
            .await
            .unwrap();

        let (status, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let feed: ReadyFeed = serde_json::from_str(&body).unwrap();
        assert_eq!(feed.total, 5);
        let kinds: Vec<_> = feed.items.iter().map(|item| item.kind.as_str()).collect();
        assert_eq!(kinds, ["approval", "permission", "draft", "task", "fact"]);
        assert_eq!(feed.groups["decisions"], 2);
        assert_eq!(feed.counts["draft"], 1);
        assert_eq!(
            feed.items[0].summary.as_deref(),
            Some("The customer asked for a date")
        );

        let draft = &feed.items[2].id;
        let uri = format!("/ready/drafts/{draft}/dismiss");
        assert_eq!(
            call(&harness.app, "POST", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        let feed: ReadyFeed = serde_json::from_str(&body).unwrap();
        assert_eq!(feed.total, 4);
        assert_eq!(feed.counts["draft"], 0);

        let fact = feed
            .items
            .iter()
            .find(|item| item.kind == "fact")
            .unwrap()
            .id
            .clone();
        let later = json!({"until": Utc::now() + Duration::hours(3)});
        let uri = format!("/ready/fact/{fact}/snooze");
        assert_eq!(
            crate::tests::send(&harness.app, "POST", &uri, Some(TOKEN), Some(later.clone()))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<ReadyFeed>(&body).unwrap().total, 3);
        assert_eq!(
            call(&harness.app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&harness.app, "DELETE", &uri, Some(TOKEN)).await.0,
            StatusCode::NOT_FOUND
        );
        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        assert_eq!(serde_json::from_str::<ReadyFeed>(&body).unwrap().total, 4);
        assert_eq!(
            crate::tests::send(
                &harness.app,
                "POST",
                "/ready/nope/x/snooze",
                Some(TOKEN),
                Some(later)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}
