//! Built-in tools with which agents propose tasks and facts to the owner.
//!
//! Both are `draft` tools: they never act and never reach anyone else. What an
//! agent proposes waits until the owner accepts or confirms it.

use async_trait::async_trait;
use lighthouse_core::RunId;
use lighthouse_desk::{Desk, NewTask, ProposedFact, Proposer, SourceLink};
use lighthouse_mcp::{McpError, ToolHost, ToolHostFactory, ToolOutput};
use lighthouse_router::ToolSpec;
use serde_json::{Value, json};
use std::sync::Arc;

/// Qualified name of the task proposal tool.
pub const PROPOSE_TASK: &str = "tasks.propose";
/// Qualified name of the fact proposal tool.
pub const PROPOSE_FACT: &str = "memory.propose";
/// Qualified name of the post proposal tool.
pub const PROPOSE_POST: &str = "posts.propose";
const DEFAULT_CONFIDENCE: f64 = 0.5;

fn spec(tool: &str) -> Option<ToolSpec> {
    let (description, input_schema) = match tool {
        PROPOSE_TASK => (
            "Proposes a task for the owner's to-do list. The owner accepts or rejects it.",
            json!({"type": "object", "properties": {
                "title": {"type": "string"},
                "notes": {"type": "string"},
                "due_on": {"type": "string", "description": "YYYY-MM-DD"},
                "priority": {"type": "integer", "minimum": 0, "maximum": 3},
                "source": {"type": "object", "properties": {
                    "kind": {"type": "string"},
                    "reference": {"type": "string"},
                    "label": {"type": "string"},
                    "url": {"type": "string"}
                }, "required": ["kind", "reference"]}
            }, "required": ["title"]}),
        ),
        PROPOSE_FACT => (
            "Proposes a fact about the owner, such as a preference. The owner confirms it first.",
            json!({"type": "object", "properties": {
                "text": {"type": "string"},
                "category": {"type": "string"},
                "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                "evidence": {"type": "string", "description": "What you learned it from"}
            }, "required": ["text"]}),
        ),
        PROPOSE_POST => (
            "Proposes a post for the owner, written in their voice. It lands as an idea; the owner edits, plans and publishes it.",
            json!({"type": "object", "properties": {
                "body": {"type": "string"},
                "title": {"type": "string"},
                "pillar": {"type": "string"},
                "topic": {"type": "string"},
                "channel": {"type": "string", "description": "Network, such as linkedin"}
            }, "required": ["body"]}),
        ),
        _ => return None,
    };
    Some(ToolSpec {
        name: tool.to_owned(),
        description: description.to_owned(),
        input_schema,
    })
}

fn is_desk_tool(tool: &str) -> bool {
    tool == PROPOSE_TASK || tool == PROPOSE_FACT || tool == PROPOSE_POST
}

struct DeskHost {
    desk: Desk,
    proposer: Proposer,
    inner: Option<Box<dyn ToolHost>>,
}

fn text(input: &Value, name: &str) -> Option<String> {
    input[name].as_str().map(str::to_owned)
}

