//! A small MCP server with sample data, for demos and tests.

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use serde_json::json;
use std::sync::Arc;

/// Serves `list_issues` (read), `echo` (read) and `delete_issue` (write).
#[derive(Clone, Copy, Debug, Default)]
pub struct DemoServer;

fn tool(name: &'static str, description: &'static str) -> Tool {
    let schema = json!({"type": "object", "properties": {}});
    let schema = schema.as_object().cloned().unwrap_or_default();
    Tool::new(name, description, Arc::new(schema))
}

impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            tool("list_issues", "Lists open issues."),
            tool("echo", "Returns its arguments."),
            tool("delete_issue", "Deletes an issue."),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let text = |value: String| vec![ContentBlock::text(value)];
        let result = match request.name.as_ref() {
            "list_issues" => CallToolResult::success(text(
                json!([
                    {"id": "DEMO-1", "title": "Fix the login timeout", "priority": "high"},
                    {"id": "DEMO-2", "title": "Update the release notes", "priority": "low"},
                ])
                .to_string(),
            )),
            "echo" => CallToolResult::success(text(json!(request.arguments).to_string())),
            "delete_issue" => CallToolResult::error(text("demo data cannot be deleted".to_owned())),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                ));
            }
        };
        Ok(result.into())
    }
}
