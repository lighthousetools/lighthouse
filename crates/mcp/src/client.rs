use crate::oauth::VaultCredentialStore;
use crate::{McpError, ToolHost, ToolHostFactory, ToolOutput, split_tool};
use async_trait::async_trait;
use lighthouse_config::{ConnectorAuth, ConnectorConfig, ConnectorTransport};
use lighthouse_core::RunId;
use lighthouse_router::ToolSpec;
use lighthouse_vault::Vault;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::auth::{AuthClient, AuthorizationManager};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio::process::Command;

type Client = RunningService<RoleClient, ()>;

/// Connects to the configured MCP servers.
#[derive(Clone, Debug)]
pub struct McpConnector {
    connectors: BTreeMap<String, ConnectorConfig>,
    vault: Vault,
}

fn connect_error(connector: &str, error: impl std::fmt::Display) -> McpError {
    McpError::Connect {
        connector: connector.to_owned(),
        detail: error.to_string(),
    }
}

impl McpConnector {
    /// Creates a connector.
    pub fn new(connectors: BTreeMap<String, ConnectorConfig>, vault: Vault) -> Self {
        Self { connectors, vault }
    }

    async fn connect(&self, name: &str) -> Result<Client, McpError> {
        let connector = self
            .connectors
            .get(name)
            .ok_or_else(|| McpError::UnknownConnector(name.to_owned()))?;
        match &connector.transport {
            ConnectorTransport::Stdio {
                command,
                args,
                secret_env,
            } => {
                let mut process = Command::new(command);
                process.args(args);
                for (variable, entry) in secret_env {
                    let value =
                        self.vault
                            .get(entry)
                            .await?
                            .ok_or_else(|| McpError::NotAuthorized {
                                connector: name.to_owned(),
                                detail: format!("vault entry `{entry}` is missing"),
                            })?;
                    process.env(variable, value.expose_secret());
                }
                let transport =
                    TokioChildProcess::new(process).map_err(|error| connect_error(name, error))?;
                ().serve(transport)
                    .await
                    .map_err(|error| connect_error(name, error))
            }
            ConnectorTransport::Http {
                url,
                auth,
                allow_private_addresses,
                ..
            } => {
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                match auth {
                    ConnectorAuth::None => {
                        let transport = StreamableHttpClientTransport::from_config(config);
                        ().serve(transport)
                            .await
                            .map_err(|error| connect_error(name, error))
                    }
                    ConnectorAuth::Bearer => {
                        let entry = format!("mcp:{name}");
                        let token = self.vault.get(&entry).await?.ok_or_else(|| {
                            McpError::NotAuthorized {
                                connector: name.to_owned(),
                                detail: format!("vault entry `{entry}` is missing"),
                            }
                        })?;
                        let config = config.auth_header(token.expose_secret().to_owned());
                        let transport = StreamableHttpClientTransport::from_config(config);
                        ().serve(transport)
                            .await
                            .map_err(|error| connect_error(name, error))
                    }
                    ConnectorAuth::Oauth => {
                        let mut manager = if *allow_private_addresses {
                            AuthorizationManager::new_allowing_private_addresses(url.as_str()).await
                        } else {
                            AuthorizationManager::new(url.as_str()).await
                        }
                        .map_err(|error| connect_error(name, error))?;
                        manager.set_credential_store(VaultCredentialStore::new(
                            self.vault.clone(),
                            name,
                        ));
                        let stored = manager
                            .initialize_from_store()
                            .await
                            .map_err(|error| connect_error(name, error))?;
                        if !stored {
                            return Err(McpError::NotAuthorized {
                                connector: name.to_owned(),
                                detail: format!("run `lht connect {name}` first"),
                            });
                        }
                        let client = AuthClient::new(reqwest::Client::default(), manager);
                        let transport = StreamableHttpClientTransport::with_client(client, config);
                        ().serve(transport)
                            .await
                            .map_err(|error| connect_error(name, error))
                    }
                }
            }
        }
    }

    /// Opens a session with the connectors of the given qualified tools.
    pub async fn session(&self, tools: &[String]) -> Result<McpSession, McpError> {
        let mut names = BTreeSet::new();
        for tool in tools {
            names.insert(split_tool(tool)?.0);
        }
        let mut clients = HashMap::new();
        for name in names {
            clients.insert(name.to_owned(), self.connect(name).await?);
        }
        Ok(McpSession { clients })
    }
}

#[async_trait]
impl ToolHostFactory for McpConnector {
    async fn open(&self, tools: &[String], _run: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        Ok(Box::new(self.session(tools).await?))
    }
}

/// Open connections for one run.
#[derive(Debug)]
pub struct McpSession {
    clients: HashMap<String, Client>,
}

impl McpSession {
    /// Lists every tool a connector offers, with unqualified names.
    pub async fn all_tools(&self, connector: &str) -> Result<Vec<ToolSpec>, McpError> {
        let tools = self
            .client(connector)?
            .list_all_tools()
            .await
            .map_err(|error| McpError::Call {
                connector: connector.to_owned(),
                detail: error.to_string(),
            })?;
        Ok(tools
            .into_iter()
            .map(|tool| ToolSpec {
                name: tool.name.to_string(),
                description: tool.description.as_deref().unwrap_or_default().to_owned(),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }

    fn client(&self, connector: &str) -> Result<&Client, McpError> {
        self.clients
            .get(connector)
            .ok_or_else(|| McpError::UnknownConnector(connector.to_owned()))
    }
}

#[async_trait]
impl ToolHost for McpSession {
    async fn list_tools(&self, tools: &[String]) -> Result<Vec<ToolSpec>, McpError> {
        let mut specs = Vec::new();
        let mut available: HashMap<&str, Vec<rmcp::model::Tool>> = HashMap::new();
        for qualified in tools {
            let (connector, tool) = split_tool(qualified)?;
            if !available.contains_key(connector) {
                let listed = self
                    .client(connector)?
                    .list_all_tools()
                    .await
                    .map_err(|error| McpError::Call {
                        connector: connector.to_owned(),
                        detail: error.to_string(),
                    })?;
                available.insert(connector, listed);
            }
            if let Some(found) = available[connector]
                .iter()
                .find(|candidate| candidate.name == tool)
            {
                specs.push(ToolSpec {
                    name: qualified.clone(),
                    description: found.description.as_deref().unwrap_or_default().to_owned(),
                    input_schema: Value::Object((*found.input_schema).clone()),
                });
            }
        }
        Ok(specs)
    }

    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<ToolOutput, McpError> {
        let (connector, name) = split_tool(tool)?;
        let mut request = CallToolRequestParams::new(name.to_owned());
        if let Value::Object(arguments) = arguments {
            request = request.with_arguments(arguments);
        }
        let result = self
            .client(connector)?
            .call_tool(request)
            .await
            .map_err(|error| McpError::Call {
                connector: connector.to_owned(),
                detail: error.to_string(),
            })?;
        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|content| content.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            text,
            is_error: result.is_error.unwrap_or(false),
        })
    }
}
