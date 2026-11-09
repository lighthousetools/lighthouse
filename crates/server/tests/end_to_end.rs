//! Starts the fully wired server against a real database and a real MCP server.

use axum::Router;
use base64::Engine;
use lighthouse_audit::{AuditLog, NewApproval};
use lighthouse_config::Config;
use lighthouse_core::{RunId, TenantId};
use lighthouse_mcp::demo::DemoServer;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TOKEN: &str = "test-admin-token";

const ON_FAILURE: &str = "---
name: On failure
trigger:
  event: run.failed
  match:
    agent_id: deleter
steps:
  - agent: reader
  - tool:
      name: demo.list_issues
  - if:
      contains: DEMO-1
  - tool:
      name: demo.echo
      input:
        note: \"{{event.agent_id}} failed\"
---
Looks into failed runs of the deleter.
";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

fn write_agent(dir: &Path, file: &str, tools: &str) {
    let text = format!(
        "---\nname: {file}\nscope: org\ntrigger:\n  manual: true\ntools:\n{tools}\n---\nGo.\n"
    );
    std::fs::write(dir.join(format!("{file}.md")), text).unwrap();
}

async fn start() -> (String, tempfile::TempDir, lighthouse_store::Store) {
    start_with("").await
}

async fn start_with(extra: &str) -> (String, tempfile::TempDir, lighthouse_store::Store) {
    start_with_hooks(extra, lighthouse_server::Hooks::default()).await
}

