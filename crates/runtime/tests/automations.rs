//! Automation runs with a mock tool host, a fake agent pool and a real database.

use async_trait::async_trait;
use lighthouse_audit::{AuditLog, RunDetail};
use lighthouse_config::{ConnectorConfig, ConnectorTransport};
use lighthouse_core::agent::Delivery;
use lighthouse_core::automation::Automation;
use lighthouse_core::{RunId, TenantId};
use lighthouse_desk::Desk;
use lighthouse_mcp::{McpError, ToolHost, ToolHostFactory, ToolOutput, ToolPolicy};
use lighthouse_notify::{Notifier, NotifyError};
use lighthouse_router::{Router, ToolSpec};
use lighthouse_runtime::{AgentOutcome, AgentSteps, Runtime};
use lighthouse_skills::SkillLibrary;
use lighthouse_store::testing::fresh_store;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

struct Host(Calls);

#[async_trait]
impl ToolHost for Host {
    async fn list_tools(&self, _: &[String]) -> Result<Vec<ToolSpec>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        self.0.lock().unwrap().push((tool.to_owned(), arguments));
        Ok(ToolOutput {
            text: format!("{tool} says URGENT"),
            is_error: false,
        })
    }
}

struct Tools(Calls);

#[async_trait]
impl ToolHostFactory for Tools {
    async fn open(&self, tools: &[String], _: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        if tools.iter().any(|tool| tool.starts_with("offline.")) {
            return Err(McpError::UnknownConnector("offline".to_owned()));
        }
        Ok(Box::new(Host(self.0.clone())))
    }
}

#[derive(Default)]
struct Mailbox(Mutex<Vec<(String, String)>>);

#[async_trait]
impl Notifier for Mailbox {
    async fn deliver(&self, _: &Delivery, subject: &str, text: &str) -> Result<(), NotifyError> {
        self.0
            .lock()
            .unwrap()
            .push((subject.to_owned(), text.to_owned()));
        Ok(())
    }
}

#[derive(Default)]
struct Agents {
    fail: bool,
    contexts: Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl AgentSteps for Agents {
    async fn run_agent(
        &self,
        agent_id: &str,
        trigger: &str,
        context: &str,
    ) -> Result<AgentOutcome, String> {
        self.contexts.lock().unwrap().push((
            agent_id.to_owned(),
            trigger.to_owned(),
            context.to_owned(),
        ));
        if agent_id == "missing" {
            return Err("unknown agent `missing`".to_owned());
        }
        Ok(AgentOutcome {
            run_id: Uuid::now_v7(),
            status: if self.fail { "failed" } else { "succeeded" }.to_owned(),
            report: (!self.fail).then(|| "Triage: all calm".to_owned()),
            reason: self.fail.then(|| "model error".to_owned()),
        })
    }
}

struct Fixture {
    runtime: Runtime,
    audit: AuditLog,
    calls: Calls,
    mailbox: Arc<Mailbox>,
}

async fn fixture() -> Fixture {
    let store = fresh_store().await;
    let audit = AuditLog::new(store.clone(), TenantId::SINGLE);
    let connector = ConnectorConfig {
        transport: ConnectorTransport::Stdio {
            command: "x".to_owned(),
            args: Vec::new(),
            secret_env: BTreeMap::new(),
        },
        read_tools: vec!["list_issues".to_owned()],
        draft_tools: Vec::new(),
    };
    let calls = Calls::default();
    let mailbox = Arc::new(Mailbox::default());
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        router: Arc::new(Arc::new(Router::default())),
        tools: Arc::new(Tools(calls.clone())),
        policy: ToolPolicy::new(&BTreeMap::from([
            ("linear".to_owned(), connector.clone()),
            ("offline".to_owned(), connector),
        ])),
        skills: SkillLibrary::load(dir.path()).unwrap(),
        audit: audit.clone(),
        notifier: mailbox.clone(),
        owner_context: "You work for Alex.".to_owned(),
        owner_email: "alex@example.com".to_owned(),
        zone: chrono_tz::Europe::Berlin,
        desk: Desk::new(store, TenantId::SINGLE),
    };
    Fixture {
        runtime,
        audit,
        calls,
        mailbox,
    }
}

fn automation(steps: &str) -> Automation {
    Automation::parse(&format!(
        "---\nname: Follow up\ntrigger:\n  event: mail.received\nsteps:\n{steps}\n---\n"
    ))
    .unwrap()
}

async fn start(fixture: &Fixture, automation: &Automation, agents: &Agents) -> RunDetail {
    let run = RunId::new();
    fixture
        .audit
        .start_run(run, &automation.run_agent_id(), "event:mail.received")
        .await
        .unwrap();
    let event = json!({"from": "client@example.com", "subject": "Offer"});
    fixture
        .runtime
        .run_automation(run, automation, event, agents)
        .await
        .unwrap();
    fixture
        .audit
        .run_detail(run.as_uuid())
        .await
        .unwrap()
        .unwrap()
}

