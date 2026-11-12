use crate::oauth::VaultCredentialStore;
use crate::openapi::{self, ApiKey, OpenApiServer};
use crate::{McpError, ToolHost, ToolHostFactory, ToolOutput, split_tool};
use async_trait::async_trait;
use lighthouse_config::{ApiKeyPlacement, ConnectorAuth, ConnectorConfig, ConnectorTransport};
use lighthouse_core::RunId;
use lighthouse_router::ToolSpec;
use lighthouse_vault::Vault;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthClientConfig};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use tokio::process::Command;

type Client = RunningService<RoleClient, ()>;

/// A connector and the vaults its tokens come from.
#[derive(Clone, Debug)]
pub struct Connection {
    /// Its settings.
    pub config: ConnectorConfig,
    /// Vaults to read tokens from, in order: the first that has one wins.
    /// OAuth tokens refresh into the vault they came from.
    pub vaults: Vec<Vault>,
    /// Whether connections must go through the guarded HTTP client, because
    /// a tenant or a person chose the URL.
    pub guarded: bool,
}

/// The connectors that apply now.
pub trait ConnectorSource: Send + Sync {
    /// The connectors for the work the current task does. Implementations
    /// may answer per user; each call returns a consistent set.
    fn connections(&self) -> Arc<BTreeMap<String, Connection>>;
}

/// The same connectors every time.
#[derive(Clone, Debug)]
pub struct FixedConnectors(Arc<BTreeMap<String, Connection>>);

impl FixedConnectors {
    /// Connectors of the configuration, with tokens from one vault.
    pub fn new(connectors: BTreeMap<String, ConnectorConfig>, vault: Option<Vault>) -> Self {
        Self(Arc::new(
            connectors
                .into_iter()
                .map(|(name, config)| {
                    let connection = Connection {
                        config,
                        vaults: vault.iter().cloned().collect(),
                        guarded: false,
                    };
                    (name, connection)
                })
                .collect(),
        ))
    }
}

impl ConnectorSource for FixedConnectors {
    fn connections(&self) -> Arc<BTreeMap<String, Connection>> {
        self.0.clone()
    }
}

/// Connects to MCP servers.
#[derive(Clone)]
pub struct McpConnector {
    source: Arc<dyn ConnectorSource>,
}

impl std::fmt::Debug for McpConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnector").finish_non_exhaustive()
    }
}

async fn first_with(
    vaults: &[Vault],
    entry: &str,
) -> Result<Option<(Vault, secrecy::SecretString)>, McpError> {
    for vault in vaults {
        if let Some(value) = vault.get(entry).await? {
            return Ok(Some((vault.clone(), value)));
        }
    }
    Ok(None)
}

/// Loads the description of an OpenAPI connector and builds its bridge.
#[allow(clippy::too_many_arguments)]
async fn openapi_server(
    name: &str,
    connection: &Connection,
    spec: &str,
    base_url: Option<&str>,
    key: ApiKeyPlacement,
    key_name: Option<&str>,
    headers: &std::collections::BTreeMap<String, String>,
    operations: &[String],
    oauth: Option<&lighthouse_config::OpenApiOAuth>,
) -> Result<OpenApiServer, McpError> {
    let remote = spec.starts_with("https://") || spec.starts_with("http://");
    let bundled = openapi::bundled(spec);
    if connection.guarded {
        if !remote && bundled.is_none() {
            return Err(connect_error(
                name,
                "a description chosen in the apps must be a URL",
            ));
        }
        lighthouse_router::net::check_url(spec).map_err(|error| connect_error(name, error))?;
        if let Some(base) = base_url {
            lighthouse_router::net::check_url(base).map_err(|error| connect_error(name, error))?;
        }
    }
    let http = http_client(connection.guarded);
    let text = if let Some(text) = bundled {
        text.to_owned()
    } else if remote {
        http.get(spec)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| connect_error(name, error.without_url()))?
            .text()
            .await
            .map_err(|error| connect_error(name, error))?
    } else {
        tokio::fs::read_to_string(spec)
            .await
            .map_err(|error| connect_error(name, error))?
    };
    let document = openapi::parse_spec(&text).map_err(|error| connect_error(name, error))?;
    let found =
        openapi::operations(&document, operations).map_err(|error| connect_error(name, error))?;
    let base =
        openapi::base_url(&document, spec, base_url).map_err(|error| connect_error(name, error))?;
    if connection.guarded {
        lighthouse_router::net::check_url(&base).map_err(|error| connect_error(name, error))?;
    }
    let api_key = if let Some(oauth) = oauth {
        let now = chrono::Utc::now().timestamp();
        ApiKey::Bearer(
            crate::rest_oauth::access_token(
                &http,
                oauth,
                name,
                &connection.vaults,
                connection.config.app_secret.as_ref(),
                now,
            )
            .await?,
        )
    } else if key == ApiKeyPlacement::None {
        ApiKey::None
    } else {
        let entry = format!("mcp:{name}");
        let (_, secret) = first_with(&connection.vaults, &entry)
            .await?
            .ok_or_else(|| McpError::NotAuthorized {
                connector: name.to_owned(),
                detail: format!("vault entry `{entry}` is missing"),
            })?;
        let named = || {
            key_name
                .map(str::to_owned)
                .ok_or_else(|| connect_error(name, "key_name is required for this key placement"))
        };
        match key {
            ApiKeyPlacement::Bearer => ApiKey::Bearer(secret),
            ApiKeyPlacement::Header => ApiKey::Header(named()?, secret),
            ApiKeyPlacement::Query => ApiKey::Query(named()?, secret),
            ApiKeyPlacement::None => ApiKey::None,
        }
    };
    let headers = header_map(name, headers)?.into_iter().collect();
    Ok(OpenApiServer::new(found, base, api_key, headers, http))
}

