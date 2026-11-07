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
    let path = dir.path().join("lighthouse.toml");
    std::fs::write(&path, toml).unwrap();
    let app = lighthouse_server::build(Config::load(&path).unwrap())
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
    let audit = AuditLog::new(database, TenantId::SINGLE);
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
