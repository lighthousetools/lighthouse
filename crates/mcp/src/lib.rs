//! MCP client with OAuth and tool classification.

mod client;
pub mod demo;
pub mod oauth;
pub mod openapi;
mod policy;

pub use client::{Connection, ConnectorSource, FixedConnectors, McpConnector, McpSession};
pub use oauth::OAuthFlows;
pub use policy::{BlockedTool, ToolPolicy};

use async_trait::async_trait;
use lighthouse_core::RunId;
use lighthouse_router::ToolSpec;
use serde_json::Value;

/// Errors from MCP connections and tool calls.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The connector is not configured.
    #[error("unknown connector `{0}`")]
    UnknownConnector(String),
    /// The tool name is not `connector.tool`.
    #[error("`{0}` is not a qualified tool name")]
    BadToolName(String),
    /// A credential is missing.
    #[error("connector `{connector}` is not authorized: {detail}")]
    NotAuthorized {
        /// Connector name.
        connector: String,
        /// What is missing.
        detail: String,
    },
    /// Connecting failed.
    #[error("cannot connect to `{connector}`: {detail}")]
    Connect {
        /// Connector name.
        connector: String,
        /// What went wrong.
        detail: String,
    },
    /// A protocol call failed.
    #[error("call to `{connector}` failed: {detail}")]
    Call {
        /// Connector name.
        connector: String,
        /// What went wrong.
        detail: String,
    },
    /// The OAuth flow failed.
    #[error("OAuth flow failed: {0}")]
    OAuth(String),
    /// The vault failed.
    #[error(transparent)]
    Vault(#[from] lighthouse_vault::VaultError),
}

/// The result of a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text content.
    pub text: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// Lists and calls tools for one run.
#[async_trait]
pub trait ToolHost: Send + Sync {
    /// Returns the specs of the given qualified tools, with qualified names.
    async fn list_tools(&self, tools: &[String]) -> Result<Vec<ToolSpec>, McpError>;

    /// Calls a qualified tool.
    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError>;
}

/// Opens tool hosts for runs.
#[async_trait]
pub trait ToolHostFactory: Send + Sync {
    /// Connects to everything the given qualified tools need for a run.
    async fn open(&self, tools: &[String], run: RunId) -> Result<Box<dyn ToolHost>, McpError>;

    /// Lists the tools a connector offers, with unqualified names.
    async fn offered(&self, connector: &str) -> Result<Vec<String>, McpError> {
        Err(McpError::UnknownConnector(connector.to_owned()))
    }
}

pub(crate) fn split_tool(qualified: &str) -> Result<(&str, &str), McpError> {
    qualified
        .split_once('.')
        .ok_or_else(|| McpError::BadToolName(qualified.to_owned()))
}
