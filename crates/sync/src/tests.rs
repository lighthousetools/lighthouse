//! Refreshes against a scripted tool host and a real database.

use crate::*;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use lighthouse_config::SourceConfig;
use lighthouse_core::{RunId, TenantId};
use lighthouse_mcp::{McpError, ToolHost, ToolHostFactory, ToolOutput};
use lighthouse_store::testing::fresh_store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Answers tools from a table that tests can change between refreshes.
#[derive(Clone, Default)]
struct Script {
    answers: Arc<Mutex<HashMap<String, Value>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Script {
    fn set(&self, tool: &str, answer: Value) {
        self.answers.lock().unwrap().insert(tool.to_owned(), answer);
    }
}

#[async_trait]
impl ToolHost for Script {
    async fn list_tools(&self, _: &[String]) -> Result<Vec<lighthouse_router::ToolSpec>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), arguments.clone()));
        let answers = self.answers.lock().unwrap();
        let key = match arguments.get("pullNumber") {
            Some(number) => format!("{tool}#{number}"),
            None => tool.to_owned(),
        };
        match answers.get(&key).or_else(|| answers.get(tool)) {
            Some(Value::String(text)) => Ok(ToolOutput {
                text: text.clone(),
                is_error: text.starts_with("error"),
            }),
            Some(answer) => Ok(ToolOutput {
                text: answer.to_string(),
                is_error: false,
            }),
            None => Err(McpError::Call {
                connector: "github".to_owned(),
                detail: format!("no answer for {key}"),
            }),
        }
    }
}

#[async_trait]
impl ToolHostFactory for Script {
    async fn open(&self, _: &[String], _: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        Ok(Box::new(self.clone()))
    }
}

fn pull(number: u32, author: &str) -> Value {
    json!({"number": number, "title": format!("Change {number}"), "user": {"login": author},
           "state": "open", "html_url": format!("https://github.com/acme/app/pull/{number}"),
           "repository_url": "https://api.github.com/repos/acme/app",
           "updated_at": Utc::now().to_rfc3339(), "body": "Please review"})
}

async fn github() -> (Syncer, Script) {
    let config = SourceConfig {
        connector: "github".to_owned(),
        preset: Some("github".to_owned()),
        ..SourceConfig::default()
    };
    let source = resolve("github", &config, vec!["alex".to_owned()]).unwrap();
    let script = Script::default();
    let store = SyncStore::new(fresh_store().await, TenantId::SINGLE);
    (
        Syncer::new(store, Arc::new(script.clone()), vec![source]),
        script,
    )
}

#[tokio::test]
async fn pull_requests_checks_and_files_sync_with_events_after_the_first_run() {
    let (syncer, script) = github().await;
    script.set(
        "github.search_pull_requests",
        json!({"items": [pull(1, "alex")]}),
    );
    script.set(
        "github.get_pull_request_status",
        json!({"statuses": [{"context": "ci", "state": "success"}]}),
    );
    script.set("github.get_pull_request_files", json!([{"filename": "src/main.rs", "status": "modified",
                                                        "additions": 3, "deletions": 1, "patch": "@@"}]));
    let now = Utc::now();
    let first = syncer.sync("github", now).await.unwrap();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(first.events.is_empty(), "the first refresh is quiet");
    let calls = script.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|(_, input)| input["query"] == "is:open is:pr review-requested:alex")
    );
    assert!(
        calls
            .iter()
            .any(|(_, input)| input["pullNumber"] == 1 && input["owner"] == "acme")
    );

    let store = syncer.store();
    let pr = store
        .item_by_external("github", "pull_request", "acme/app/pull/1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pr.flags,
        ["mine", "review_requested"],
        "both collections returned it"
    );
    let children = store.children(&pr).await.unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].kind, "check");
    assert_eq!(children[1].data["additions"], 3);

    script.set(
        "github.search_pull_requests",
        json!({"items": [pull(1, "alex"), pull(2, "sam")]}),
    );
    script.set(
        "github.get_pull_request_status",
        json!({"statuses": [{"context": "ci", "state": "failure"}]}),
    );
    let second = syncer
        .sync("github", now + Duration::minutes(20))
        .await
        .unwrap();
    let names: Vec<&str> = second
        .events
        .iter()
        .map(|event| event.name.as_str())
        .collect();
    assert!(names.contains(&"review.requested"), "{names:?}");
    assert!(names.contains(&"check.failed"), "{names:?}");
    let failed = second
        .checks
        .iter()
        .find(|check| check.key.contains("pull/1/ci"))
        .unwrap();
    assert!(failed.failed && failed.on_my_pull_request);

    script.set("github.search_pull_requests", json!({"items": []}));
    script.set(
        "github.get_pull_request_status",
        json!({"statuses": [{"context": "ci", "state": "success"}]}),
    );
    let third = syncer
        .sync("github", now + Duration::minutes(40))
        .await
        .unwrap();
    assert!(
        third.checks.iter().any(|check| !check.failed),
        "a recovered check is reported"
    );
    let pr = store
        .item_by_external("github", "pull_request", "acme/app/pull/2")
        .await
        .unwrap()
        .unwrap();
    assert!(
        pr.flags.is_empty(),
        "flags of a full collection go when it no longer returns the item"
    );

    let statuses = store.statuses().await.unwrap();
    assert!(statuses[0].last_error.is_none());
    assert!(syncer.due(Utc::now()).await.unwrap().is_empty());
    assert_eq!(
        syncer.due(Utc::now() + Duration::hours(2)).await.unwrap(),
        ["github"]
    );
}