async fn start_with_hooks(
    extra: &str,
    hooks: lighthouse_server::Hooks,
) -> (String, tempfile::TempDir, lighthouse_store::Store) {
    let database = lighthouse_store::testing::fresh_store().await;
    let url: (String,) = lighthouse_store::sqlx::query_as("SELECT current_database()::text")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let base = std::env::var("DATABASE_URL").unwrap();
    let (server, _) = base.rsplit_once('/').unwrap();
    let database_url = format!("{server}/{}", url.0);

    let service = StreamableHttpService::new(
        || Ok(DemoServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let mcp = serve(Router::new().nest_service("/mcp", service)).await;

    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    std::fs::create_dir(&agents).unwrap();
    write_agent(&agents, "reader", "  - demo.list_issues");
    write_agent(&agents, "deleter", "  - demo.delete_issue");
    std::fs::write(agents.join("broken.md"), "not an agent").unwrap();
    let automations = dir.path().join("automations");
    std::fs::create_dir(&automations).unwrap();
    std::fs::write(automations.join("on-failure.md"), ON_FAILURE).unwrap();
    let key = base64::engine::general_purpose::STANDARD.encode([5u8; 32]);
    let toml = format!(
        r#"
[server]
bind = "127.0.0.1:0"
public_url = "https://lighthouse.test"
admin_token = "{TOKEN}"
[database]
url = "{database_url}"
[vault]
master_key = "{key}"
[owner]
name = "Alex Example"
email = "alex@example.com"
[paths]
agents = "{agents}"
created_agents = "{created}"
automations = "{automations}"
created_automations = "{created_automations}"
skills = "{skills}"
[providers.mock]
kind = "mock"
[providers.mock.tasks.text]
model = "mock"
input_eur_per_mtok = 1.0
output_eur_per_mtok = 1.0
[routing.text]
provider = "mock"
[connectors.demo]
transport = "http"
url = "{mcp}/mcp"
read_tools = ["list_issues"]
[sources.demo]
connector = "demo"
domain = "issues"
[sources.demo.collections.issues]
tool = "list_issues"
kind = "issue"
flags = ["assigned"]
[sources.demo.collections.issues.fields]
external_id = "id"
title = "title"
priority = "priority"
[sources.demo.actions.comment]
tool = "echo"
kind = "issue"
input = {{ issue = "{{{{item.external_id}}}}", body = "{{{{input.body}}}}" }}
[sources.demo.actions.lookup]
tool = "list_issues"
[talk]
agent = "reader"
[publishing]
connector = "demo"
tool = "echo"
input = {{ text = "{{{{post.body}}}}" }}
[brain]
path = "{brain}"
"#,
        agents = agents.display(),
        created = dir.path().join("created").display(),
        automations = automations.display(),
        created_automations = dir.path().join("created-automations").display(),
        brain = dir.path().join("brain").display(),
        skills = dir.path().join("skills").display(),
    );
    let toml = format!(
        "{toml}\n{}",
        extra.replace("{dir}", &dir.path().display().to_string())
    );
    let path = dir.path().join("lighthouse.toml");
    std::fs::write(&path, toml).unwrap();
    let app = lighthouse_server::build_with(Config::load(&path).unwrap(), hooks)
        .await
        .unwrap();
    (serve(app.router).await, dir, database)
}

async fn run_to_end(base: &str, agent: &str) -> Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/agents/{agent}/run"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let started: Value = response.json().await.unwrap();
    let id = started["id"].as_str().unwrap();
    for _ in 0..100 {
        let detail: Value = client
            .get(format!("{base}/runs/{id}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if detail["run"]["status"] != "running" {
            return detail;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run did not finish");
}

#[tokio::test]
async fn read_only_agent_runs_and_write_agent_is_rejected() {
    let (base, _dir, _database) = start().await;
    let health: Value = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");

    let detail = run_to_end(&base, "reader").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    assert_eq!(detail["run"]["report"], "mock answer");

    let detail = run_to_end(&base, "deleter").await;
    assert_eq!(detail["run"]["status"], "rejected");
    assert!(
        detail["run"]["reason"]
            .as_str()
            .unwrap()
            .contains("would have needed an approval")
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/agents/broken/run"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn a_decision_through_the_api_finishes_the_paused_run() {
    let (base, _dir, database) = start().await;
    let audit = AuditLog::new(database, TenantId::DEFAULT);
    let run = RunId::new();
    let approval = Uuid::now_v7();
    audit.start_run(run, "reader", "manual").await.unwrap();
    let state = json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Go"}]}],
        "spent": 0.0,
        "steps": 0,
        "saw_external": false,
        "results": [],
        "pending": [],
    });
    let proposal = NewApproval {
        id: approval,
        tool: "demo.delete_issue".to_owned(),
        input: json!({}),
        quotes_external: false,
        reason: None,
        consulted: Vec::new(),
    };
    audit
        .pause_run(run, "reader", &state, &[proposal])
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/approvals/{approval}/decision"))
        .bearer_auth(TOKEN)
        .json(&json!({"approve": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    for _ in 0..100 {
        let detail = audit.run_detail(run.as_uuid()).await.unwrap().unwrap();
        if detail.run.status == "succeeded" {
            assert_eq!(detail.run.report.as_deref(), Some("mock answer"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the run did not resume");
}

#[tokio::test]
async fn a_question_is_answered_from_the_brain() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/brain/entries"))
        .bearer_auth(TOKEN)
        .json(&json!({"type": "note", "title": "Rule", "body": "Due in January"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let answer: Value = client
        .post(format!("{base}/brain/ask"))
        .bearer_auth(TOKEN)
        .json(&json!({"question": "january"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer["answer"], "mock answer", "{answer}");
    assert_eq!(answer["sources"][0]["title"], "Rule");
}

#[tokio::test]
async fn an_agent_created_through_the_api_runs_without_a_restart() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let source = "---\nname: Created\nscope: org\ntrigger:\n  cron: \"0 8 * * *\"\n---\nGo.\n";
    let response = client
        .post(format!("{base}/agents"))
        .bearer_auth(TOKEN)
        .json(&json!({"source": source}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let saved: Value = response.json().await.unwrap();
    assert_eq!(saved["id"], "created");
    assert!(saved["next_run"].is_string(), "{saved}");
    assert_eq!(saved["editable"], true);

    let detail = run_to_end(&base, "created").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");

    let agents: Value = client
        .get(format!("{base}/agents"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let deleter = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == "deleter")
        .unwrap();
    assert_eq!(deleter["blocked"].as_array().unwrap().len(), 1);
    assert_eq!(deleter["editable"], false);

    let response = client
        .delete(format!("{base}/agents/deleter"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422, "managed files stay untouched");
    let connectors: Value = client
        .get(format!("{base}/connectors"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(connectors[0]["name"], "demo");
    assert_eq!(connectors[0]["connected"], true);
}

async fn get(base: &str, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Waits up to 30 seconds, since runs are slow under coverage.
async fn wait_for(base: &str, path: &str, check: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..600 {
        let value = get(base, path).await;
        if check(&value) {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{path} never reached the expected state: {}",
        get(base, path).await
    );
}

#[tokio::test]
async fn a_failed_run_triggers_an_automation_that_asks_before_it_acts() {
    let (base, _dir, _database) = start().await;
    let automations = get(&base, "/automations").await;
    assert_eq!(automations[0]["id"], "on-failure", "{automations}");
    assert_eq!(automations[0]["steps"][3]["needs_approval"], true);

    let detail = run_to_end(&base, "deleter").await;
    assert_eq!(detail["run"]["status"], "rejected");
    let runs = wait_for(&base, "/automations/on-failure/runs", |runs| {
        runs[0]["status"] == "waiting"
    })
    .await;
    let run = runs[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(runs[0]["trigger"], "event:run.failed");

    let alerts = get(&base, "/alerts").await;
    assert_eq!(alerts[0]["key"], "run:deleter", "{alerts}");
    let ready = get(&base, "/ready").await;
    assert_eq!(ready["counts"]["approval"], 1, "{ready}");
    let approval = &ready["items"][0]["detail"];
    assert_eq!(approval["agent_id"], "automation:on-failure");
    assert_eq!(approval["input"]["note"], "deleter failed");

    let response = reqwest::Client::new()
        .post(format!(
            "{base}/approvals/{}/decision",
            approval["id"].as_str().unwrap()
        ))
        .bearer_auth(TOKEN)
        .json(&json!({"approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let detail = wait_for(&base, &format!("/runs/{run}"), |detail| {
        detail["run"]["status"] == "succeeded"
    })
    .await;
    let steps: Vec<(String, String)> = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["name"].as_str().unwrap().to_owned(),
                step["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("reader".to_owned(), "ok".to_owned()),
            ("demo.list_issues".to_owned(), "ok".to_owned()),
            ("contains DEMO-1".to_owned(), "ok".to_owned()),
            ("demo.echo".to_owned(), "proposed".to_owned()),
            ("demo.echo".to_owned(), "ok".to_owned()),
        ]
    );
    assert!(
        detail["run"]["report"]
            .as_str()
            .unwrap()
            .contains("deleter failed")
    );
}

#[tokio::test]
async fn automations_are_created_switched_and_started_through_the_api() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let source = "---\nname: Hook\ntrigger:\n  webhook: true\n  match:\n    kind: deploy\nsteps:\n  - tool:\n      name: demo.list_issues\n---\n";
    let response = client
        .post(format!("{base}/automations"))
        .bearer_auth(TOKEN)
        .json(&json!({"source": source}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let hook = |payload: Value| {
        client
            .post(format!("{base}/automations/hook/webhook"))
            .bearer_auth(TOKEN)
            .json(&payload)
            .send()
    };
    let skipped: Value = hook(json!({"kind": "other"}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(skipped["runs"], json!([]));
    let started: Value = hook(json!({"kind": "deploy"}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run = started["runs"][0].as_str().unwrap().to_owned();
    wait_for(&base, &format!("/runs/{run}"), |detail| {
        detail["run"]["status"] == "succeeded"
    })
    .await;

    let response = client
        .patch(format!("{base}/automations/hook"))
        .bearer_auth(TOKEN)
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    let switched: Value = response.json().await.unwrap();
    assert_eq!(switched["enabled"], false);
    assert_eq!(hook(json!({"kind": "deploy"})).await.unwrap().status(), 409);
    let manual = client
        .post(format!("{base}/automations/hook/run"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(manual.status(), 202, "a manual run ignores the switch");
    let wrong = client
        .post(format!("{base}/automations/on-failure/webhook"))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 409);
    let event = client
        .post(format!("{base}/automations/events"))
        .bearer_auth(TOKEN)
        .json(&json!({"name": "mail.received", "payload": {}}))
        .send()
        .await
        .unwrap();
    let event: Value = event.json().await.unwrap();
    assert_eq!(event["runs"], json!([]));
    let deleted = client
        .delete(format!("{base}/automations/hook"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
}

#[tokio::test]
async fn connected_issues_refresh_and_comments_wait_for_approval() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let sources = get(&base, "/connected/sources").await;
    assert_eq!(sources[0]["name"], "demo");
    let refreshed: Value = client
        .post(format!("{base}/connected/sources/demo/refresh"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refreshed["items"], 2, "{refreshed}");
    let issues = get(&base, "/issues?q=login").await;
    assert_eq!(issues["total"], 1, "{issues}");
    let inbox = get(&base, "/inbox").await;
    assert_eq!(inbox["total"], 2);
    let id = issues["items"][0]["id"].as_str().unwrap().to_owned();

    let response = client
        .post(format!("{base}/connected/items/{id}/actions/comment"))
        .bearer_auth(TOKEN)
        .json(&json!({"input": {"body": "On it"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let proposed: Value = response.json().await.unwrap();
    let run = proposed["run_id"].as_str().unwrap().to_owned();
    let ready = get(&base, "/ready").await;
    let approval = &ready["items"][0]["detail"];
    assert_eq!(approval["agent_id"], "action:demo", "{ready}");
    assert_eq!(
        approval["input"],
        json!({"issue": "DEMO-1", "body": "On it"})
    );
    let response = client
        .post(format!(
            "{base}/approvals/{}/decision",
            approval["id"].as_str().unwrap()
        ))
        .bearer_auth(TOKEN)
        .json(&json!({"approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let detail = wait_for(&base, &format!("/runs/{run}"), |detail| {
        detail["run"]["status"] == "succeeded"
    })
    .await;
    assert!(detail["run"]["report"].as_str().unwrap().contains("On it"));

    let lookup = client
        .post(format!("{base}/connected/sources/demo/actions/lookup"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(lookup.status(), 200);
    let lookup: Value = lookup.json().await.unwrap();
    assert_eq!(lookup["status"], "done");
    let status = get(&base, "/connected/sources").await;
    assert!(
        status[0]["status"]["last_success_at"].is_string(),
        "{status}"
    );
}

#[tokio::test]
async fn a_source_that_would_write_does_not_start() {
    let (_base, dir, _database) = start().await;
    let path = dir.path().join("lighthouse.toml");
    let toml = std::fs::read_to_string(&path).unwrap().replace(
        "[sources.demo.collections.issues]\ntool = \"list_issues\"",
        "[sources.demo.collections.issues]\ntool = \"delete_issue\"",
    );
    std::fs::write(&path, toml).unwrap();
    let error = lighthouse_server::build(Config::load(&path).unwrap())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("read_tools"), "{error}");
}

#[tokio::test]
async fn a_post_is_published_only_after_approval() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let body = format!(
        "Most invoices are late for one reason.\n\n{} #finance",
        "Here is why. ".repeat(30)
    );
    let post: Value = client
        .post(format!("{base}/posts"))
        .bearer_auth(TOKEN)
        .json(&json!({"body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = post["id"].as_str().unwrap().to_owned();
    let response = client
        .post(format!("{base}/posts/{id}/publish"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let waiting = get(&base, &format!("/posts/{id}")).await;
    assert_eq!(waiting["status"], "publishing");
    let ready = get(&base, "/ready").await;
    let approval = &ready["items"][0]["detail"];
    assert_eq!(approval["agent_id"], format!("action:post:{id}"), "{ready}");
    assert_eq!(approval["tool"], "demo.echo");
    let response = client
        .post(format!(
            "{base}/approvals/{}/decision",
            approval["id"].as_str().unwrap()
        ))
        .bearer_auth(TOKEN)
        .json(&json!({"approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let published = wait_for(&base, &format!("/posts/{id}"), |post| {
        post["status"] == "published"
    })
    .await;
    assert!(published["published_at"].is_string());

    let second: Value = client
        .post(format!("{base}/posts"))
        .bearer_auth(TOKEN)
        .json(&json!({"body": body}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second = second["id"].as_str().unwrap().to_owned();
    client
        .post(format!("{base}/posts/{second}/publish"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let ready = get(&base, "/ready").await;
    let approval = ready["items"][0]["detail"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .post(format!("{base}/approvals/{approval}/decision"))
        .bearer_auth(TOKEN)
        .json(&json!({"approve": false}))
        .send()
        .await
        .unwrap();
    wait_for(&base, &format!("/posts/{second}"), |post| {
        post["status"] == "draft"
    })
    .await;
}

#[tokio::test]
async fn a_dry_run_shows_the_steps_and_a_draft_needs_a_valid_file() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let plan: Value = client
        .post(format!("{base}/automations/dry-run"))
        .bearer_auth(TOKEN)
        .json(&json!({"id": "on-failure", "payload": {"agent_id": "deleter"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plan["trigger_matches"], true, "{plan}");
    assert_eq!(plan["steps"][3]["needs_approval"], true);
    assert_eq!(plan["steps"][3]["input"]["note"], "deleter failed");
    let source = "---\nname: B\ntrigger:\n  manual: true\nsteps:\n  - if:\n      contains: x\n      then:\n        - agent: reader\n      else:\n        - notify:\n            message: \"{{output}}\"\n---\n";
    let plan: Value = client
        .post(format!("{base}/automations/dry-run"))
        .bearer_auth(TOKEN)
        .json(&json!({"source": source}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let paths: Vec<&str> = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["1", "1.then.1", "1.else.1"]);
    assert_eq!(plan["trigger_matches"], Value::Null);
    let draft = client
        .post(format!("{base}/automations/draft"))
        .bearer_auth(TOKEN)
        .json(&json!({"description": "Tell me when a run fails"}))
        .send()
        .await
        .unwrap();
    assert_eq!(draft.status(), 502, "the mock model writes no file");
}

#[tokio::test]
async fn a_delegated_task_gets_the_report_of_its_run() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let task: Value = client
        .post(format!("{base}/tasks"))
        .bearer_auth(TOKEN)
        .json(&json!({"title": "Collect the open issues"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = task["id"].as_str().unwrap().to_owned();
    let response = client
        .post(format!("{base}/tasks/{id}/delegate"))
        .bearer_auth(TOKEN)
        .json(&json!({"agent_id": "reader"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let task = wait_for(&base, &format!("/tasks/{id}"), |task| {
        task["result"].is_string()
    })
    .await;
    assert_eq!(task["result"], "mock answer");
    assert_eq!(task["result_status"], "succeeded");
}

#[tokio::test]
async fn a_talk_agent_answers_a_conversation_through_a_run() {
    let (base, _dir, _database) = start().await;
    let client = reqwest::Client::new();
    let thread: Value = client
        .post(format!("{base}/talk/threads"))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = thread["id"].as_str().unwrap().to_owned();
    let response = client
        .post(format!("{base}/talk/threads/{id}/messages"))
        .bearer_auth(TOKEN)
        .json(&json!({"text": "What is open?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let detail = wait_for(&base, &format!("/talk/threads/{id}"), |detail| {
        detail["messages"][1]["status"] == "done"
    })
    .await;
    assert_eq!(detail["messages"][1]["text"], "mock answer");
    assert_eq!(detail["thread"]["title"], "What is open?");
}

#[tokio::test]
async fn a_signed_up_tenant_works_on_its_own_and_can_be_deleted() {
    let (base, dir, _database) =
        start_with("[tenancy]\nsignup = \"open\"\ndata_dir = \"{dir}/tenants\"\n").await;
    let client = reqwest::Client::new();
    let issued: Value = client
        .post(format!("{base}/auth/signup"))
        .json(
            &json!({"email": "sam@example.com", "name": "Sam", "password": "a long enough password",
                      "tenant_name": "Studio", "device_name": "web", "device_kind": "web"}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = issued["token"].as_str().unwrap().to_owned();
    let tenant = issued["tenant_id"].as_str().unwrap().to_owned();
    let tenant_dir = dir.path().join("tenants").join(&tenant);

    let started: Value = client
        .post(format!("{base}/agents/reader/run"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run = started["id"].as_str().unwrap().to_owned();
    let mut status = Value::Null;
    for _ in 0..600 {
        let detail: Value = client
            .get(format!("{base}/runs/{run}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        status = detail["run"]["status"].clone();
        if status != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "succeeded");
    assert!(
        tenant_dir.join("brain").is_dir(),
        "the tenant got its own Brain"
    );
    let in_default = client
        .get(format!("{base}/runs/{run}"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        in_default.status(),
        404,
        "the default tenant does not see it"
    );

    let deleted = client
        .delete(format!("{base}/tenant"))
        .bearer_auth(&token)
        .json(&json!({"confirm": "Studio"}))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    assert!(!tenant_dir.exists(), "its files are gone");
    let after = client
        .get(format!("{base}/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);
}

struct Gate;

#[async_trait::async_trait]
impl lighthouse_api::hooks::Lifecycle for Gate {
    async fn on_start(&self, instance: &lighthouse_api::Instance) {
        let name = instance.extension_config["gate"]["prepared"]
            .as_str()
            .unwrap();
        instance.create_tenant(name, None).await.unwrap();
    }

    async fn on_signup(&self, email: &str, _client: &str) -> lighthouse_api::hooks::SignupDecision {
        if email.starts_with("wait") {
            lighthouse_api::hooks::SignupDecision::Pending("we will be in touch".to_owned())
        } else if email.starts_with("no") {
            lighthouse_api::hooks::SignupDecision::Reject("not today".to_owned())
        } else {
            lighthouse_api::hooks::SignupDecision::Allow
        }
    }
}

#[async_trait::async_trait]
impl lighthouse_api::hooks::Entitlements for Gate {
    async fn check(
        &self,
        tenant: TenantId,
        action: &lighthouse_api::hooks::Action,
    ) -> Result<(), String> {
        match action {
            lighthouse_api::hooks::Action::RunAgent { .. } if tenant != TenantId::DEFAULT => {
                Err("runs are not included".to_owned())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
struct Outbox(std::sync::Mutex<Vec<lighthouse_notify::Mail>>);

#[async_trait::async_trait]
impl lighthouse_notify::Mailer for Outbox {
    async fn send(
        &self,
        mail: &lighthouse_notify::Mail,
    ) -> Result<(), lighthouse_notify::NotifyError> {
        self.0.lock().unwrap().push(mail.clone());
        Ok(())
    }
}

async fn whoami(caller: lighthouse_api::Caller) -> String {
    caller.tenant.as_uuid().to_string()
}

#[tokio::test]
async fn another_program_adds_hooks_limits_and_routes() {
    use axum::routing::get as route_get;
    let routes = lighthouse_api::Extensions {
        public: axum::Router::new().route("/ext/ping", route_get(|| async { "pong" })),
        signed_in: axum::Router::new().route("/ext/whoami", route_get(whoami)),
    };
    let outbox = Arc::new(Outbox::default());
    let hooks = lighthouse_server::Hooks {
        lifecycle: Arc::new(Gate),
        entitlements: Arc::new(Gate),
        routes,
        mailer: Some(outbox.clone()),
        ..lighthouse_server::Hooks::default()
    };
    let (base, _dir, _database) = start_with_hooks(
        "[tenancy]\nsignup = \"open\"\ndata_dir = \"{dir}/tenants\"\n\
         [extensions.gate]\nprepared = \"Prepared\"\n",
        hooks,
    )
    .await;
    let client = reqwest::Client::new();
    let signup = |email: &str| {
        client
            .post(format!("{base}/auth/signup"))
            .json(
                &json!({"email": email, "name": "Sam", "password": "a long enough password",
                          "device_name": "web", "device_kind": "web"}),
            )
            .send()
    };
    assert_eq!(signup("wait@example.com").await.unwrap().status(), 202);
    assert_eq!(signup("no@example.com").await.unwrap().status(), 403);
    let issued: Value = signup("sam@example.com")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = issued["token"].as_str().unwrap();
    let tenants: Value = client
        .get(format!("{base}/tenants"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        tenants
            .as_array()
            .unwrap()
            .iter()
            .any(|tenant| tenant["tenant_name"] == "Prepared"),
        "{tenants}"
    );
    let verification = outbox.0.lock().unwrap().pop().unwrap();
    assert_eq!(verification.to, "sam@example.com");
    assert!(verification.text.contains("/#/verify?token="));

    let ping = client.get(format!("{base}/ext/ping")).send().await.unwrap();
    assert_eq!(ping.text().await.unwrap(), "pong");
    let anonymous = client
        .get(format!("{base}/ext/whoami"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    let whoami = client
        .get(format!("{base}/ext/whoami"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(whoami, issued["tenant_id"].as_str().unwrap());

    let refused = client
        .post(format!("{base}/agents/reader/run"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409);
    assert!(refused.text().await.unwrap().contains("not included"));

    let thread: Value = client
        .post(format!("{base}/talk/threads"))
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let talk = client
        .post(format!(
            "{base}/talk/threads/{}/messages",
            thread["id"].as_str().unwrap()
        ))
        .bearer_auth(token)
        .json(&json!({"text": "Hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(talk.status(), 409, "the Talk agent may not run either");

    let source = "---\nname: Gated\ntrigger:\n  webhook: true\nsteps:\n  - agent: reader\n---\n";
    let saved = client
        .post(format!("{base}/automations"))
        .bearer_auth(token)
        .json(&json!({"source": source}))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    let started: Value = client
        .post(format!("{base}/automations/gated/run"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let run = started["id"].as_str().unwrap().to_owned();
    let mut detail = Value::Null;
    for _ in 0..200 {
        detail = client
            .get(format!("{base}/runs/{run}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if detail["run"]["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(detail["run"]["status"], "failed", "{detail}");
    assert!(
        detail.to_string().contains("not included"),
        "the agent step was refused: {detail}"
    );
    assert_eq!(
        run_to_end(&base, "reader").await["run"]["status"],
        "succeeded"
    );
}

/// An OpenAI compatible model that answers with a fixed text, for the tenant
/// key only.
async fn tenant_model() -> String {
    async fn complete(headers: axum::http::HeaderMap) -> axum::response::Response {
        use axum::response::IntoResponse;
        let authorized = headers
            .get("authorization")
            .is_some_and(|value| value == "Bearer tenant-key");
        if !authorized {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({
            "choices": [{"message": {"content": "from the tenant model"}}],
            "usage": {"prompt_tokens": 100000, "completion_tokens": 0},
        }))
        .into_response()
    }
    serve(Router::new().route("/v1/chat/completions", axum::routing::post(complete))).await
}

async fn send(base: &str, method: reqwest::Method, path: &str, body: Value) -> reqwest::StatusCode {
    reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn tenant_settings_take_effect_without_a_restart() {
    let (base, _dir, _database) = start_with("[tenancy]\nallow_private_endpoints = true\n").await;
    let model = tenant_model().await;
    let put = reqwest::Method::PUT;

    // The tenant routes text to its own provider. Without its key, runs fail
    // and name what is missing; with it, runs use the tenant's model.
    let status = send(
        &base,
        put.clone(),
        "/settings/tenant/providers/local",
        json!({"kind": "openai", "base_url": model}),
    )
    .await;
    assert_eq!(status, 204);
    let status = send(
        &base,
        put.clone(),
        "/settings/tenant/providers/local/models/text",
        json!({"model": "tenant-model", "input_eur_per_mtok": 2.0, "output_eur_per_mtok": 4.0}),
    )
    .await;
    assert_eq!(status, 204);
    let status = send(
        &base,
        put.clone(),
        "/settings/tenant/routing/text",
        json!({"provider": "local"}),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_to_end(&base, "reader").await;
    assert_eq!(detail["run"]["status"], "failed", "{detail}");
    assert!(
        detail
            .to_string()
            .contains("no API key stored for provider `local`"),
        "{detail}"
    );
    let status = send(
        &base,
        put.clone(),
        "/credentials/provider:local",
        json!({"value": "tenant-key"}),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_to_end(&base, "reader").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    assert_eq!(detail["run"]["report"], "from the tenant model");
    assert!(
        (detail["run"]["cost_eur"].as_f64().unwrap() - 0.2).abs() < 1e-9,
        "{detail}"
    );
    let models = get(&base, "/settings/models").await;
    assert_eq!(models[0]["provider"], "local");
    let agents = get(&base, "/agents").await;
    assert_eq!(agents[0]["model"]["model"], "tenant-model", "{agents}");

    // Switching the instance connector off blocks the agent that needs it; a
    // tenant connector to the same server under another name works at once.
    let status = send(
        &base,
        put.clone(),
        "/settings/tenant/connectors/demo/enabled",
        json!({"enabled": false}),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_to_end(&base, "reader").await;
    assert_eq!(detail["run"]["status"], "rejected", "{detail}");
    let connectors = get(&base, "/connectors").await;
    assert!(connectors.as_array().unwrap().is_empty(), "{connectors}");
    let mcp = get(&base, "/settings/effective").await;
    let url = mcp["connectors"][0]["url"].as_str().unwrap().to_owned();
    let status = send(
        &base,
        put.clone(),
        "/settings/tenant/connectors/issues",
        json!({"url": url, "read_tools": ["list_issues"]}),
    )
    .await;
    assert_eq!(status, 204);
    let connectors = get(&base, "/connectors").await;
    assert_eq!(connectors[0]["name"], "issues");
    let source = "---\nname: Issues\nscope: org\ntrigger:\n  manual: true\ntools:\n  - issues.list_issues\n---\nGo.\n";
    let response = reqwest::Client::new()
        .post(format!("{base}/agents"))
        .bearer_auth(TOKEN)
        .json(&json!({"source": source}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let saved: Value = response.json().await.unwrap();
    assert!(saved["blocked"].as_array().unwrap().is_empty(), "{saved}");
    let detail = run_to_end(&base, "issues").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    let status = send(
        &base,
        reqwest::Method::DELETE,
        "/settings/tenant/routing/text",
        json!({}),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_to_end(&base, "issues").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    assert_ne!(detail["run"]["report"], "from the tenant model");
}

const PASSWORD: &str = "a long enough password";

/// Signs in the configured owner and returns their token.
async fn owner_token(base: &str) -> String {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/auth/setup"))
        .bearer_auth(TOKEN)
        .json(&json!({"password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let login: Value = client
        .post(format!("{base}/auth/login"))
        .json(&json!({"password": PASSWORD, "device_name": "test", "device_kind": "web"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    login["token"].as_str().unwrap().to_owned()
}

/// Invites someone as a member and returns their token.
async fn member_token(base: &str, owner: &str, email: &str) -> String {
    let client = reqwest::Client::new();
    let invitation: Value = client
        .post(format!("{base}/tenant/invitations"))
        .bearer_auth(owner)
        .json(&json!({"email": email, "role": "member"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = invitation["accept_url"].as_str().unwrap();
    let token = url.split("token=").nth(1).unwrap();
    let accepted: Value = client
        .post(format!("{base}/auth/accept"))
        .json(
            &json!({"token": token, "name": "Robin", "password": PASSWORD,
                      "device_name": "web", "device_kind": "web"}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    accepted["token"].as_str().unwrap().to_owned()
}

/// Calls the API as someone and returns the status and body.
async fn as_user(
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

const MEMBER_TENANCY: &str = "[tenancy]\nsignup = \"invite\"\nallow_private_endpoints = true\n";

/// The address of the demo MCP server, from the instance connector.
async fn mcp_url(base: &str) -> String {
    let view = get(base, "/settings/effective").await;
    view["connectors"][0]["url"].as_str().unwrap().to_owned()
}

fn mail_source(connector: &str) -> Value {
    json!({"definition": {"connector": connector, "domain": "mail", "service": "mail", "collections": {
        "inbox": {"tool": "list_threads", "items": "threads", "kind": "thread", "fields": {
            "external_id": "id", "title": "subject", "author": "from", "occurred_at": "date",
            "unread": {"path": "labels", "contains": "UNREAD"}}}}}})
}

#[tokio::test]
async fn a_member_who_connects_their_own_mail_sees_it_and_nobody_else_does() {
    let (base, _dir, _database) = start_with(MEMBER_TENANCY).await;
    let owner = owner_token(&base).await;
    let robin = member_token(&base, &owner, "robin@example.com").await;
    let sam = member_token(&base, &owner, "sam@example.com").await;
    let url = mcp_url(&base).await;
    let put = reqwest::Method::PUT;
    let post = reqwest::Method::POST;
    let fetch = reqwest::Method::GET;

    // Without a listed read tool, or without a connector, there is no source.
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/sources/inbox",
        Some(mail_source("mymail")),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/connectors/mymail",
        Some(json!({"url": url, "auth": "none"})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/sources/inbox",
        Some(mail_source("mymail")),
    )
    .await;
    assert_eq!(status, 422, "the tool is not listed as read-only: {body}");
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/connectors/mymail",
        Some(json!({"url": url, "auth": "none", "read_tools": ["list_threads"]})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/sources/inbox",
        Some(mail_source("mymail")),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    // Sam may use the same name for their own.
    let (status, _) = as_user(
        &base,
        &sam,
        put.clone(),
        "/settings/tenant/sources/inbox",
        Some(mail_source("mymail")),
    )
    .await;
    assert_eq!(status, 403);

    // Only the owner of a source sees it, refreshes it and acts on it.
    let (_, sources) = as_user(&base, &robin, fetch.clone(), "/connected/sources", None).await;
    let mine = sources
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["personal"] == true)
        .unwrap();
    assert_eq!(mine["label"], "inbox");
    let key = mine["name"].as_str().unwrap().to_owned();
    let (_, sources) = as_user(&base, &owner, fetch.clone(), "/connected/sources", None).await;
    assert!(
        sources
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["personal"] == false),
        "{sources}"
    );
    let refresh = format!("/connected/sources/{key}/refresh");
    let (status, _) = as_user(&base, &sam, post.clone(), &refresh, None).await;
    assert_eq!(status, 404, "not Sam's");
    let (status, body) = as_user(&base, &robin, post.clone(), &refresh, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["errors"], json!([]), "{body}");
    assert!(body["items"].as_u64().unwrap() > 0, "{body}");

    // The mail reaches Robin's inbox, and only theirs.
    let (status, threads) = as_user(&base, &robin, fetch.clone(), "/mail/threads", None).await;
    assert_eq!(status, 200, "{threads}");
    let items = threads["items"].as_array().unwrap();
    assert!(!items.is_empty(), "{threads}");
    let thread = items[0]["id"].as_str().unwrap().to_owned();
    let (_, inbox) = as_user(&base, &robin, fetch.clone(), "/inbox", None).await;
    assert!(inbox.to_string().contains(&thread), "{inbox}");
    for other in [&owner, &sam] {
        let (_, threads) = as_user(&base, other, fetch.clone(), "/mail/threads", None).await;
        assert!(!threads.to_string().contains(&thread), "{threads}");
        let (_, inbox) = as_user(&base, other, fetch.clone(), "/inbox", None).await;
        assert!(!inbox.to_string().contains(&thread), "{inbox}");
        let (status, _) = as_user(
            &base,
            other,
            fetch.clone(),
            &format!("/connected/items/{thread}"),
            None,
        )
        .await;
        assert_eq!(status, 404);
    }

    // Deleting the source takes its items with it.
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::DELETE,
        "/settings/me/sources/inbox",
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (_, threads) = as_user(&base, &robin, fetch, "/mail/threads", None).await;
    assert!(!threads.to_string().contains(&thread), "{threads}");
}

/// An OpenAI compatible model for one key: it calls the first tool it is
/// offered, then answers with what the tool returned.
async fn tool_model(key: &'static str) -> String {
    let complete = move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
        use axum::response::IntoResponse;
        if headers
            .get("authorization")
            .is_none_or(|value| value != format!("Bearer {key}").as_str())
        {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        let message = if last["role"] == "tool" {
            json!({"content": format!("the tool said: {}", last["content"])})
        } else {
            let tool = body["tools"][0]["function"]["name"].as_str().unwrap();
            json!({"content": null, "tool_calls": [{"id": "call-1", "type": "function",
                   "function": {"name": tool, "arguments": "{}"}}]})
        };
        axum::Json(json!({
            "choices": [{"message": message}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 10},
        }))
        .into_response()
    };
    serve(Router::new().route("/v1/chat/completions", axum::routing::post(complete))).await
}

fn personal_agent(tool: &str) -> String {
    format!(
        "---\nname: Mine\nscope: org\ntrigger:\n  manual: true\ntools:\n  - {tool}\n---\nRead my mail.\n"
    )
}

async fn run_as(base: &str, token: &str, agent: &str) -> Value {
    let (status, started) = as_user(
        base,
        token,
        reqwest::Method::POST,
        &format!("/agents/{agent}/run"),
        None,
    )
    .await;
    assert_eq!(status, 202, "{started}");
    let id = started["id"].as_str().unwrap().to_owned();
    for _ in 0..200 {
        let (_, detail) = as_user(
            base,
            token,
            reqwest::Method::GET,
            &format!("/runs/{id}"),
            None,
        )
        .await;
        if detail["run"]["status"] != "running" {
            return detail;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run did not finish");
}

#[tokio::test]
async fn a_member_runs_their_own_agent_with_their_own_key_and_connector() {
    let (base, _dir, _database) = start_with(MEMBER_TENANCY).await;
    let owner = owner_token(&base).await;
    let robin = member_token(&base, &owner, "robin@example.com").await;
    let sam = member_token(&base, &owner, "sam@example.com").await;
    let model = tool_model("robins-key").await;
    let url = mcp_url(&base).await;
    let put = reqwest::Method::PUT;
    for (path, body) in [
        (
            "/settings/me/providers/own",
            json!({"kind": "openai", "base_url": model}),
        ),
        (
            "/settings/me/providers/own/models/text",
            json!({"model": "robins-model", "input_eur_per_mtok": 1.0, "output_eur_per_mtok": 1.0}),
        ),
        ("/settings/me/routing/text", json!({"provider": "own"})),
        (
            "/me/credentials/provider:own",
            json!({"value": "robins-key"}),
        ),
        (
            "/settings/me/connectors/mymail",
            json!({"url": url, "auth": "bearer", "read_tools": ["list_threads"]}),
        ),
        (
            "/me/credentials/mcp:mymail",
            json!({"value": "robins-token"}),
        ),
    ] {
        let (status, body) = as_user(&base, &robin, put.clone(), path, Some(body)).await;
        assert_eq!(status, 204, "{path}: {body}");
    }
    let (status, agent) = as_user(
        &base,
        &robin,
        reqwest::Method::POST,
        "/agents",
        Some(json!({"source": personal_agent("mymail.list_threads")})),
    )
    .await;
    assert_eq!(status, 200, "{agent}");
    assert_eq!(agent["layer"], "personal");
    assert_eq!(agent["blocked"], json!([]), "{agent}");

    let detail = run_as(&base, &robin, "mine").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    let report = detail["run"]["report"].as_str().unwrap();
    assert!(
        report.starts_with("the tool said:") && report.contains("threads"),
        "{report}"
    );
    let steps = detail["steps"].to_string();
    assert!(steps.contains("provider=own"), "{steps}");
    assert!(steps.contains("mymail.list_threads"), "{steps}");

    // Nobody else has the agent, the route, the key or the connector.
    for other in [&owner, &sam] {
        let (status, _) = as_user(&base, other, reqwest::Method::GET, "/agents/mine", None).await;
        assert_eq!(status, 404);
        let (status, _) = as_user(
            &base,
            other,
            reqwest::Method::POST,
            "/agents/mine/run",
            None,
        )
        .await;
        assert_eq!(status, 404);
        let (_, runs) = as_user(&base, other, reqwest::Method::GET, "/runs", None).await;
        assert!(!runs.to_string().contains("\"mine\""), "{runs}");
        let detail = run_as(&base, other, "reader").await;
        assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
        assert!(
            detail["steps"].to_string().contains("provider=mock"),
            "{detail}"
        );
    }
    // Robin's other runs use Robin's model too.
    let detail = run_as(&base, &robin, "reader").await;
    assert!(
        detail["steps"].to_string().contains("provider=own"),
        "{detail}"
    );

    // Members keep their hands off shared agents; the switches work per layer.
    let source = "---\nname: Shared\nscope: org\ntrigger:\n  manual: true\n---\nGo.\n";
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::POST,
        "/agents?layer=tenant",
        Some(json!({"source": source})),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = as_user(
        &base,
        &owner,
        reqwest::Method::POST,
        "/agents",
        Some(json!({"source": source})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::DELETE,
        "/agents/shared",
        None,
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/agents/shared/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::POST,
        "/agents/shared/run",
        None,
    )
    .await;
    assert_eq!(status, 409, "switched off for Robin");
    let detail = run_as(&base, &sam, "shared").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");
    let (status, _) = as_user(
        &base,
        &owner,
        put.clone(),
        "/settings/tenant/agents/reader/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = as_user(
        &base,
        &sam,
        reqwest::Method::POST,
        "/agents/reader/run",
        None,
    )
    .await;
    assert_eq!(status, 409, "the tenant switched a built-in agent off");
    let (status, _) = as_user(
        &base,
        &owner,
        put.clone(),
        "/settings/tenant/agents/nothing/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, 404);
    // A member's agent cannot mail others.
    let relay = "---\nname: Relay\nscope: org\ntrigger:\n  manual: true\ndeliver: [mail:someone@example.com]\n---\nGo.\n";
    let (status, body) = as_user(
        &base,
        &robin,
        reqwest::Method::POST,
        "/agents",
        Some(json!({"source": relay})),
    )
    .await;
    assert_eq!(status, 422, "{body}");

    // Automations and skills work the same way.
    let automation =
        "---\nname: Daily\ntrigger:\n  event: mine.happened\nsteps:\n  - agent: mine\n---\nMine.\n";
    let (status, body) = as_user(
        &base,
        &robin,
        reqwest::Method::POST,
        "/automations",
        Some(json!({"source": automation})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["layer"], "personal");
    let (_, list) = as_user(&base, &sam, reqwest::Method::GET, "/automations", None).await;
    assert!(!list.to_string().contains("daily"), "{list}");
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/automations/daily/enabled",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, 204);
    let (_, one) = as_user(
        &base,
        &robin,
        reqwest::Method::GET,
        "/automations/daily",
        None,
    )
    .await;
    assert_eq!(one["enabled"], false, "{one}");
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::DELETE,
        "/automations/daily",
        None,
    )
    .await;
    assert_eq!(status, 204);
    let skill = "---\nname: tone\ndescription: How I write\n---\nBe brief.\n";
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/skills/tone",
        Some(json!({"source": skill})),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/skills/other",
        Some(json!({"source": skill})),
    )
    .await;
    assert_eq!(status, 422);
    let (_, skills) = as_user(&base, &robin, reqwest::Method::GET, "/skills", None).await;
    assert_eq!(skills[0]["layer"], "personal", "{skills}");
    let (_, skills) = as_user(&base, &sam, reqwest::Method::GET, "/skills", None).await;
    assert!(!skills.to_string().contains("tone"), "{skills}");
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::DELETE,
        "/settings/me/skills/tone",
        None,
    )
    .await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn publishing_and_brain_models_change_per_layer() {
    let (base, _dir, _database) = start_with(MEMBER_TENANCY).await;
    let owner = owner_token(&base).await;
    let robin = member_token(&base, &owner, "robin@example.com").await;
    let put = reqwest::Method::PUT;
    let (_, values) = as_user(
        &base,
        &robin,
        reqwest::Method::GET,
        "/settings/values",
        None,
    )
    .await;
    let value = |values: &Value, key: &str| {
        values
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["key"] == key)
            .unwrap()
            .clone()
    };
    assert_eq!(value(&values, "publishing")["source"], "instance");
    assert_eq!(value(&values, "publishing")["value"]["tool"], "echo");
    let post = json!({"body": "Hello", "channel": "demo"});
    let (status, created) =
        as_user(&base, &robin, reqwest::Method::POST, "/posts", Some(post)).await;
    assert_eq!(status, 201, "{created}");
    let publish = format!("/posts/{}/publish", created["id"].as_str().unwrap());
    let (status, body) = as_user(&base, &robin, reqwest::Method::POST, &publish, None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["tool"], "demo.echo");
    let post = json!({"body": "Again", "channel": "demo"});
    let (_, created) = as_user(&base, &robin, reqwest::Method::POST, "/posts", Some(post)).await;
    let publish = format!("/posts/{}/publish", created["id"].as_str().unwrap());
    // A person's own setting names another tool, which does not exist for the policy as read-only.
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/values/publishing",
        Some(json!({"connector": "demo", "tool": "list_issues"})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, body) = as_user(&base, &robin, reqwest::Method::POST, &publish, None).await;
    assert!(
        body.to_string().contains("read or draft tool"),
        "{status}: {body}"
    );
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::DELETE,
        "/settings/me/values/publishing",
        None,
    )
    .await;
    assert_eq!(status, 204);

    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/values/brain",
        Some(json!({"embedding_model": "mine"})),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    let (status, _) = as_user(
        &base,
        &owner,
        put.clone(),
        "/settings/tenant/values/brain",
        Some(json!({"provider": "mock", "embedding_model": "new-model"})),
    )
    .await;
    assert_eq!(status, 204);
    let (_, values) = as_user(
        &base,
        &robin,
        reqwest::Method::GET,
        "/settings/values",
        None,
    )
    .await;
    assert_eq!(value(&values, "brain")["source"], "tenant");
    // Search keeps working with a model that has no vectors or key yet.
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::GET,
        "/brain/search?q=anything",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = as_user(
        &base,
        &robin,
        put,
        "/settings/me/values/slack_channels",
        Some(json!(["#mine"])),
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = as_user(
        &base,
        &robin,
        reqwest::Method::GET,
        "/settings/values",
        None,
    )
    .await;
    assert_eq!(status, 200);
}

const OAUTH_TOKEN: &str = "test-access-token";

#[derive(Clone, Default)]
struct AuthStub {
    base: Arc<std::sync::Mutex<String>>,
    exchanged: Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
}

async fn stub_guard(
    axum::extract::State(stub): axum::extract::State<AuthStub>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let authorized = request
        .headers()
        .get("authorization")
        .is_some_and(|value| value == format!("Bearer {OAUTH_TOKEN}").as_str());
    if authorized {
        return next.run(request).await;
    }
    let base = stub.base.lock().unwrap().clone();
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(
            "www-authenticate",
            format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""),
        )],
    )
        .into_response()
}

/// An authorization server with a protected MCP server behind it.
async fn auth_stub() -> (String, AuthStub) {
    use axum::routing::{get as route_get, post as route_post};
    let stub = AuthStub::default();
    let service = StreamableHttpService::new(
        || Ok(DemoServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let protected =
        Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn_with_state(
                stub.clone(),
                stub_guard,
            ));
    let resource = |axum::extract::State(stub): axum::extract::State<AuthStub>| async move {
        let base = stub.base.lock().unwrap().clone();
        axum::Json(json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}))
    };
    let router = Router::new()
        .route("/.well-known/oauth-protected-resource", route_get(resource))
        .route("/.well-known/oauth-protected-resource/mcp", route_get(resource))
        .route("/.well-known/oauth-authorization-server", route_get(
            |axum::extract::State(stub): axum::extract::State<AuthStub>| async move {
                let base = stub.base.lock().unwrap().clone();
                axum::Json(json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                }))
            },
        ))
        .route("/register", route_post(|axum::Json(body): axum::Json<Value>| async move {
            (axum::http::StatusCode::CREATED,
             axum::Json(json!({"client_id": "registered", "redirect_uris": body["redirect_uris"]})))
        }))
        .route("/token", route_post(
            |axum::extract::State(stub): axum::extract::State<AuthStub>,
             axum::Form(form): axum::Form<std::collections::HashMap<String, String>>| async move {
                stub.exchanged.lock().unwrap().push(form);
                axum::Json(json!({"access_token": OAUTH_TOKEN, "token_type": "Bearer",
                                  "refresh_token": "refresh", "expires_in": 3600}))
            },
        ))
        .with_state(stub.clone())
        .merge(protected);
    let base = serve(router).await;
    *stub.base.lock().unwrap() = base.clone();
    (base, stub)
}

#[tokio::test]
async fn a_member_connects_with_oauth_and_only_their_runs_use_the_tokens() {
    let (base, _dir, _database) = start_with(MEMBER_TENANCY).await;
    let owner = owner_token(&base).await;
    let robin = member_token(&base, &owner, "robin@example.com").await;
    let (auth, stub) = auth_stub().await;
    let put = reqwest::Method::PUT;
    let post = reqwest::Method::POST;
    let fetch = reqwest::Method::GET;
    let connector =
        json!({"url": format!("{auth}/mcp"), "auth": "oauth", "read_tools": ["list_issues"]});
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/connectors/guarded",
        Some(connector),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    let agent = "---\nname: Guarded\nscope: org\ntrigger:\n  manual: true\ntools:\n  - guarded.list_issues\n---\nGo.\n";
    let (status, body) = as_user(
        &base,
        &robin,
        post.clone(),
        "/agents",
        Some(json!({"source": agent})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let detail = run_as(&base, &robin, "guarded").await;
    assert_eq!(
        detail["run"]["status"], "failed",
        "not connected yet: {detail}"
    );

    // The flow: start, the provider's redirect, the callback, stored tokens.
    let (status, started) = as_user(
        &base,
        &robin,
        post.clone(),
        "/connectors/guarded/connect",
        None,
    )
    .await;
    assert_eq!(status, 200, "{started}");
    let authorize = started["authorize_url"].as_str().unwrap();
    assert!(
        authorize.starts_with(&format!("{auth}/authorize")),
        "{authorize}"
    );
    let state = reqwest::Url::parse(authorize)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let response = reqwest::get(format!("{base}/oauth/callback?code=the-code&state={state}"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("Connected"));
    let exchange = stub.exchanged.lock().unwrap()[0].clone();
    assert_eq!(exchange["code"], "the-code");
    assert!(exchange.contains_key("code_verifier"), "{exchange:?}");

    // The tokens are Robin's: in their vault, and nobody else's.
    let (_, mine) = as_user(&base, &robin, fetch.clone(), "/me/credentials", None).await;
    assert_eq!(mine, json!(["oauth:guarded"]));
    let (_, shared) = as_user(&base, &owner, fetch.clone(), "/credentials", None).await;
    assert!(!shared.to_string().contains("guarded"), "{shared}");
    let (_, view) = as_user(&base, &robin, fetch.clone(), "/settings/effective", None).await;
    let row = view["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "guarded")
        .unwrap();
    assert_eq!(row["connected"], "personal", "{row}");
    let detail = run_as(&base, &robin, "guarded").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");

    // A personal bearer token reaches the server, and a wrong one does not pass.
    let bearer =
        json!({"url": format!("{auth}/mcp"), "auth": "bearer", "read_tools": ["list_issues"]});
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/connectors/keyed",
        Some(bearer),
    )
    .await;
    assert_eq!(status, 204);
    let agent = "---\nname: Keyed\nscope: org\ntrigger:\n  manual: true\ntools:\n  - keyed.list_issues\n---\nGo.\n";
    let (status, _) = as_user(
        &base,
        &robin,
        post.clone(),
        "/agents",
        Some(json!({"source": agent})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/me/credentials/mcp:keyed",
        Some(json!({"value": "wrong"})),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_as(&base, &robin, "keyed").await;
    assert_eq!(detail["run"]["status"], "failed", "{detail}");
    let (status, _) = as_user(
        &base,
        &robin,
        put.clone(),
        "/me/credentials/mcp:keyed",
        Some(json!({"value": OAUTH_TOKEN})),
    )
    .await;
    assert_eq!(status, 204);
    let detail = run_as(&base, &robin, "keyed").await;
    assert_eq!(detail["run"]["status"], "succeeded", "{detail}");

    // A personal source never reads through the account a workspace shares.
    let shared =
        json!({"url": format!("{auth}/mcp"), "auth": "bearer", "read_tools": ["list_threads"]});
    let (status, _) = as_user(
        &base,
        &owner,
        put.clone(),
        "/settings/tenant/connectors/team",
        Some(shared),
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = as_user(
        &base,
        &owner,
        put.clone(),
        "/credentials/mcp:team",
        Some(json!({"value": OAUTH_TOKEN})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, body) = as_user(
        &base,
        &robin,
        put.clone(),
        "/settings/me/sources/inbox",
        Some(mail_source("team")),
    )
    .await;
    assert_eq!(status, 204, "{body}");
    let (_, sources) = as_user(&base, &robin, fetch.clone(), "/connected/sources", None).await;
    let key = sources
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["label"] == "inbox")
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_owned();
    let refresh = format!("/connected/sources/{key}/refresh");
    let (status, body) = as_user(&base, &robin, post.clone(), &refresh, None).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["errors"]
            .to_string()
            .contains("connect your own account"),
        "{body}"
    );
    let (_, threads) = as_user(&base, &robin, fetch.clone(), "/mail/threads", None).await;
    assert_eq!(threads["items"], json!([]), "{threads}");
    let (status, _) = as_user(
        &base,
        &robin,
        put,
        "/me/credentials/mcp:team",
        Some(json!({"value": OAUTH_TOKEN})),
    )
    .await;
    assert_eq!(status, 204);
    let (status, body) = as_user(&base, &robin, post, &refresh, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["errors"], json!([]), "{body}");
    let (_, threads) = as_user(&base, &robin, fetch, "/mail/threads", None).await;
    assert!(
        !threads["items"].as_array().unwrap().is_empty(),
        "{threads}"
    );
}
