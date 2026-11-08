//! "Ready for you": everything that waits for the owner, in one call.

use crate::ready_view::{Preview, Review, Reviewer};
use crate::{ApiError, ApiState, Scoped, desk_error, internal};
use axum::Json;
use axum::extract::Path;
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
    /// Who it is for, the service, why, the text and the item it answers.
    #[serde(flatten)]
    pub review: Review,
    /// What the owner can do: `approve`, `edit`, `rewrite`, `snooze`, `deny`,
    /// `dismiss`, `accept`, `confirm` and `context`.
    pub actions: Vec<String>,
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

fn actions(names: &[&str], review: &Review) -> Vec<String> {
    let mut actions: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    if review.context_item_id.is_some() {
        actions.push("context".to_owned());
    }
    actions
}

/// Builds a reviewer with the sources and organizations of this moment.
pub(crate) async fn reviewer(state: &ApiState) -> Reviewer<'_> {
    let sources = state.connected.sources().await.unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot list the sources for the feed");
        Vec::new()
    });
    let mut orgs = std::collections::HashMap::new();
    match state.sync.organizations().await {
        Ok(organizations) => {
            for org in organizations {
                for source in org.sources {
                    orgs.insert(source, org.name.clone());
                }
            }
        }
        Err(error) => tracing::warn!(%error, "cannot list the organizations for the feed"),
    }
    Reviewer::new(&state.sync, sources, orgs, state.publisher.service())
}

/// The tools a run used successfully before `except`, in order.
pub(crate) async fn run_tools(state: &ApiState, run: Uuid, except: Option<&str>) -> Vec<String> {
    match state.audit.run_detail(run).await {
        Ok(Some(detail)) => detail
            .steps
            .into_iter()
            .filter(|step| step.kind == "tool" && step.status == "ok")
            .map(|step| step.name)
            .filter(|name| Some(name.as_str()) != except)
            .collect(),
        Ok(None) => Vec::new(),
        Err(error) => {
            tracing::warn!(%error, "cannot read the steps of a run");
            Vec::new()
        }
    }
}

