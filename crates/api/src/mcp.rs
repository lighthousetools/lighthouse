//! Lighthouse as an MCP server.
//!
//! `/mcp` speaks the streamable HTTP transport of MCP, so Claude Code and other
//! clients can use the Brain, tasks, facts, rules, the calendar and agents of
//! the signed-in person. Every tool is a request to the API itself with the
//! caller's own `Authorization` header: the same sign-in, tenant, roles and
//! approvals apply as in the apps, and nothing here can do more than the token.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, header, request::Parts};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tower::ServiceExt;

use crate::auth::TENANT_HEADER;

/// The longest tool result in characters. Longer answers are cut.
const MAX_RESULT_CHARS: usize = 60_000;
/// The largest API answer read for one tool call.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// A request to the API.
#[derive(Debug, PartialEq)]
struct Call {
    method: Method,
    path: String,
    body: Option<Value>,
}

/// One tool: its name, what it does, its input schema and the request it makes.
struct Spec {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    call: fn(&Map<String, Value>) -> Result<Call, String>,
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

fn none() -> Value {
    object(json!({}), &[])
}

fn text(arguments: &Map<String, Value>, name: &str) -> Result<String, String> {
    match arguments.get(name) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        _ => Err(format!("`{name}` is required")),
    }
}

/// Characters that stay as they are in a path segment or query value.
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if unreserved(byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A path with the given optional query arguments that are set.
fn query(path: &str, arguments: &Map<String, Value>, names: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = names
        .iter()
        .filter_map(|(argument, parameter)| {
            let value = match arguments.get(*argument)? {
                Value::String(value) if !value.is_empty() => value.clone(),
                Value::Number(value) => value.to_string(),
                Value::Bool(value) => value.to_string(),
                _ => return None,
            };
            Some(format!("{parameter}={}", encode(&value)))
        })
        .collect();
    if pairs.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", pairs.join("&"))
    }
}

fn get(path: String) -> Result<Call, String> {
    Ok(Call {
        method: Method::GET,
        path,
        body: None,
    })
}

fn post(path: String, body: Value) -> Result<Call, String> {
    Ok(Call {
        method: Method::POST,
        path,
        body: Some(body),
    })
}

fn id(arguments: &Map<String, Value>) -> Result<String, String> {
    text(arguments, "id").map(|value| encode(&value))
}

