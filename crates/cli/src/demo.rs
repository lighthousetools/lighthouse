//! Seeds the local demo with fictional data: two companies, tasks, rules,
//! facts, alerts, posts with archive metrics, a deck, a contract template and
//! a conversation. Nothing here describes real people or companies.

use chrono::{Duration, Utc};
use lighthouse_auth::Auth;
use lighthouse_brain::{Brain, NewEntry, Source};
use lighthouse_core::TenantId;
use lighthouse_desk::{
    BrandProfile, Desk, ImportedPost, NewAlert, NewFact, NewPost, NewRule, NewTask, PillarInput,
    PostSlot, ProposedFact, Proposer, Severity, SourceLink,
};
use lighthouse_store::Store;
use lighthouse_sync::{OrganizationInput, SyncStore};
use secrecy::SecretString;
use serde_json::json;
use std::path::Path;
use uuid::Uuid;

/// Password of the demo owner.
pub const DEMO_PASSWORD: &str = "lighthouse-demo";

/// What the seed did.
#[derive(Debug, PartialEq, Eq)]
pub enum Seeded {
    /// The demo data was added.
    Added,
    /// The database had demo data already.
    AlreadyThere,
}

fn app_source(reference: &str) -> Vec<Source> {
    vec![Source {
        kind: "app".to_owned(),
        reference: reference.to_owned(),
    }]
}

fn entry(kind: &str, title: &str, body: &str, links: Vec<String>) -> NewEntry {
    NewEntry {
        kind: kind.to_owned(),
        title: title.to_owned(),
        body: body.to_owned(),
        sources: app_source("demo"),
        links,
    }
}

/// The demo agents to run so the demo looks full: `ready` while the feed of
/// `GET /ready` holds no proposals, `morning-brief` while there is no brief and
/// `weekly-report` while `GET /recaps` is empty. Restarts add no duplicates.
pub fn warmup_agents(
    feed: &serde_json::Value,
    has_brief: bool,
    recaps: &serde_json::Value,
) -> Vec<&'static str> {
    let mut agents = Vec::new();
    if feed["counts"]["approval"].as_u64().unwrap_or(0) == 0 {
        agents.push("ready");
    }
    if !has_brief {
        agents.push("morning-brief");
    }
    if recaps.as_array().is_none_or(Vec::is_empty) {
        agents.push("weekly-report");
    }
    agents
}

/// Whether the demo machine still has to report: while `GET /sessions` is empty.
pub fn needs_node(sessions: &serde_json::Value) -> bool {
    sessions.as_array().is_none_or(Vec::is_empty)
}

/// The demo session that works.
const WORKING: &str = "demo-session-billing";
/// The demo session that waits for a permission.
const WAITING: &str = "demo-session-website";

/// The token request of the demo machine.
pub fn node_token_request() -> serde_json::Value {
    json!({"name": "Demo MacBook", "kind": "node"})
}

/// The report of the demo machine: one session works, one waits.
pub fn node_report() -> serde_json::Value {
    json!({"name": "Demo MacBook", "sessions": [
        {"id": WORKING, "project": "billing", "branch": "feature/csv-export", "status": "running",
         "summary": "Adds the CSV export of invoices", "cost_eur": 0.0},
        {"id": WAITING, "project": "website", "branch": "main", "status": "waiting",
         "summary": "Updates the pricing page", "cost_eur": 0.0},
    ]})
}

