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
[brain]
path = "{brain}"
"#,
        agents = agents.display(),
        created = dir.path().join("created").display(),
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