impl DeskHost {
    async fn dispatch(&self, tool: &str, input: &Value) -> Result<String, String> {
        match tool {
            PROPOSE_TASK => {
                let source = match input.get("source") {
                    Some(value) if !value.is_null() => Some(
                        serde_json::from_value::<SourceLink>(value.clone())
                            .map_err(|_| "`source` needs a kind and a reference".to_owned())?,
                    ),
                    _ => None,
                };
                let due_on = text(input, "due_on")
                    .map(|day| {
                        day.parse()
                            .map_err(|_| "`due_on` is not a date like 2026-01-31".to_owned())
                    })
                    .transpose()?;
                let task = NewTask {
                    title: text(input, "title").unwrap_or_default(),
                    notes: text(input, "notes")
                        .or_else(|| lighthouse_core::proposal::reason(input)),
                    due_on,
                    priority: input["priority"]
                        .as_i64()
                        .and_then(|value| i16::try_from(value).ok())
                        .unwrap_or(0),
                    source,
                };
                let task = self
                    .desk
                    .create_task(task, Some(self.proposer.clone()))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("Proposed as task {}", task.id))
            }
            PROPOSE_FACT => {
                let fact = ProposedFact {
                    text: text(input, "text").unwrap_or_default(),
                    category: text(input, "category"),
                    confidence: input["confidence"].as_f64().unwrap_or(DEFAULT_CONFIDENCE),
                    evidence: text(input, "evidence")
                        .or_else(|| lighthouse_core::proposal::reason(input)),
                };
                let fact = self
                    .desk
                    .propose_fact(fact, &self.proposer)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("Proposed as fact {}", fact.id))
            }
            PROPOSE_POST => {
                let post = lighthouse_desk::NewPost {
                    body: text(input, "body").unwrap_or_default(),
                    title: text(input, "title"),
                    pillar: text(input, "pillar"),
                    topic: text(input, "topic"),
                    channel: text(input, "channel"),
                    reason: lighthouse_core::proposal::reason(input),
                    ..lighthouse_desk::NewPost::default()
                };
                let post = self
                    .desk
                    .propose_post(post)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("Proposed as post {}", post.id))
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

#[async_trait]
impl ToolHost for DeskHost {
    async fn list_tools(&self, tools: &[String]) -> Result<Vec<ToolSpec>, McpError> {
        let (own, other): (Vec<_>, Vec<_>) =
            tools.iter().cloned().partition(|tool| is_desk_tool(tool));
        let mut specs: Vec<ToolSpec> = own.iter().filter_map(|tool| spec(tool)).collect();
        if let Some(inner) = &self.inner {
            specs.extend(inner.list_tools(&other).await?);
        }
        Ok(specs)
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        if is_desk_tool(tool) {
            return Ok(match self.dispatch(tool, &arguments).await {
                Ok(text) => ToolOutput {
                    text,
                    is_error: false,
                },
                Err(text) => ToolOutput {
                    text,
                    is_error: true,
                },
            });
        }
        match &self.inner {
            Some(inner) => inner.call_tool(tool, arguments).await,
            None => Err(McpError::UnknownConnector(tool.to_owned())),
        }
    }
}

/// Serves `tasks.propose` and `memory.propose` and passes everything else on.
pub struct DeskToolFactory {
    desk: Desk,
    inner: Arc<dyn ToolHostFactory>,
}

impl DeskToolFactory {
    /// Wraps a tool factory.
    pub fn new(desk: Desk, inner: Arc<dyn ToolHostFactory>) -> Self {
        Self { desk, inner }
    }
}

/// Agent ID for proposals when a run cannot name its agent.
const UNKNOWN_AGENT: &str = "agent";

#[async_trait]
impl ToolHostFactory for DeskToolFactory {
    async fn open(&self, tools: &[String], run: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        let other: Vec<String> = tools
            .iter()
            .filter(|tool| !is_desk_tool(tool))
            .cloned()
            .collect();
        let inner = if other.is_empty() {
            None
        } else {
            Some(self.inner.open(&other, run).await?)
        };
        let agent_id = match self.desk.agent_of_run(run.as_uuid()).await {
            Ok(Some(agent)) => agent,
            Ok(None) => UNKNOWN_AGENT.to_owned(),
            Err(error) => {
                tracing::warn!(%error, "cannot read the agent of a run");
                UNKNOWN_AGENT.to_owned()
            }
        };
        Ok(Box::new(DeskHost {
            desk: self.desk.clone(),
            proposer: Proposer::agent(agent_id, run.as_uuid()),
            inner,
        }))
    }