/// What the working session did: prompt, todos, plan, edits, tool calls.
pub fn node_events(now: chrono::DateTime<Utc>) -> serde_json::Value {
    let at = |minutes: i64| (now - Duration::minutes(minutes)).to_rfc3339();
    json!({"events": [
        {"session_id": WORKING, "kind": "prompt", "summary": "Add a CSV export for invoices with a test", "at": at(42)},
        {"session_id": WORKING, "kind": "plan", "tool": "ExitPlanMode", "at": at(40),
         "detail": {"plan": "1. Add an export module\n2. Stream rows in batches\n3. Add a download button\n4. Test with 10,000 invoices"}},
        {"session_id": WORKING, "kind": "todos", "tool": "TodoWrite", "at": at(38),
         "detail": {"todos": [
             {"content": "Add the export module", "status": "completed"},
             {"content": "Stream rows in batches", "status": "completed"},
             {"content": "Add the download button", "status": "in_progress"},
             {"content": "Test with 10,000 invoices", "status": "pending"}]}},
        {"session_id": WORKING, "kind": "file", "tool": "Write", "file_path": "src/export.rs", "at": at(30),
         "detail": {"diff": "+pub fn export_csv(rows: &[Invoice]) -> String {\n+    rows.iter().map(Invoice::to_csv).collect()\n+}"}},
        {"session_id": WORKING, "kind": "tool", "tool": "Bash", "summary": "cargo test export", "at": at(20)},
        {"session_id": WORKING, "kind": "file", "tool": "Edit", "file_path": "src/invoices.rs", "at": at(12),
         "detail": {"diff": "-let rows = load();\n+let rows = load_in_batches(500);"}},
        {"session_id": WORKING, "kind": "tool", "tool": "Bash", "summary": "cargo test", "at": at(5)},
        {"session_id": WAITING, "kind": "prompt", "summary": "Update the prices on the pricing page", "at": at(9)},
        {"session_id": WAITING, "kind": "file", "tool": "Edit", "file_path": "pages/pricing.html", "at": at(6),
         "detail": {"diff": "-<td>19 EUR</td>\n+<td>24 EUR</td>"}},
    ]})
}

/// Cost and tokens of the working session, as Claude Code exports them.
pub fn node_metrics() -> serde_json::Value {
    let point = |kind: Option<&str>, value: f64| {
        let mut attributes = vec![json!({"key": "session.id", "value": {"stringValue": WORKING}})];
        if let Some(kind) = kind {
            attributes.push(json!({"key": "type", "value": {"stringValue": kind}}));
        }
        json!({"attributes": attributes, "asDouble": value})
    };
    json!({"resourceMetrics": [{"resource": {"attributes": []}, "scopeMetrics": [{"metrics": [
        {"name": "claude_code.cost.usage", "sum": {"aggregationTemporality": 2, "dataPoints": [point(None, 1.84)]}},
        {"name": "claude_code.token.usage", "sum": {"aggregationTemporality": 2, "dataPoints": [
            point(Some("input"), 182_000.0), point(Some("output"), 24_500.0),
            point(Some("cacheRead"), 640_000.0), point(Some("cacheCreation"), 51_000.0)]}},
    ]}]}]})
}

/// The permission the waiting session asks for.
pub fn node_permission() -> serde_json::Value {
    json!({"session_id": WAITING, "project": "website", "tool": "Bash",
           "input": {"command": "git push origin main", "description": "Publish the new prices"}})
}