#[tokio::test]
async fn failures_are_recorded_and_lists_search_and_page() {
    let (syncer, script) = github().await;
    script.set("github.search_pull_requests", json!("error: rate limited"));
    let report = syncer.sync("github", Utc::now()).await.unwrap();
    assert!(!report.errors.is_empty());
    let status = &syncer.store().statuses().await.unwrap()[0];
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap()
            .contains("rate limited")
    );
    assert!(status.last_success_at.is_none());
    assert!(matches!(
        syncer.sync("nope", Utc::now()).await,
        Err(SyncError::UnknownSource(_))
    ));

    script.set(
        "github.search_pull_requests",
        json!({"items": [pull(1, "alex"), pull(2, "sam"), pull(3, "sam")]}),
    );
    script.set("github.get_pull_request_status", json!({"statuses": []}));
    script.set("github.get_pull_request_files", json!([]));
    syncer.sync("github", Utc::now()).await.unwrap();
    let store = syncer.store();
    let filter = ItemFilter {
        domain: Some("code".to_owned()),
        kinds: vec!["pull_request".to_owned()],
        limit: 2,
        ..ItemFilter::default()
    };
    let page = store.items(&filter).await.unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next_offset, Some(2));
    let search = ItemFilter {
        q: Some("change".to_owned()),
        flags: vec!["mine".to_owned()],
        limit: 10,
        ..ItemFilter::default()
    };
    assert_eq!(store.items(&search).await.unwrap().total, 1);
    let open = ItemFilter {
        state: Some("OPEN".to_owned()),
        open: true,
        any_flags: vec!["review_requested".to_owned()],
        order: Order::Importance,
        limit: 10,
        ..ItemFilter::default()
    };
    assert_eq!(store.items(&open).await.unwrap().total, 3);
    let item = &page.items[0];
    assert_eq!(store.item(item.id).await.unwrap().unwrap().id, item.id);
    assert!(store.raw(item.id).await.unwrap().unwrap()["number"].is_number());
}

#[tokio::test]
async fn actions_resolve_to_tool_calls_with_the_item() {
    let (syncer, script) = github().await;
    script.set(
        "github.search_pull_requests",
        json!({"items": [pull(7, "sam")]}),
    );
    script.set("github.get_pull_request_status", json!({"statuses": []}));
    script.set("github.get_pull_request_files", json!([]));
    syncer.sync("github", Utc::now()).await.unwrap();
    let pr = syncer
        .store()
        .item_by_external("github", "pull_request", "acme/app/pull/7")
        .await
        .unwrap()
        .unwrap();
    let action = syncer
        .prepare_action("github", "approve", Some(&pr), &json!({"body": ""}))
        .unwrap();
    assert_eq!(action.tool, "github.create_pull_request_review");
    assert_eq!(
        action.input,
        json!({"owner": "acme", "repo": "app", "pullNumber": 7, "event": "APPROVE"})
    );
    assert!(
        syncer
            .prepare_action("github", "approve", None, &json!({}))
            .is_err()
    );
    assert!(
        syncer
            .prepare_action("github", "merge", Some(&pr), &json!({}))
            .is_err()
    );
    assert!(
        syncer
            .prepare_action("nope", "approve", None, &json!({}))
            .is_err()
    );
    assert_eq!(
        prune_empty(json!({"a": "", "b": {"c": ""}, "d": [1]})),
        json!({"b": {}, "d": [1]})
    );
}