    async fn offered(&self, connector: &str) -> Result<Vec<String>, McpError> {
        self.inner.offered(connector).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighthouse_audit::AuditLog;
    use lighthouse_core::TenantId;
    use lighthouse_desk::{FactFilter, TaskView};
    use lighthouse_store::testing::fresh_store;

    struct Nothing;

    #[async_trait]
    impl ToolHostFactory for Nothing {
        async fn open(&self, _: &[String], _: RunId) -> Result<Box<dyn ToolHost>, McpError> {
            Err(McpError::UnknownConnector("other".to_owned()))
        }
    }

    #[tokio::test]
    async fn agents_propose_tasks_and_facts_for_the_owner() {
        let store = fresh_store().await;
        let desk = Desk::new(store.clone(), TenantId::DEFAULT);
        let run = RunId::new();
        AuditLog::new(store, TenantId::DEFAULT)
            .start_run(run, "inbox", "manual")
            .await
            .unwrap();
        let factory = DeskToolFactory::new(desk.clone(), Arc::new(Nothing));
        let names = [
            PROPOSE_TASK.to_owned(),
            PROPOSE_FACT.to_owned(),
            PROPOSE_POST.to_owned(),
        ];
        let host = factory.open(&names, run).await.unwrap();
        let specs = host.list_tools(&names).await.unwrap();
        assert_eq!(specs.len(), 3);
        let post = host
            .call_tool(
                PROPOSE_POST,
                json!({"body": "An idea", "pillar": "finance", "_reason": "It did well last week"}),
            )
            .await
            .unwrap();
        assert!(!post.is_error, "{}", post.text);
        assert!(
            host.call_tool(PROPOSE_POST, json!({}))
                .await
                .unwrap()
                .is_error
        );
        let posts = desk.proposed_posts().await.unwrap();
        assert_eq!(posts[0].reason.as_deref(), Some("It did well last week"));

        let task = json!({"title": "Reply to the request", "due_on": "2026-05-04", "priority": 2,
                          "_reason": "Anna waits for an answer",
                          "source": {"kind": "mail", "reference": "m-1"}});
        let output = host.call_tool(PROPOSE_TASK, task).await.unwrap();
        assert!(!output.is_error, "{}", output.text);
        let fact = json!({"text": "Prefers calls to mail", "confidence": 0.8,
                          "evidence": "Asked for a call twice"});
        let output = host.call_tool(PROPOSE_FACT, fact).await.unwrap();
        assert!(!output.is_error, "{}", output.text);

        let today = "2026-05-01".parse().unwrap();
        let tasks = desk.tasks(TaskView::Proposed, today, 10).await.unwrap();
        assert_eq!(tasks[0].agent_id.as_deref(), Some("inbox"));
        assert_eq!(tasks[0].run_id, Some(run.as_uuid()));
        assert_eq!(tasks[0].notes.as_deref(), Some("Anna waits for an answer"));
        let facts = desk.facts(&FactFilter::default()).await.unwrap();
        assert_eq!(facts[0].status, "proposed");
        assert!(desk.known_facts().await.unwrap().is_empty());

        for (tool, input) in [
            (PROPOSE_TASK, json!({"title": "x", "due_on": "soon"})),
            (
                PROPOSE_TASK,
                json!({"title": "x", "source": {"kind": "mail"}}),
            ),
            (PROPOSE_TASK, json!({})),
            (PROPOSE_FACT, json!({"text": "x", "confidence": 3})),
        ] {
            assert!(host.call_tool(tool, input).await.unwrap().is_error);
        }
        assert!(host.call_tool("linear.list", json!({})).await.is_err());
        assert!(
            factory
                .open(&["linear.list".to_owned()], run)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_run_without_a_record_still_proposes() {
        let desk = Desk::new(fresh_store().await, TenantId::DEFAULT);
        let factory = DeskToolFactory::new(desk.clone(), Arc::new(Nothing));
        let host = factory
            .open(&[PROPOSE_TASK.to_owned()], RunId::new())
            .await
            .unwrap();
        let output = host
            .call_tool(PROPOSE_TASK, json!({"title": "Check"}))
            .await
            .unwrap();
        assert!(!output.is_error);
        let today = "2026-05-01".parse().unwrap();
        let tasks = desk.tasks(TaskView::Proposed, today, 10).await.unwrap();
        assert_eq!(tasks[0].agent_id.as_deref(), Some(UNKNOWN_AGENT));
    }
}