/// Adds the demo data, unless it is there already.
pub async fn seed(store: &Store, brain_dir: &Path) -> anyhow::Result<Seeded> {
    let tenant = TenantId::SINGLE;
    let sync = SyncStore::new(store.clone(), tenant);
    if !sync.organizations().await?.is_empty() {
        return Ok(Seeded::AlreadyThere);
    }
    Auth::new(store.clone(), tenant)
        .set_password(&SecretString::from(DEMO_PASSWORD.to_owned()))
        .await?;
    for (id, name, color, sources) in [
        (
            "acme",
            "Acme Ltd",
            "#2563eb",
            vec!["crm-acme", "bank-acme", "books-acme", "rooms", "contracts"],
        ),
        (
            "globex",
            "Globex Inc",
            "#16a34a",
            vec!["crm-globex", "bank-globex", "books-globex"],
        ),
    ] {
        let input = OrganizationInput {
            name: name.to_owned(),
            color: Some(color.to_owned()),
            sources: sources.into_iter().map(str::to_owned).collect(),
        };
        sync.save_organization(id, input).await?;
    }

    let desk = Desk::new(store.clone(), tenant);
    let today = Utc::now().date_naive();
    for (title, due, priority) in [
        ("Send the updated offer to Globex", Some(today), 3),
        (
            "Prepare the weekly planning",
            Some(today + Duration::days(1)),
            2,
        ),
        (
            "Renew the domain of the demo site",
            Some(today + Duration::days(9)),
            1,
        ),
        ("Read the onboarding notes of Jane Doe", None, 0),
    ] {
        desk.create_task(
            NewTask {
                title: title.to_owned(),
                due_on: due,
                priority,
                ..NewTask::default()
            },
            None,
        )
        .await?;
    }
    desk.create_task(
        NewTask {
            title: "Answer John Roe about the pilot".to_owned(),
            source: Some(SourceLink {
                kind: "mail".to_owned(),
                reference: "t1".to_owned(),
                label: Some("Updated offer for Globex?".to_owned()),
                url: None,
            }),
            ..NewTask::default()
        },
        Some(Proposer::agent("inbox", Uuid::now_v7())),
    )
    .await?;

    for (text, agent) in [
        ("Always ask before anything goes to a customer.", None),
        ("Write in plain English, short sentences.", None),
        (
            "Keep the brief to what needs me today.",
            Some("morning-brief"),
        ),
    ] {
        desk.create_rule(NewRule {
            text: text.to_owned(),
            agent_id: agent.map(str::to_owned),
            category: Some("work".to_owned()),
            enabled: true,
        })
        .await?;
    }
    for text in [
        "Prefers meetings before noon.",
        "Reviews pull requests in the afternoon.",
    ] {
        desk.create_fact(NewFact {
            text: text.to_owned(),
            category: Some("work".to_owned()),
        })
        .await?;
    }
    desk.propose_fact(
        ProposedFact {
            text: "Answers customers within a day.".to_owned(),
            category: Some("work".to_owned()),
            confidence: 0.7,
            evidence: Some("Reply times in the mail of the last month".to_owned()),
        },
        &Proposer::agent("inbox", Uuid::now_v7()),
    )
    .await?;

    // Alerts with a history: repeats count up, one escalates, one is acknowledged.
    let alert =
        |kind: &str, key: &str, severity, title: &str, detail: &str, value, unit: &str| NewAlert {
            value: Some(value),
            unit: Some(unit.to_owned()),
            kind: kind.to_owned(),
            key: key.to_owned(),
            severity,
            title: title.to_owned(),
            detail: Some(detail.to_owned()),
            source: None,
        };
    let failures = lighthouse_desk::FAILURES;
    let mut calendar = None;
    for (severity, detail) in [
        (Severity::Warning, "Timeout after 30 s"),
        (Severity::Warning, "Timeout after 30 s"),
        (
            Severity::Critical,
            "Timeout after 30 s on three refreshes in a row",
        ),
    ] {
        let raised = desk
            .raise_alert(alert(
                "connector",
                "connector:calendar",
                severity,
                "The calendar connector cannot be reached",
                detail,
                1.0,
                failures,
            ))
            .await?;
        calendar = Some(raised.alert.id);
    }
    if let Some(id) = calendar {
        desk.acknowledge_alert(id).await?;
    }
    for detail in [
        "The model answered too late",
        "The model answered too late again",
    ] {
        desk.raise_alert(alert(
            "run_failed",
            "run:weekly-report",
            Severity::Warning,
            "The agent weekly-report failed",
            detail,
            1.0,
            failures,
        ))
        .await?;
    }
    desk.raise_alert(alert(
        "cost_limit",
        "cost:assistant",
        Severity::Warning,
        "The agent assistant hit its cost limit",
        "cost limit of 0.50 EUR reached",
        0.52,
        "EUR",
    ))
    .await?;

    desk.save_brand_profile(&BrandProfile {
        headline: Some("Helps small teams send invoices on time".to_owned()),
        bio: Some("Runs Acme Ltd and advises Globex Inc. Writes about plain processes.".to_owned()),
        audience: Some("Founders of small service companies".to_owned()),
        voice: Some("Plain, warm, concrete. One idea per post.".to_owned()),
        topics: vec!["Invoicing".to_owned(), "Small teams".to_owned()],
        avoid: vec!["Buzzwords".to_owned()],
        updated_at: None,
    })
    .await?;
    for (id, name, topics) in [
        (
            "finance",
            "Finance for small teams",
            vec!["Invoices", "Cash"],
        ),
        ("work", "How we work", vec!["Meetings", "Tools"]),
    ] {
        desk.save_pillar(
            id,
            PillarInput {
                name: name.to_owned(),
                topics: topics.into_iter().map(str::to_owned).collect(),
                ..PillarInput::default()
            },
        )
        .await?;
    }
    let slots: Vec<PostSlot> = [(2, "08:30", "finance"), (4, "12:00", "work")]
        .into_iter()
        .map(|(weekday, time, pillar)| PostSlot {
            id: Uuid::now_v7(),
            weekday,
            time: time.to_owned(),
            pillar: Some(pillar.to_owned()),
            org: Some("acme".to_owned()),
        })
        .collect();
    desk.set_post_slots(&slots).await?;
    desk.set_blocked_days(vec![lighthouse_desk::BlockedDay {
        date: today + Duration::days(12),
        reason: Some("Holiday".to_owned()),
    }])
    .await?;
    let body = "Most invoices are late for one boring reason.\n\nNobody owns the reminder. We gave every invoice an owner and a date, and the average delay went from 19 to 6 days.\n\nWhat is the boring fix in your team?\n\n#invoicing #smallteams";
    desk.create_post(NewPost {
        body: body.to_owned(),
        title: Some("Late invoices".to_owned()),
        pillar: Some("finance".to_owned()),
        org: Some("acme".to_owned()),
        scheduled_at: Some(Utc::now() + Duration::days(2)),
        first_comment: Some("How we set the reminders: https://example.com/reminders".to_owned()),
        ..NewPost::default()
    })
    .await?;
    desk.create_post(NewPost {
        body: "Our weekly planning takes 20 minutes. Here is the agenda.".to_owned(),
        pillar: Some("work".to_owned()),
        ..NewPost::default()
    })
    .await?;
    let archive: Vec<ImportedPost> = [(30, 4200, 61, 12), (23, 2900, 33, 4), (9, 6100, 98, 21)]
        .into_iter()
        .enumerate()
        .map(
            |(index, (days, impressions, reactions, comments))| ImportedPost {
                external_id: Some(format!("demo-post-{index}")),
                body: format!("An earlier post of the demo, number {}.", index + 1),
                published_at: Some(Utc::now() - Duration::days(days)),
                pillar: Some("finance".to_owned()),
                metrics: [
                    ("impressions".to_owned(), json!(impressions)),
                    ("reactions".to_owned(), json!(reactions)),
                    ("comments".to_owned(), json!(comments)),
                ]
                .into_iter()
                .collect(),
                ..ImportedPost::default()
            },
        )
        .collect();
    desk.import_posts(&archive).await?;

    let thread = desk.create_thread("What is open with Globex?").await?;
    desk.add_message(
        thread.id,
        "owner",
        "What is open with Globex?",
        "done",
        None,
        json!([]),
    )
    .await?;
    desk.add_message(
        thread.id,
        "assistant",
        "Globex waits for the updated offer, and the renewal has had no activity for 35 days.",
        "done",
        None,
        json!([]),
    )
    .await?;

    let brain = Brain::new(store.clone(), tenant, brain_dir.to_path_buf())?;
    brain
        .remember(entry(
            "deck",
            "Proposal for Globex",
            "# Proposal for Globex\n\n---\n\n## The problem\n\n- Invoices go out late\n\n---\n\n## Next steps\n\n- Pilot with one team",
            vec!["customer:globex".to_owned()],
        ))
        .await?;
    brain
        .remember(entry(
            "template",
            "Service agreement",
            "# Service agreement\n\nBetween Acme Ltd and {{customer}}, starting {{start}}.\n\nFee: {{fee}} EUR per month.",
            Vec::new(),
        ))
        .await?;
    brain
        .remember(entry(
            "customer",
            "Globex Inc",
            "Contact: John Roe. Pilot planned for one team. Renewal under negotiation.",
            Vec::new(),
        ))
        .await?;
    for (title, body, link) in [
        (
            "Why I hold Example Corp",
            "Steady revenue and low debt. Check after each quarterly report.",
            "holding:depot:EXA",
        ),
        (
            "Revenue growth above 10%",
            "Below that for two quarters, the thesis no longer holds.",
            "holding:depot:EXA#metric",
        ),
        (
            "Does the new plant pay off by 2027?",
            "Look for the utilization in the next report.",
            "holding:depot:EXA#question",
        ),
    ] {
        brain
            .remember(entry("note", title, body, vec![link.to_owned()]))
            .await?;
    }
    Ok(Seeded::Added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_desk::{AlertView, TaskView};
    use lighthouse_store::testing::fresh_store;

    #[test]
    fn the_demo_machine_reports_two_sessions_once() {
        assert!(needs_node(&json!([])));
        assert!(!needs_node(&json!([{"id": WORKING}])));
        assert_eq!(node_report()["sessions"].as_array().unwrap().len(), 2);
        let events = node_events(Utc::now());
        let kinds: Vec<&str> = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["kind"].as_str())
            .collect();
        for kind in ["prompt", "plan", "todos", "file", "tool"] {
            assert!(kinds.contains(&kind), "{kind}");
        }
        let usage = lighthouse_sessions::parse_otlp(&node_metrics());
        assert_eq!(usage[0].session_id, WORKING);
        assert_eq!(node_permission()["session_id"], WAITING);
        assert_eq!(node_token_request()["kind"], "node");
    }

    #[test]
    fn the_warmup_runs_only_what_is_missing() {
        let empty = json!({"counts": {"approval": 0}});
        assert_eq!(
            warmup_agents(&empty, false, &json!([])),
            ["ready", "morning-brief", "weekly-report"]
        );
        assert_eq!(
            warmup_agents(&json!({}), true, &json!(null)),
            ["ready", "weekly-report"]
        );
        let full = json!({"counts": {"approval": 3}});
        assert!(warmup_agents(&full, true, &json!([{"kind": "weekly_recap"}])).is_empty());
    }

    #[tokio::test]
    async fn the_demo_is_seeded_once() {
        let store = fresh_store().await;
        let brain = tempfile::tempdir().unwrap();
        assert_eq!(seed(&store, brain.path()).await.unwrap(), Seeded::Added);
        assert_eq!(
            seed(&store, brain.path()).await.unwrap(),
            Seeded::AlreadyThere
        );
        let desk = Desk::new(store.clone(), TenantId::SINGLE);
        let today = Utc::now().date_naive();
        assert_eq!(desk.tasks(TaskView::All, today, 50).await.unwrap().len(), 5);
        let alerts = desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(alerts.len(), 3);
        let calendar = alerts
            .iter()
            .find(|alert| alert.key == "connector:calendar")
            .unwrap();
        assert_eq!(calendar.value, Some(3.0));
        let kinds: Vec<&str> = calendar
            .events
            .iter()
            .map(|event| event.kind.as_str())
            .collect();
        assert_eq!(kinds, ["raised", "seen", "escalated", "acknowledged"]);
        let sync = SyncStore::new(store.clone(), TenantId::SINGLE);
        assert_eq!(sync.organizations().await.unwrap().len(), 2);
        let brain = Brain::new(store, TenantId::SINGLE, brain.path().to_path_buf()).unwrap();
        assert_eq!(brain.list(Some("deck"), 5).await.unwrap().len(), 1);
    }
}