fn kinds(detail: &RunDetail) -> Vec<(String, String, String)> {
    detail
        .steps
        .iter()
        .map(|step| (step.kind.clone(), step.name.clone(), step.status.clone()))
        .collect()
}

#[tokio::test]
async fn steps_run_in_order_and_pass_their_output_on() {
    let fixture = fixture().await;
    let agents = Agents::default();
    let automation = automation(
        "  - agent: triage\n  - tool:\n      name: linear.list_issues\n      input:\n        query: \"{{event.from}}\"\n  - if:\n      contains: urgent\n  - notify:\n      message: \"{{automation}}: {{output}}\"",
    );
    let detail = start(&fixture, &automation, &agents).await;
    assert_eq!(detail.run.status, "succeeded", "{:?}", detail.run.reason);
    assert_eq!(detail.run.agent_id, "automation:follow-up");
    let steps = kinds(&detail);
    assert_eq!(steps[0], ("agent".into(), "triage".into(), "ok".into()));
    assert_eq!(
        steps[1],
        ("tool".into(), "linear.list_issues".into(), "ok".into())
    );
    assert_eq!(
        steps[2],
        ("condition".into(), "contains urgent".into(), "ok".into())
    );
    assert_eq!(
        steps[3],
        (
            "delivery".into(),
            "mail:alex@example.com".into(),
            "ok".into()
        )
    );

    let contexts = agents.contexts.lock().unwrap().clone();
    assert_eq!(contexts[0].1, "automation:follow-up");
    assert!(contexts[0].2.contains("client@example.com"));
    let calls = fixture.calls.lock().unwrap().clone();
    assert_eq!(calls[0].1, json!({"query": "client@example.com"}));
    let mail = fixture.mailbox.0.lock().unwrap().clone();
    assert_eq!(
        mail[0],
        (
            "Follow up".to_owned(),
            "Follow up: linear.list_issues says URGENT".to_owned()
        )
    );
}