const SPECS: &[Spec] = &[
    Spec {
        name: "search_brain",
        description: "Searches the Second Brain: notes, decisions, projects, customers, people and meetings. Returns matching entries with their IDs.",
        schema: || {
            object(
                json!({"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 50}}),
                &["query"],
            )
        },
        call: |arguments| {
            text(arguments, "query")?;
            get(query(
                "/brain/search",
                arguments,
                &[("query", "q"), ("limit", "limit")],
            ))
        },
    },
    Spec {
        name: "get_brain_entry",
        description: "Reads one Brain entry with its full text and links.",
        schema: || object(json!({"id": {"type": "string"}}), &["id"]),
        call: |arguments| get(format!("/brain/entries/{}", id(arguments)?)),
    },
    Spec {
        name: "ask_brain",
        description: "Answers a question from the Second Brain, with the entries the answer is based on.",
        schema: || object(json!({"question": {"type": "string"}}), &["question"]),
        call: |arguments| {
            post(
                "/brain/ask".to_owned(),
                json!({"question": text(arguments, "question")?}),
            )
        },
    },
    Spec {
        name: "save_to_brain",
        description: "Stores knowledge in the Second Brain so later sessions and agents find it: a decision, the state of a project, notes of a meeting. Write it so it stands on its own.",
        schema: || {
            object(
                json!({
                    "title": {"type": "string"},
                    "body": {"type": "string", "description": "Markdown"},
                    "type": {"type": "string", "enum": ["note", "decision", "meeting", "project", "customer", "person"]},
                    "links": {"type": "array", "items": {"type": "string"}, "description": "Titles of related entries"},
                    "source": {"type": "string", "description": "Where it comes from, such as a repository or a session"}
                }),
                &["title", "body"],
            )
        },
        call: |arguments| {
            let kind = arguments
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("note");
            let source = arguments
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("mcp");
            post(
                "/brain/entries".to_owned(),
                json!({
                    "type": kind,
                    "title": text(arguments, "title")?,
                    "body": text(arguments, "body")?,
                    "links": arguments.get("links").cloned().unwrap_or_else(|| json!([])),
                    "sources": [{"kind": "mcp", "ref": source}],
                }),
            )
        },
    },
    Spec {
        name: "list_facts",
        description: "Lists the confirmed facts about the owner: role, preferences, habits.",
        schema: none,
        call: |_| get("/memory?status=confirmed".to_owned()),
    },
    Spec {
        name: "remember_fact",
        description: "Stores a fact about the owner that every agent will see. Only for something the owner said about themselves in this conversation.",
        schema: || {
            object(
                json!({"text": {"type": "string"}, "category": {"type": "string"}}),
                &["text"],
            )
        },
        call: |arguments| {
            let mut body = json!({"text": text(arguments, "text")?});
            if let Some(category) = arguments.get("category").and_then(Value::as_str) {
                body["category"] = json!(category);
            }
            post("/memory".to_owned(), body)
        },
    },
    Spec {
        name: "list_rules",
        description: "Lists the owner's standing rules, which every agent follows.",
        schema: none,
        call: |_| get("/rules".to_owned()),
    },
    Spec {
        name: "list_tasks",
        description: "Lists the owner's tasks.",
        schema: || {
            object(
                json!({"view": {"type": "string", "enum": ["all", "today", "upcoming", "done", "proposed"]}, "limit": {"type": "integer"}}),
                &[],
            )
        },
        call: |arguments| {
            get(query(
                "/tasks",
                arguments,
                &[("view", "view"), ("limit", "limit")],
            ))
        },
    },
    Spec {
        name: "create_task",
        description: "Adds a task to the owner's list.",
        schema: || {
            object(
                json!({
                    "title": {"type": "string"},
                    "notes": {"type": "string"},
                    "due_on": {"type": "string", "description": "YYYY-MM-DD"},
                    "priority": {"type": "integer", "minimum": 0, "maximum": 3}
                }),
                &["title"],
            )
        },
        call: |arguments| {
            let mut body = json!({"title": text(arguments, "title")?});
            for name in ["notes", "due_on", "priority"] {
                if let Some(value) = arguments.get(name) {
                    body[name] = value.clone();
                }
            }
            post("/tasks".to_owned(), body)
        },
    },
    Spec {
        name: "complete_task",
        description: "Marks a task as done.",
        schema: || object(json!({"id": {"type": "string"}}), &["id"]),
        call: |arguments| post(format!("/tasks/{}/complete", id(arguments)?), json!({})),
    },
    Spec {
        name: "whats_ready",
        description: "What waits for the owner: decisions, approvals and drafts, most important first.",
        schema: none,
        call: |_| get("/ready".to_owned()),
    },
    Spec {
        name: "list_inbox",
        description: "Open items from the connected services that need the owner: important mail, mentions, review requests, assigned issues.",
        schema: || {
            object(
                json!({"query": {"type": "string"}, "limit": {"type": "integer"}}),
                &[],
            )
        },
        call: |arguments| {
            get(query(
                "/inbox",
                arguments,
                &[("query", "q"), ("limit", "limit")],
            ))
        },
    },
    Spec {
        name: "list_calendar",
        description: "Calendar events from the connected calendars, earliest first.",
        schema: || {
            object(
                json!({"from": {"type": "string", "description": "RFC 3339"}, "to": {"type": "string", "description": "RFC 3339"}}),
                &[],
            )
        },
        call: |arguments| {
            get(query(
                "/calendar/events",
                arguments,
                &[("from", "from"), ("to", "to")],
            ))
        },
    },
    Spec {
        name: "list_issues",
        description: "Issues from the connected trackers, open ones by default.",
        schema: || {
            object(
                json!({"query": {"type": "string"}, "limit": {"type": "integer"}}),
                &[],
            )
        },
        call: |arguments| {
            get(query(
                "/issues",
                arguments,
                &[("query", "q"), ("limit", "limit")],
            ))
        },
    },
    Spec {
        name: "list_agents",
        description: "Lists the agents with their schedules and tools.",
        schema: none,
        call: |_| get("/agents".to_owned()),
    },
    Spec {
        name: "run_agent",
        description: "Starts an agent now. Writing steps still wait for the owner's approval.",
        schema: || object(json!({"id": {"type": "string"}}), &["id"]),
        call: |arguments| post(format!("/agents/{}/run", id(arguments)?), json!({})),
    },
    Spec {
        name: "get_run",
        description: "Reads a run of an agent or automation: status, steps, costs and report.",
        schema: || object(json!({"id": {"type": "string"}}), &["id"]),
        call: |arguments| get(format!("/runs/{}", id(arguments)?)),
    },
    Spec {
        name: "report_event",
        description: "Reports an event to Lighthouse, such as `ci.pushed` after a push, so matching automations start.",
        schema: || {
            object(
                json!({"name": {"type": "string"}, "payload": {"type": "object"}}),
                &["name"],
            )
        },
        call: |arguments| {
            post(
                "/automations/events".to_owned(),
                json!({"name": text(arguments, "name")?, "payload": arguments.get("payload").cloned().unwrap_or_else(|| json!({}))}),
            )
        },
    },
];

/// The MCP server. It holds the API router it sends every tool call to.
#[derive(Clone)]
pub struct LighthouseMcp {
    api: Router,
}

impl LighthouseMcp {
    async fn forward(&self, call: Call, headers: &HeaderMap) -> CallToolResult {
        let mut request = Request::builder().method(call.method).uri(&call.path);
        for name in [header::AUTHORIZATION.as_str(), TENANT_HEADER] {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        let body = match call.body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let reply = match request.body(body) {
            Ok(request) => self.api.clone().oneshot(request).await,
            Err(error) => return failure(format!("invalid request: {error}")),
        };
        let Ok(reply) = reply;
        let status = reply.status();
        let text = match to_bytes(reply.into_body(), MAX_BODY_BYTES).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => return failure("the answer was too large".to_owned()),
        };
        let text = cut(text);
        if status.is_success() {
            CallToolResult::success(vec![ContentBlock::text(text)])
        } else {
            failure(format!("{status}: {text}"))
        }
    }
}

fn failure(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn cut(text: String) -> String {
    if text.chars().count() <= MAX_RESULT_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(MAX_RESULT_CHARS).collect();
    short.push_str("\n[cut]");
    short
}

fn tool(spec: &Spec) -> Tool {
    let Value::Object(schema) = (spec.schema)() else {
        unreachable!("schemas are objects")
    };
    Tool::new(spec.name, spec.description, Arc::new(schema))
}

impl ServerHandler for LighthouseMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("lighthouse", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Lighthouse holds the owner's Second Brain, tasks, facts, rules, connected services and agents. \
                 Search the Brain before you ask the owner something it may know, and save decisions and the state \
                 of work with save_to_brain so later sessions can go on.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            SPECS.iter().map(tool).collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let Some(spec) = SPECS.iter().find(|spec| spec.name == name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {name}"),
                None,
            ));
        };
        let arguments = request.arguments.clone().unwrap_or_default();
        let call = match (spec.call)(&arguments) {
            Ok(call) => call,
            Err(message) => return Ok(failure(message).into()),
        };
        let headers = context
            .extensions
            .get::<Parts>()
            .map(|parts| parts.headers.clone())
            .unwrap_or_default();
        Ok(self.forward(call, &headers).await.into())
    }
}

