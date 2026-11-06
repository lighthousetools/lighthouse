//! Built-in `brain.*` tools for agents.

use crate::{Brain, NewEntry, Source};
use async_trait::async_trait;
use lighthouse_core::RunId;
use lighthouse_mcp::{McpError, ToolHost, ToolHostFactory, ToolOutput};
use lighthouse_router::ToolSpec;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

const CONNECTOR: &str = "brain";

const SEARCH_LIMIT: usize = 10;
/// Agents may store these kinds. Deletion is reserved for people.
const AGENT_KINDS: [&str; 3] = ["note", "decision", "meeting"];

fn spec(tool: &str) -> Option<ToolSpec> {
    let (description, input_schema) = match tool {
        "search" => (
            "Searches the owner's Second Brain.",
            json!({"type": "object", "properties": {"query": {"type": "string"}},
                   "required": ["query"]}),
        ),
        "get" => (
            "Reads one entry of the Second Brain by ID.",
            json!({"type": "object", "properties": {"id": {"type": "string"}},
                   "required": ["id"]}),
        ),
        "remember" => (
            "Stores a note, decision or meeting summary in the Second Brain.",
            json!({"type": "object", "properties": {
                "type": {"type": "string", "enum": AGENT_KINDS},
                "title": {"type": "string"},
                "body": {"type": "string"}
            }, "required": ["type", "title", "body"]}),
        ),
        "link" => (
            "Links an entry to a person, project or customer, such as project:release.",
            json!({"type": "object", "properties": {
                "id": {"type": "string"},
                "link": {"type": "string"}
            }, "required": ["id", "link"]}),
        ),
        _ => return None,
    };
    Some(ToolSpec {
        name: format!("{CONNECTOR}.{tool}"),
        description: description.to_owned(),
        input_schema,
    })
}

fn text_field<'a>(input: &'a Value, name: &str) -> Result<&'a str, String> {
    input[name]
        .as_str()
        .ok_or_else(|| format!("`{name}` is missing"))
}

fn id_field(input: &Value) -> Result<Uuid, String> {
    Uuid::parse_str(text_field(input, "id")?).map_err(|_| "`id` is not a valid ID".to_owned())
}

struct BrainHost {
    brain: Brain,
    inner: Option<Box<dyn ToolHost>>,
    run: String,
}

impl BrainHost {
    async fn dispatch(&self, tool: &str, input: &Value) -> Result<String, String> {
        let failed = |error: crate::BrainError| error.to_string();
        match tool {
            "search" => {
                let query = text_field(input, "query")?;
                let hits = self
                    .brain
                    .search(query, SEARCH_LIMIT)
                    .await
                    .map_err(failed)?;
                serde_json::to_string(&hits).map_err(|error| error.to_string())
            }
            "get" => {
                let id = id_field(input)?;
                match self.brain.get(id).await.map_err(failed)? {
                    Some(entry) => Ok(format!("# {}\n\n{}", entry.title, entry.body)),
                    None => Err("no such entry".to_owned()),
                }
            }
            "remember" => {
                let kind = text_field(input, "type")?;
                if !AGENT_KINDS.contains(&kind) {
                    return Err(format!("agents cannot store `{kind}` entries"));
                }
                let entry = self
                    .brain
                    .remember(NewEntry {
                        kind: kind.to_owned(),
                        title: text_field(input, "title")?.to_owned(),
                        body: text_field(input, "body")?.to_owned(),
                        sources: vec![Source {
                            kind: "agent".to_owned(),
                            reference: self.run.clone(),
                        }],
                        links: Vec::new(),
                    })
                    .await
                    .map_err(failed)?;
                Ok(format!("Stored as {}", entry.id))
            }
            "link" => {
                let id = id_field(input)?;
                let link = text_field(input, "link")?;
                match self.brain.link(id, link).await.map_err(failed)? {
                    Some(_) => Ok("Linked".to_owned()),
                    None => Err("no such entry".to_owned()),
                }
            }
            other => Err(format!("unknown tool `{other}`")),
        }
    }
}

#[async_trait]
impl ToolHost for BrainHost {
    async fn list_tools(&self, tools: &[String]) -> Result<Vec<ToolSpec>, McpError> {
        let prefix = format!("{CONNECTOR}.");
        let (own, other): (Vec<_>, Vec<_>) = tools
            .iter()
            .cloned()
            .partition(|tool| tool.starts_with(&prefix));
        let mut specs: Vec<ToolSpec> = own
            .iter()
            .filter_map(|tool| spec(tool.strip_prefix(&prefix)?))
            .collect();
        if let Some(inner) = &self.inner {
            specs.extend(inner.list_tools(&other).await?);
        }
        Ok(specs)
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        if let Some(name) = tool.strip_prefix("brain.") {
            return Ok(match self.dispatch(name, &arguments).await {
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

/// Serves `brain.*` tools and passes everything else on.
pub struct BrainToolFactory {
    brain: Brain,
    inner: Arc<dyn ToolHostFactory>,
}

impl BrainToolFactory {
    /// Wraps a tool factory.
    pub fn new(brain: Brain, inner: Arc<dyn ToolHostFactory>) -> Self {
        Self { brain, inner }
    }
}

#[async_trait]
impl ToolHostFactory for BrainToolFactory {
    async fn open(&self, tools: &[String], run: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        let other: Vec<String> = tools
            .iter()
            .filter(|tool| !tool.starts_with("brain."))
            .cloned()
            .collect();
        let inner = if other.is_empty() {
            None
        } else {
            Some(self.inner.open(&other, run).await?)
        };
        Ok(Box::new(BrainHost {
            brain: self.brain.clone(),
            inner,
            run: run.as_uuid().to_string(),
        }))
    }
}