#[tokio::test]
async fn mail_and_calendar_map_events_and_busy_times() {
    let mail = SourceConfig {
        connector: "mail".to_owned(),
        domain: Some("mail".to_owned()),
        collections: toml::from_str(
            r#"
[inbox]
tool = "list_threads"
items = "threads"
kind = "thread"
input = { after = "{{since_date}}" }
[inbox.fields]
external_id = "id"
title = "subject"
author = "from"
summary = "snippet"
occurred_at = "date"
important = { path = "labels", contains = "important" }
"#,
        )
        .unwrap(),
        ..SourceConfig::default()
    };
    let calendar = SourceConfig {
        connector: "cal".to_owned(),
        domain: Some("calendar".to_owned()),
        collections: toml::from_str(
            r#"
[events]
tool = "list_events"
kind = "event"
[events.fields]
external_id = "id"
title = "summary"
starts_at = "start"
ends_at = "end"
state = "status"
"#,
        )
        .unwrap(),
        ..SourceConfig::default()
    };
    let owner = vec!["alex@example.com".to_owned()];
    let sources = vec![
        resolve("mail", &mail, owner.clone()).unwrap(),
        resolve("cal", &calendar, owner).unwrap(),
    ];
    let script = Script::default();
    let store = SyncStore::new(fresh_store().await, TenantId::SINGLE);
    let syncer = Syncer::new(store, Arc::new(script.clone()), sources);
    let now = Utc::now();
    script.set("mail.list_threads", json!({"threads": []}));
    syncer.sync("mail", now).await.unwrap();
    script.set(
        "mail.list_threads",
        json!({"threads": [
            {"id": "t1", "subject": "Offer", "from": "client@example.com", "labels": ["IMPORTANT"],
             "date": now.to_rfc3339(), "snippet": "Can you send an offer?"},
            {"id": "t2", "subject": "Re: Offer", "from": "Alex <alex@example.com>", "date": now.to_rfc3339()}
        ]}),
    );
    let report = syncer.sync("mail", now + Duration::hours(1)).await.unwrap();
    assert_eq!(report.events.len(), 1, "the owner's own mail is no event");
    assert_eq!(report.events[0].name, "mail.received");
    assert_eq!(report.events[0].payload["from"], "client@example.com");

    let start = now + Duration::hours(2);
    script.set(
        "cal.list_events",
        json!([
            {"id": "e1", "summary": "Review", "start": start.to_rfc3339(), "end": (start + Duration::hours(1)).to_rfc3339()},
            {"id": "e2", "summary": "Lunch", "start": (start + Duration::minutes(30)).to_rfc3339(), "end": (start + Duration::hours(2)).to_rfc3339()},
            {"id": "e3", "summary": "Cancelled", "start": (start + Duration::hours(5)).to_rfc3339(), "end": (start + Duration::hours(6)).to_rfc3339(), "status": "cancelled"}
        ]),
    );
    syncer.sync("cal", now).await.unwrap();
    let busy = syncer
        .store()
        .busy(now, now + Duration::days(1))
        .await
        .unwrap();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].1 - busy[0].0, Duration::hours(2));
    let window = ItemFilter {
        domain: Some("calendar".to_owned()),
        from: Some(start + Duration::minutes(90)),
        to: Some(start + Duration::hours(3)),
        order: Order::Start,
        limit: 10,
        ..ItemFilter::default()
    };
    let titles: Vec<_> = syncer
        .store()
        .items(&window)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|item| item.title.unwrap())
        .collect();
    assert_eq!(titles, ["Lunch"]);
}