#[tokio::test]
async fn a_condition_that_does_not_hold_stops_the_run() {
    let fixture = fixture().await;
    let automation = automation(
        "  - agent: triage\n  - if:\n      contains: urgent\n  - notify:\n      message: never",
    );
    let detail = start(&fixture, &automation, &Agents::default()).await;
    assert_eq!(detail.run.status, "succeeded");
    assert!(
        detail
            .run
            .report
            .as_deref()
            .unwrap()
            .contains("did not hold")
    );
    assert_eq!(kinds(&detail)[1].2, "skipped");
    assert!(fixture.mailbox.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_last_step_fails_the_run_and_status_conditions_see_it() {
    let fixture = fixture().await;
    let failing = Agents {
        fail: true,
        ..Agents::default()
    };
    let detail = start(&fixture, &automation("  - agent: triage"), &failing).await;
    assert_eq!(detail.run.status, "failed");
    assert!(detail.run.reason.unwrap().contains("model error"));

    let handled = automation(
        "  - agent: missing\n  - if:\n      status: failed\n  - tool:\n      name: offline.list_issues\n  - notify:\n      to: \"slack:#ops\"\n      subject: \"Problem in {{automation}}\"\n      message: \"{{status}}\"",
    );
    let detail = start(&fixture, &handled, &Agents::default()).await;
    let steps = kinds(&detail);
    assert_eq!(steps[0].2, "error");
    assert_eq!(steps[1].2, "ok");
    assert_eq!(
        steps[2],
        ("tool".into(), "offline.list_issues".into(), "error".into())
    );
    assert_eq!(steps[3].1, "slack:#ops");
    assert_eq!(detail.run.status, "succeeded", "the notification went out");
    let mail = fixture.mailbox.0.lock().unwrap().clone();
    assert_eq!(
        mail[0],
        ("Problem in Follow up".to_owned(), "failed".to_owned())
    );
}

async fn paused(fixture: &Fixture) -> (RunId, Automation) {
    let automation = automation(
        "  - tool:\n      name: linear.create_issue\n      input:\n        title: \"Answer {{event.from}}\"\n  - notify:\n      message: \"{{output}}\"",
    );
    let run = RunId::new();
    fixture
        .audit
        .start_run(run, &automation.run_agent_id(), "webhook")
        .await
        .unwrap();
    fixture
        .runtime
        .run_automation(
            run,
            &automation,
            json!({"from": "client@example.com"}),
            &Agents::default(),
        )
        .await
        .unwrap();
    (run, automation)
}

#[tokio::test]
async fn a_tool_with_external_effect_waits_for_approval() {
    let fixture = fixture().await;
    let (run, automation) = paused(&fixture).await;
    let detail = fixture
        .audit
        .run_detail(run.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.run.status, "waiting");
    assert!(fixture.calls.lock().unwrap().is_empty(), "nothing ran yet");
    let approvals = fixture.audit.run_approvals(run.as_uuid()).await.unwrap();
    assert_eq!(approvals[0].agent_id, "automation:follow-up");
    assert_eq!(
        approvals[0].input,
        json!({"title": "Answer client@example.com"})
    );
    assert!(approvals[0].quotes_external);

    let edited = json!({"title": "Answer the client"});
    fixture
        .audit
        .decide_approval(approvals[0].id, true, Some(&edited))
        .await
        .unwrap();
    let checkpoint = fixture
        .audit
        .take_checkpoint(run.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.agent_id, "automation:follow-up");
    fixture
        .runtime
        .resume_automation(run, &automation, checkpoint.state, &Agents::default())
        .await
        .unwrap();
    let detail = fixture
        .audit
        .run_detail(run.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.run.status, "succeeded");
    assert_eq!(fixture.calls.lock().unwrap()[0].1, edited);
    assert_eq!(fixture.mailbox.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_denied_tool_stops_the_run_without_acting() {
    let fixture = fixture().await;
    let (run, automation) = paused(&fixture).await;
    let approvals = fixture.audit.run_approvals(run.as_uuid()).await.unwrap();
    fixture
        .audit
        .decide_approval(approvals[0].id, false, None)
        .await
        .unwrap();
    let checkpoint = fixture
        .audit
        .take_checkpoint(run.as_uuid())
        .await
        .unwrap()
        .unwrap();
    fixture
        .runtime
        .resume_automation(run, &automation, checkpoint.state, &Agents::default())
        .await
        .unwrap();
    let detail = fixture
        .audit
        .run_detail(run.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.run.status, "succeeded");
    assert!(detail.run.report.unwrap().contains("did not approve"));
    assert!(fixture.calls.lock().unwrap().is_empty());
    assert!(fixture.mailbox.0.lock().unwrap().is_empty());

    let broken = RunId::new();
    fixture
        .audit
        .start_run(broken, "automation:follow-up", "webhook")
        .await
        .unwrap();
    fixture
        .runtime
        .resume_automation(broken, &automation, json!("garbage"), &Agents::default())
        .await
        .unwrap();
    let detail = fixture
        .audit
        .run_detail(broken.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.run.status, "failed");
}

#[tokio::test]
async fn actions_run_at_once_or_wait_for_approval() {
    use lighthouse_runtime::ActionOutcome;
    let fixture = fixture().await;
    let done = fixture
        .runtime
        .act(
            "action:linear",
            "linear.list_issues",
            json!({"q": "x"}),
            "Look up",
        )
        .await
        .unwrap();
    let ActionOutcome::Done { run, ok, output } = done else {
        panic!("a read tool runs at once")
    };
    assert!(ok);
    assert!(output.contains("URGENT"));
    let detail = fixture.audit.run_detail(run).await.unwrap().unwrap();
    assert_eq!(detail.run.status, "succeeded");

    let failed = fixture
        .runtime
        .act(
            "action:offline",
            "offline.list_issues",
            json!({}),
            "Look up",
        )
        .await
        .unwrap();
    assert!(matches!(failed, ActionOutcome::Done { ok: false, .. }));

    for approve in [true, false] {
        let proposed = fixture
            .runtime
            .act(
                "action:linear",
                "linear.create_comment",
                json!({"body": "Thanks"}),
                "Comment on ENG-1",
            )
            .await
            .unwrap();
        let ActionOutcome::Proposed { run, approval } = proposed else {
            panic!("a write tool waits")
        };
        let approvals = fixture.audit.run_approvals(run).await.unwrap();
        assert_eq!(approvals[0].reason.as_deref(), Some("Comment on ENG-1"));
        fixture
            .audit
            .decide_approval(approval, approve, None)
            .await
            .unwrap();
        let checkpoint = fixture.audit.take_checkpoint(run).await.unwrap().unwrap();
        assert_eq!(checkpoint.agent_id, "action:linear");
        fixture
            .runtime
            .resume_action(RunId::from_uuid(run), checkpoint.state)
            .await
            .unwrap();
        let detail = fixture.audit.run_detail(run).await.unwrap().unwrap();
        assert_eq!(detail.run.status, "succeeded");
        let comments = fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(tool, _)| tool == "linear.create_comment")
            .count();
        assert_eq!(comments, 1, "only the approved comment was made");
    }
    let broken = RunId::new();
    fixture
        .audit
        .start_run(broken, "action:linear", "owner")
        .await
        .unwrap();
    fixture
        .runtime
        .resume_action(broken, json!("garbage"))
        .await
        .unwrap();
    let detail = fixture
        .audit
        .run_detail(broken.as_uuid())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.run.status, "failed");
}