/// Redirect URI given to the OAuth client when it only refreshes tokens.
const REFRESH_REDIRECT: &str = "http://localhost/oauth/callback";

/// Headers a connector sends with every request. Names and values must be valid HTTP.
fn header_map(
    connector: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<
    std::collections::HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    McpError,
> {
    headers
        .iter()
        .map(|(name, value)| {
            let header = reqwest::header::HeaderName::try_from(name.as_str())
                .map_err(|_| connect_error(connector, format!("invalid header name `{name}`")))?;
            let value = reqwest::header::HeaderValue::try_from(value.as_str()).map_err(|_| {
                connect_error(connector, format!("invalid value for header `{name}`"))
            })?;
            Ok((header, value))
        })
        .collect()
}

fn http_client(guarded: bool) -> reqwest::Client {
    if guarded {
        lighthouse_router::net::guarded_client()
    } else {
        reqwest::Client::default()
    }
}

fn connect_error(connector: &str, error: impl std::fmt::Display) -> McpError {
    McpError::Connect {
        connector: connector.to_owned(),
        detail: error.to_string(),
    }
}

impl McpConnector {
    /// Creates a connector for configured servers with tokens from one vault.
    pub fn new(connectors: BTreeMap<String, ConnectorConfig>, vault: Vault) -> Self {
        Self::live(Arc::new(FixedConnectors::new(connectors, Some(vault))))
    }

    /// Creates a connector that asks a source for the connectors on every session.
    pub fn live(source: Arc<dyn ConnectorSource>) -> Self {
        Self { source }
    }

    async fn connect(
        &self,
        connections: &BTreeMap<String, Connection>,
        name: &str,
    ) -> Result<Client, McpError> {
        let connection = connections
            .get(name)
            .ok_or_else(|| McpError::UnknownConnector(name.to_owned()))?;
        let missing = |entry: &str| McpError::NotAuthorized {
            connector: name.to_owned(),
            detail: format!("vault entry `{entry}` is missing"),
        };
        match &connection.config.transport {
            ConnectorTransport::Stdio {
                command,
                args,
                secret_env,
            } => {
                let mut process = Command::new(command);
                process.args(args);
                for (variable, entry) in secret_env {
                    let (_, value) = first_with(&connection.vaults, entry)
                        .await?
                        .ok_or_else(|| missing(entry))?;
                    process.env(variable, value.expose_secret());
                }
                let transport =
                    TokioChildProcess::new(process).map_err(|error| connect_error(name, error))?;
                ().serve(transport)
                    .await
                    .map_err(|error| connect_error(name, error))
            }
            ConnectorTransport::OpenApi {
                spec,
                base_url,
                key,
                key_name,
                headers,
                operations,
                oauth,
                ..
            } => {
                let server = openapi_server(
                    name,
                    connection,
                    spec,
                    base_url.as_deref(),
                    *key,
                    key_name.as_deref(),
                    headers,
                    operations,
                    oauth.as_ref(),
                )
                .await?;
                let (client_side, server_side) = tokio::io::duplex(1 << 20);
                tokio::spawn(async move {
                    if let Ok(running) = server.serve(server_side).await {
                        let _ = running.waiting().await;
                    }
                });
                ().serve(client_side)
                    .await
                    .map_err(|error| connect_error(name, error))
            }
            ConnectorTransport::Http {
                url,
                auth,
                allow_private_addresses,
                client_id,
                headers,
                ..
            } => {
                if connection.guarded {
                    lighthouse_router::net::check_url(url)
                        .map_err(|error| connect_error(name, error))?;
                }
                let config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(header_map(name, headers)?);
                let client = http_client(connection.guarded);
                match auth {
                    ConnectorAuth::None => {
                        let transport = StreamableHttpClientTransport::with_client(client, config);
                        ().serve(transport)
                            .await
                            .map_err(|error| connect_error(name, error))
                    }
                    ConnectorAuth::Bearer => {
                        let entry = format!("mcp:{name}");
                        let (_, token) = first_with(&connection.vaults, &entry)
                            .await?
                            .ok_or_else(|| missing(&entry))?;
                        let config = config.auth_header(token.expose_secret().to_owned());
                        let transport = StreamableHttpClientTransport::with_client(client, config);
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
                        let entry = format!("oauth:{name}");
                        let vault = match first_with(&connection.vaults, &entry).await? {
                            Some((vault, _)) => vault,
                            None => {
                                return Err(McpError::NotAuthorized {
                                    connector: name.to_owned(),
                                    detail: format!("run `lht connect {name}` first"),
                                });
                            }
                        };
                        manager.set_credential_store(VaultCredentialStore::new(vault, name));
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
                        let secret_entry = crate::oauth::client_secret_entry(name);
                        let secret = match &connection.config.app_secret {
                            Some(secret) => Some(secret.clone()),
                            None => first_with(&connection.vaults, &secret_entry)
                                .await?
                                .map(|(_, secret)| secret),
                        };
                        if let (Some(client_id), Some(secret)) = (client_id, secret) {
                            // Refreshing needs the secret too. The redirect URI is only used
                            // by the authorization step, which already happened.
                            let client =
                                OAuthClientConfig::new(client_id.clone(), REFRESH_REDIRECT)
                                    .with_client_secret(secret.expose_secret().to_owned());
                            manager
                                .configure_client(client)
                                .map_err(|error| connect_error(name, error))?;
                        }
                        let client = AuthClient::new(client, manager);
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
        let connections = self.source.connections();
        let mut clients = HashMap::new();
        for name in names {
            clients.insert(name.to_owned(), self.connect(&connections, name).await?);
        }
        Ok(McpSession { clients })
    }
}

#[async_trait]
impl ToolHostFactory for McpConnector {
    async fn open(&self, tools: &[String], _run: RunId) -> Result<Box<dyn ToolHost>, McpError> {
        Ok(Box::new(self.session(tools).await?))
    }

    async fn offered(&self, connector: &str) -> Result<Vec<String>, McpError> {
        let session = self.session(&[format!("{connector}.tools")]).await?;
        let tools = session.all_tools(connector).await?;
        Ok(tools.into_iter().map(|tool| tool.name).collect())
    }

    async fn offered_tools(&self, connector: &str) -> Result<Vec<crate::OfferedTool>, McpError> {
        let session = self.session(&[format!("{connector}.tools")]).await?;
        let tools = session
            .client(connector)?
            .list_all_tools()
            .await
            .map_err(|error| McpError::Call {
                connector: connector.to_owned(),
                detail: error.to_string(),
            })?;
        Ok(tools
            .into_iter()
            .map(|tool| crate::OfferedTool {
                name: tool.name.to_string(),
                description: tool.description.as_deref().unwrap_or_default().to_owned(),
                read_only: tool
                    .annotations
                    .as_ref()
                    .and_then(|hints| hints.read_only_hint),
                destructive: tool
                    .annotations
                    .as_ref()
                    .and_then(|hints| hints.destructive_hint),
            })
            .collect())
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