/// What a run read, readable, such as `Mail · 3 threads`.
#[utoipa::path(
    get,
    operation_id = "run_basis",
    path = "/runs/{id}/basis",
    params(("id" = Uuid, Path, description = "Run ID")),
    responses((status = 200, body = Vec<crate::ready_view::Basis>)),
    security(("bearer" = []))
)]
pub(crate) async fn run_basis(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<crate::ready_view::Basis>>, ApiError> {
    let tools = run_tools(&state, id, None).await;
    Ok(Json(reviewer(&state).await.basis(&tools).await))
}

/// The arguments of a prepared draft, from its logged step.
fn draft_input(detail: Option<&str>) -> Value {
    detail
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
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
pub(crate) async fn feed(Scoped(state): Scoped) -> Result<Json<ReadyFeed>, ApiError> {
    let reviewer = reviewer(&state).await;
    let mut items = Vec::new();
    for approval in state
        .audit
        .list_approvals(Some("pending"), LIMIT)
        .await
        .map_err(internal)?
    {
        let mut review = reviewer
            .call(&approval.tool, &approval.input, approval.reason.as_deref())
            .await;
        review.consulted = reviewer.basis(&approval.consulted).await;
        items.push(ReadyItem {
            kind: "approval".to_owned(),
            group: "decisions".to_owned(),
            id: approval.id.to_string(),
            title: review
                .summary
                .clone()
                .unwrap_or_else(|| format!("{} proposes {}", approval.agent_id, approval.tool)),
            actions: actions(&["approve", "edit", "rewrite", "snooze", "deny"], &review),
            review,
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
            review: Review {
                summary: Some(format!("{} in {}", request.tool, request.project)),
                service: Some("claude-code".to_owned()),
                ..Review::default()
            },
            actions: actions(&["approve", "deny"], &Review::default()),
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
        let mut calls = state
            .audit
            .tool_calls(&tools, since, LIMIT)
            .await
            .map_err(internal)?;
        // Only the newest draft per tool and target: a newer one replaces it.
        calls.sort_by_key(|call| std::cmp::Reverse(call.created_at));
        let mut targets = std::collections::HashSet::new();
        for call in calls
            .into_iter()
            .filter(|call| !dismissed.contains(&call.id.to_string()))
        {
            let input = draft_input(call.detail.as_deref());
            if let Some(target) = lighthouse_core::proposal::target(&input)
                && !targets.insert((call.tool.clone(), target))
            {
                continue;
            }
            let mut review = reviewer.call(&call.tool, &input, None).await;
            review.consulted = reviewer
                .basis(&run_tools(&state, call.run_id, Some(&call.tool)).await)
                .await;
            items.push(ReadyItem {
                kind: DRAFT.to_owned(),
                group: "drafts".to_owned(),
                id: call.id.to_string(),
                title: review
                    .summary
                    .clone()
                    .unwrap_or_else(|| format!("{} prepared {}", call.agent_id, call.tool)),
                actions: actions(&["rewrite", "snooze", "dismiss"], &review),
                review,
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
            review: Review {
                summary: task.notes.clone(),
                reason: task.notes.clone(),
                service: task.source.as_ref().map(|source| source.kind.clone()),
                ..Review::default()
            },
            actions: actions(&["accept", "snooze", "dismiss"], &Review::default()),
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
            review: Review {
                summary: fact.source.label.clone(),
                reason: fact.source.label.clone(),
                ..Review::default()
            },
            actions: actions(&["confirm", "snooze", "dismiss"], &Review::default()),
            agent_id: fact.agent_id.clone(),
            run_id: fact.run_id,
            created_at: fact.created_at,
            expires_at: None,
            detail: to_value(&fact),
        });
    }
    for post in state.desk.proposed_posts().await.map_err(desk_error)? {
        let title = post
            .title
            .clone()
            .unwrap_or_else(|| post.body.lines().next().unwrap_or_default().to_owned());
        items.push(ReadyItem {
            kind: "post".to_owned(),
            group: "suggestions".to_owned(),
            id: post.id.to_string(),
            review: Review {
                summary: Some(title.clone()),
                reason: post.reason.clone(),
                service: reviewer.post_service(&post.channel),
                preview: Some(Preview {
                    fields: Vec::new(),
                    text: Some(post.body.clone()),
                    text_key: Some("body".to_owned()),
                }),
                ..Review::default()
            },
            actions: actions(
                &["accept", "edit", "rewrite", "snooze", "dismiss"],
                &Review::default(),
            ),
            title,
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
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
    Scoped(state): Scoped,
    Path((kind, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    if !state.desk.unsnooze(&kind, &id).await.map_err(desk_error)? {
        return Err(crate::not_found("hidden item"));
    }
    state.events.emit("ready.changed", json!({"id": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// How to rewrite a draft.
#[derive(Deserialize, ToSchema)]
pub struct ToneRequest {
    /// `shorter`, `warmer`, `formal`, or an instruction of your own.
    pub tone: String,
}

/// A rewritten draft. Nothing is saved; edit the item to keep it.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Rewritten {
    /// The tone asked for.
    pub tone: String,
    /// The new text.
    pub text: String,
    /// The run that wrote it.
    pub run_id: Uuid,
}

/// The owner's version of a draft text.
#[derive(Deserialize, ToSchema)]
pub struct TextEdit {
    /// The new text.
    pub text: String,
}

fn tone_instruction(tone: &str) -> String {
    match tone.trim() {
        "shorter" => "Make it shorter: keep the point and the facts, cut the rest.".to_owned(),
        "warmer" => "Make it warmer and more personal, without exaggerating.".to_owned(),
        "formal" => "Make it more formal and precise.".to_owned(),
        other => other.to_owned(),
    }
}

async fn pending_approval(
    state: &ApiState,
    id: Uuid,
) -> Result<lighthouse_audit::Approval, ApiError> {
    state
        .audit
        .list_approvals(Some("pending"), 500)
        .await
        .map_err(internal)?
        .into_iter()
        .find(|approval| approval.id == id)
        .ok_or_else(|| crate::not_found("open approval"))
}

/// The text of an item of the feed that has one.
async fn draft_text(state: &ApiState, kind: &str, id: &str) -> Result<String, ApiError> {
    let parsed = Uuid::parse_str(id).map_err(|_| crate::not_found("item"))?;
    let reviewer = reviewer(state).await;
    let text = match kind {
        "approval" => {
            let approval = pending_approval(state, parsed).await?;
            reviewer
                .call(&approval.tool, &approval.input, None)
                .await
                .preview
                .and_then(|preview| preview.text)
        }
        DRAFT => {
            let tools = draft_tools(state).await;
            let since = Utc::now() - Duration::days(DRAFT_DAYS);
            let call = state
                .audit
                .tool_calls(&tools, since, LIMIT)
                .await
                .map_err(internal)?
                .into_iter()
                .find(|call| call.id == parsed)
                .ok_or_else(|| crate::not_found("draft"))?;
            let input = draft_input(call.detail.as_deref());
            reviewer
                .call(&call.tool, &input, None)
                .await
                .preview
                .and_then(|preview| preview.text)
        }
        "post" => state
            .desk
            .post(parsed)
            .await
            .map_err(desk_error)?
            .map(|post| post.body),
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("`{other}` items have no text to rewrite"),
            ));
        }
    };
    text.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "this item has no text to rewrite".to_owned(),
        )
    })
}

/// Writes a variant of a draft's text in the owner's voice, such as a shorter
/// or warmer one. Works for approvals, drafts and proposed posts.
#[utoipa::path(
    post,
    operation_id = "rewrite_ready_item",
    path = "/ready/{kind}/{id}/rewrite",
    params(
        ("kind" = String, Path, description = "approval, draft or post"),
        ("id" = String, Path, description = "Item ID")
    ),
    request_body = ToneRequest,
    responses(
        (status = 200, body = Rewritten),
        (status = 400, description = "A kind without text"),
        (status = 404, description = "Unknown or decided item"),
        (status = 422, description = "The item has no text"),
        (status = 502, description = "The writer failed")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn rewrite(
    Scoped(state): Scoped,
    Path((kind, id)): Path<(String, String)>,
    Json(request): Json<ToneRequest>,
) -> Result<Json<Rewritten>, ApiError> {
    if request.tone.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "`tone` must not be empty".to_owned(),
        ));
    }
    let text = draft_text(&state, &kind, &id).await?;
    let instructions = format!(
        "Rewrite this draft for the owner, in their voice. {} Keep its meaning, facts and language. \
         Answer with the new text only.",
        tone_instruction(&request.tone)
    );
    let written = state
        .writer
        .write(&instructions, &text)
        .await
        .map_err(|error| ApiError(StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(Rewritten {
        tone: request.tone,
        text: written.text,
        run_id: written.run_id,
    }))
}

/// Replaces the text of an open approval with the owner's version, so the
/// approved call sends that. The other arguments stay as proposed.
#[utoipa::path(
    put,
    operation_id = "edit_ready_approval_text",
    path = "/ready/approval/{id}/text",
    params(("id" = Uuid, Path, description = "Approval ID")),
    request_body = TextEdit,
    responses(
        (status = 200, body = lighthouse_audit::Approval),
        (status = 404, description = "Unknown or decided approval"),
        (status = 422, description = "The call has no text")
    ),
    security(("bearer" = []))
)]
pub(crate) async fn edit_text(
    Scoped(state): Scoped,
    Path(id): Path<Uuid>,
    Json(edit): Json<TextEdit>,
) -> Result<Json<lighthouse_audit::Approval>, ApiError> {
    let approval = pending_approval(&state, id).await?;
    let key = reviewer(&state)
        .await
        .call(&approval.tool, &approval.input, None)
        .await
        .preview
        .and_then(|preview| preview.text_key)
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "this call has no text to edit".to_owned(),
            )
        })?;
    let mut input = approval.input;
    input[key.as_str()] = Value::String(edit.text);
    let edited = state
        .audit
        .edit_approval(id, &input)
        .await
        .map_err(internal)?
        .ok_or_else(|| crate::not_found("open approval"))?;
    state.events.emit("ready.changed", json!({"id": id}));
    Ok(Json(edited))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{TOKEN, call, harness, send};
    use lighthouse_audit::{NewApproval, NewStep};
    use lighthouse_core::RunId;
    use lighthouse_desk::{NewTask, ProposedFact, Proposer};
    use lighthouse_sessions::NewPermission;

    #[tokio::test]
    async fn approvals_read_like_messages_and_can_be_rewritten_and_edited() {
        let harness = harness(Some(TOKEN)).await;
        let original = lighthouse_sync::MappedItem {
            external_id: "t1".to_owned(),
            title: Some("Updated offer?".to_owned()),
            author: Some("John Roe <john@globex.example>".to_owned()),
            summary: Some("Could you send the offer by Friday?".to_owned()),
            occurred_at: Some(Utc::now()),
            data: json!({}),
            ..lighthouse_sync::MappedItem::default()
        };
        harness
            .sync
            .upsert("mail", "mail", "thread", &[original], &[])
            .await
            .unwrap();
        let run = RunId::new();
        harness
            .audit
            .start_run(run, "inbox", "manual")
            .await
            .unwrap();
        let id = Uuid::now_v7();
        let proposal = NewApproval {
            id,
            tool: "mail.send_reply".to_owned(),
            input: json!({"thread_id": "t1", "body": "Hi John,\nhere it is.", "_reason": "John asked by Friday."}),
            quotes_external: true,
            reason: Some("John asked by Friday.".to_owned()),
            consulted: vec![
                "mail.list_threads".to_owned(),
                "posts.propose".to_owned(),
                "mail.archive".to_owned(),
                "mail.send_reply".to_owned(),
                "other.lookup_thing".to_owned(),
            ],
        };
        harness
            .audit
            .pause_run(run, "inbox", &json!({}), &[proposal])
            .await
            .unwrap();

        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        let feed: Value = serde_json::from_str(&body).unwrap();
        let item = &feed["items"][0];
        assert_eq!(item["title"], "Re: Updated offer?", "{body}");
        assert_eq!(item["reason"], "John asked by Friday.");
        assert_eq!(item["service"], "gmail");
        assert_eq!(item["counterpart"]["name"], "John Roe");
        assert_eq!(item["counterpart"]["org"], "Globex");
        assert_eq!(item["quote"]["text"], "Could you send the offer by Friday?");
        assert!(item["context_item_id"].is_string());
        assert_eq!(item["preview"]["text"], "Hi John,\nhere it is.");
        assert_eq!(item["preview"]["fields"], json!([]));
        assert!(
            item["actions"]
                .as_array()
                .unwrap()
                .contains(&json!("context"))
        );
        let labels: Vec<&str> = item["consulted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|basis| basis["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["Gmail · 1 thread", "Archive", "Lookup thing"]);

        let uri = format!("/ready/approval/{id}/rewrite");
        let (status, body) = send(
            &harness.app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"tone": "shorter"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rewritten: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(rewritten["text"], "rewritten: Hi John,\nhere it is.");
        assert_eq!(
            tone_instruction("warmer"),
            "Make it warmer and more personal, without exaggerating."
        );
        assert_eq!(
            tone_instruction("formal"),
            "Make it more formal and precise."
        );
        assert_eq!(tone_instruction("In German"), "In German");
        let (status, _) = send(
            &harness.app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"tone": " "})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let task_uri = format!("/ready/task/{id}/rewrite");
        let (status, _) = send(
            &harness.app,
            "POST",
            &task_uri,
            Some(TOKEN),
            Some(json!({"tone": "shorter"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let unknown = format!("/ready/approval/{}/rewrite", Uuid::now_v7());
        let (status, _) = send(
            &harness.app,
            "POST",
            &unknown,
            Some(TOKEN),
            Some(json!({"tone": "shorter"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let uri = format!("/ready/approval/{id}/text");
        let (status, body) = send(
            &harness.app,
            "PUT",
            &uri,
            Some(TOKEN),
            Some(json!({"text": "Hi John, attached."})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let approval: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(approval["input"]["body"], "Hi John, attached.");
        assert_eq!(approval["input"]["thread_id"], "t1");
    }

    #[tokio::test]
    async fn only_the_newest_draft_per_target_is_shown_with_what_its_run_read() {
        let harness = harness(Some(TOKEN)).await;
        let audit = &harness.audit;
        let run = RunId::new();
        audit.start_run(run, "inbox", "manual").await.unwrap();
        let tool_step = |name: &str, detail: &str| NewStep {
            kind: "tool".to_owned(),
            name: name.to_owned(),
            status: "ok".to_owned(),
            detail: Some(detail.to_owned()),
            ..NewStep::default()
        };
        audit
            .record_step(run, &tool_step("mail.list_threads", "{}"))
            .await
            .unwrap();
        for body in ["First", "Second"] {
            let detail = json!({"thread_id": "t1", "title": body}).to_string();
            audit
                .record_step(run, &tool_step("linear.save_draft", &detail))
                .await
                .unwrap();
        }
        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        let feed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(feed["counts"]["draft"], 1, "{body}");
        let draft = &feed["items"][0];
        assert_eq!(draft["title"], "Second");
        assert_eq!(draft["consulted"][0]["label"], "Gmail · 0 threads");
        let uri = format!("/runs/{}/basis", run.as_uuid());
        let (status, body) = call(&harness.app, "GET", &uri, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        let basis: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(basis.as_array().unwrap().len(), 2, "each tool once: {body}");
    }

    #[tokio::test]
    async fn posts_and_drafts_show_their_text_and_service() {
        let harness = harness(Some(TOKEN)).await;
        let post = harness
            .desk
            .propose_post(lighthouse_desk::NewPost {
                body: "Late invoices have one boring cause.".to_owned(),
                reason: Some("Your last post on invoices did well.".to_owned()),
                ..lighthouse_desk::NewPost::default()
            })
            .await
            .unwrap();
        let (_, body) = call(&harness.app, "GET", "/ready", Some(TOKEN)).await;
        let feed: Value = serde_json::from_str(&body).unwrap();
        let item = &feed["items"][0];
        assert_eq!(item["service"], "linkedin", "{body}");
        assert_eq!(item["reason"], "Your last post on invoices did well.");
        assert_eq!(
            item["preview"]["text"],
            "Late invoices have one boring cause."
        );
        let uri = format!("/ready/post/{}/rewrite", post.id);
        let (status, body) = send(
            &harness.app,
            "POST",
            &uri,
            Some(TOKEN),
            Some(json!({"tone": "warmer"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("rewritten: Late invoices"));
        let draft = format!("/ready/draft/{}/rewrite", Uuid::now_v7());
        let (status, _) = send(
            &harness.app,
            "POST",
            &draft,
            Some(TOKEN),
            Some(json!({"tone": "warmer"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(draft_input(Some("not json")), Value::Null);
    }

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
            feed.items[0].review.reason.as_deref(),
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