/// The `/mcp` service for an API router. It answers with JSON and keeps no
/// sessions, so it works behind any proxy and on more than one replica.
pub fn service(api: Router) -> StreamableHttpService<LighthouseMcp, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_legacy_session_mode(false)
        .disable_allowed_hosts();
    StreamableHttpService::new(
        move || Ok(LighthouseMcp { api: api.clone() }),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Names of all tools.
#[cfg(test)]
fn tool_names() -> Vec<&'static str> {
    SPECS.iter().map(|spec| spec.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn spec(name: &str) -> &'static Spec {
        SPECS.iter().find(|spec| spec.name == name).unwrap()
    }

    #[test]
    fn names_are_unique_and_schemas_are_objects() {
        let mut names = tool_names();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SPECS.len());
        for spec in SPECS {
            assert_eq!((spec.schema)()["type"], "object", "{}", spec.name);
        }
    }

    #[test]
    fn search_encodes_the_query() {
        let call =
            (spec("search_brain").call)(&arguments(json!({"query": "a&b ü", "limit": 5}))).unwrap();
        assert_eq!(call.path, "/brain/search?q=a%26b%20%C3%BC&limit=5");
        assert_eq!(call.method, Method::GET);
        assert!((spec("search_brain").call)(&arguments(json!({}))).is_err());
    }

    #[test]
    fn ids_cannot_leave_their_segment() {
        let call = (spec("get_run").call)(&arguments(json!({"id": "../tasks"}))).unwrap();
        assert_eq!(call.path, "/runs/..%2Ftasks");
    }

    #[test]
    fn brain_entries_carry_a_source() {
        let call = (spec("save_to_brain").call)(&arguments(
            json!({"title": "T", "body": "B", "source": "repo x"}),
        ))
        .unwrap();
        let body = call.body.unwrap();
        assert_eq!(body["type"], "note");
        assert_eq!(body["sources"][0]["ref"], "repo x");
        assert!((spec("save_to_brain").call)(&arguments(json!({"title": "T"}))).is_err());
    }

    #[test]
    fn tasks_take_only_known_fields() {
        let call = (spec("create_task").call)(&arguments(
            json!({"title": "T", "priority": 2, "other": 1}),
        ))
        .unwrap();
        assert_eq!(call.body.unwrap(), json!({"title": "T", "priority": 2}));
        let list = (spec("list_tasks").call)(&arguments(json!({"view": "today"}))).unwrap();
        assert_eq!(list.path, "/tasks?view=today");
        assert_eq!(
            (spec("list_tasks").call)(&arguments(json!({})))
                .unwrap()
                .path,
            "/tasks"
        );
    }

    #[test]
    fn long_results_are_cut() {
        let long = "x".repeat(MAX_RESULT_CHARS + 10);
        assert!(cut(long).ends_with("[cut]"));
        assert_eq!(cut("short".to_owned()), "short");
    }
}
