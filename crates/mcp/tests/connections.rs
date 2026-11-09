//! Tests against real MCP servers: stdio, HTTP, bearer and the full OAuth flow.

use axum::Json;
use axum::Router;
use axum::extract::{Form, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use lighthouse_config::{ConnectorAuth, ConnectorConfig, ConnectorTransport};
use lighthouse_core::TenantId;
use lighthouse_mcp::demo::DemoServer;
use lighthouse_mcp::{McpConnector, McpError, OAuthFlows, ToolHost};
use lighthouse_store::testing::fresh_store;
use lighthouse_vault::{MasterKey, Vault};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;

const TOKEN: &str = "test-access-token";
const REFRESHED: &str = "test-refreshed-token";

async fn vault() -> Vault {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    Vault::new(
        fresh_store().await,
        TenantId::DEFAULT,
        &MasterKey::from_base64(&key).unwrap(),
    )
}

fn http_connector(url: String, auth: ConnectorAuth) -> ConnectorConfig {
    ConnectorConfig {
        transport: ConnectorTransport::Http {
            url,
            auth,
            client_id: None,
            scopes: Vec::new(),
            allow_private_addresses: true,
            headers: Default::default(),
        },
        read_tools: vec!["list_issues".to_owned()],
        draft_tools: Vec::new(),
    }
}

fn connectors(config: ConnectorConfig) -> BTreeMap<String, ConnectorConfig> {
    BTreeMap::from([("demo".to_owned(), config)])
}

fn tools() -> Vec<String> {
    vec!["demo.list_issues".to_owned()]
}

async fn assert_works(session: &impl ToolHost) {
    let specs = session.list_tools(&tools()).await.unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "demo.list_issues");
    let output = session
        .call_tool("demo.list_issues", json!({}))
        .await
        .unwrap();
    assert!(output.text.contains("DEMO-1"));
    assert!(!output.is_error);
}

fn mcp_service() -> StreamableHttpService<DemoServer, LocalSessionManager> {
    StreamableHttpService::new(
        || Ok(DemoServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

#[derive(Clone, Default)]
struct AuthServer {
    base: Arc<Mutex<String>>,
    forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
    /// Whether the server takes client ID metadata documents.
    cimd: Arc<std::sync::atomic::AtomicBool>,
}

async fn require_token(State(state): State<AuthServer>, request: Request, next: Next) -> Response {
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let valid = [format!("Bearer {TOKEN}"), format!("Bearer {REFRESHED}")];
    if header_value.is_some_and(|value| valid.iter().any(|token| token == value)) {
        return next.run(request).await;
    }
    let base = state.base.lock().await.clone();
    let challenge =
        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
    )
        .into_response()
}

async fn resource_metadata(State(state): State<AuthServer>) -> Json<Value> {
    let base = state.base.lock().await.clone();
    Json(json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}))
}

async fn server_metadata(State(state): State<AuthServer>) -> Json<Value> {
    let base = state.base.lock().await.clone();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": state.cimd.load(std::sync::atomic::Ordering::SeqCst),
    }))
}

async fn register(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let redirect_uris = body["redirect_uris"].clone();
    (
        StatusCode::CREATED,
        Json(json!({"client_id": "registered-client", "redirect_uris": redirect_uris})),
    )
}

async fn token(
    State(state): State<AuthServer>,
    headers: axum::http::HeaderMap,
    Form(mut form): Form<HashMap<String, String>>,
) -> Json<Value> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        form.insert("authorization".to_owned(), value.to_owned());
    }
    let refresh = form
        .get("grant_type")
        .is_some_and(|grant| grant == "refresh_token");
    state.forms.lock().await.push(form);
    let (access, expires) = if refresh {
        (REFRESHED, 3600)
    } else {
        (TOKEN, 1)
    };
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "refresh_token": "test-refresh-token",
        "expires_in": expires,
    }))
}

async fn oauth_server() -> (String, AuthServer) {
    let state = AuthServer::default();
    let protected = Router::new()
        .nest_service("/mcp", mcp_service())
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/register", post(register))
        .route("/token", post(token))
        .with_state(state.clone())
        .merge(protected);
    let base = serve(router).await;
    *state.base.lock().await = base.clone();
    (base, state)
}

#[tokio::test]
async fn stdio_server_lists_and_calls_tools() {
    let config = ConnectorConfig {
        transport: ConnectorTransport::Stdio {
            command: env!("CARGO_BIN_EXE_lighthouse-demo-mcp").to_owned(),
            args: Vec::new(),
            secret_env: BTreeMap::from([("DEMO_TOKEN".to_owned(), "mcp:demo".to_owned())]),
        },
        read_tools: vec!["list_issues".to_owned()],
        draft_tools: Vec::new(),
    };
    let vault = vault().await;
    vault
        .put("mcp:demo", &SecretString::from("value"))
        .await
        .unwrap();
    let session = McpConnector::new(connectors(config), vault)
        .session(&tools())
        .await
        .unwrap();
    assert_works(&session).await;
}

#[tokio::test]
async fn stdio_server_needs_its_secret() {
    let config = ConnectorConfig {
        transport: ConnectorTransport::Stdio {
            command: env!("CARGO_BIN_EXE_lighthouse-demo-mcp").to_owned(),
            args: Vec::new(),
            secret_env: BTreeMap::from([("DEMO_TOKEN".to_owned(), "mcp:demo".to_owned())]),
        },
        read_tools: Vec::new(),
        draft_tools: Vec::new(),
    };
    let error = McpConnector::new(connectors(config), vault().await)
        .session(&tools())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::NotAuthorized { .. }));
}

#[tokio::test]
async fn tool_errors_and_unknown_tools_are_reported() {
    let config = ConnectorConfig {
        transport: ConnectorTransport::Stdio {
            command: env!("CARGO_BIN_EXE_lighthouse-demo-mcp").to_owned(),
            args: Vec::new(),
            secret_env: BTreeMap::new(),
        },
        read_tools: Vec::new(),
        draft_tools: Vec::new(),
    };
    let session = McpConnector::new(connectors(config), vault().await)
        .session(&tools())
        .await
        .unwrap();
    let output = session
        .call_tool("demo.delete_issue", json!({}))
        .await
        .unwrap();
    assert!(output.is_error);
    assert!(session.call_tool("demo.nothing", json!({})).await.is_err());
    assert!(session.call_tool("other.tool", json!({})).await.is_err());
    assert!(session.call_tool("no-dot", json!({})).await.is_err());
    assert!(
        session
            .list_tools(&["demo.nothing".to_owned()])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn http_server_without_auth() {
    let base = serve(Router::new().nest_service("/mcp", mcp_service())).await;
    let config = http_connector(format!("{base}/mcp"), ConnectorAuth::None);
    let session = McpConnector::new(connectors(config), vault().await)
        .session(&tools())
        .await
        .unwrap();
    assert_works(&session).await;
    let names: Vec<_> = session
        .all_tools("demo")
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names.len(), lighthouse_mcp::demo::TOOLS.len());
    assert_eq!(names[..3], ["list_issues", "echo", "delete_issue"]);
}

#[tokio::test]
async fn http_server_with_bearer_token() {
    let (base, _) = oauth_server().await;
    let config = http_connector(format!("{base}/mcp"), ConnectorAuth::Bearer);
    let vault = vault().await;
    let connector = McpConnector::new(connectors(config), vault.clone());
    assert!(matches!(
        connector.session(&tools()).await,
        Err(McpError::NotAuthorized { .. })
    ));
    vault
        .put("mcp:demo", &SecretString::from(TOKEN))
        .await
        .unwrap();
    assert_works(&connector.session(&tools()).await.unwrap()).await;
}

#[tokio::test]
async fn unknown_connector_is_an_error() {
    let connector = McpConnector::new(BTreeMap::new(), vault().await);
    assert!(matches!(
        connector.session(&tools()).await,
        Err(McpError::UnknownConnector(_))
    ));
}

#[tokio::test]
async fn oauth_flow_stores_tokens_in_the_vault_and_connects() {
    let (base, server) = oauth_server().await;
    let config = connectors(http_connector(format!("{base}/mcp"), ConnectorAuth::Oauth));
    let vault = vault().await;
    let connector = McpConnector::new(config.clone(), vault.clone());
    assert!(matches!(
        connector.session(&tools()).await,
        Err(McpError::NotAuthorized { .. })
    ));

    let flows = OAuthFlows::new(config.clone(), "https://lighthouse.test/");
    let authorize = flows.start("demo", &vault).await.unwrap();
    let url = url::Url::parse(&authorize).unwrap();
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert!(authorize.starts_with(&format!("{base}/authorize")));
    assert_eq!(query["client_id"], "registered-client");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(
        query["redirect_uri"],
        "https://lighthouse.test/oauth/callback"
    );

    assert!(flows.finish("code", "wrong-state", None).await.is_err());
    assert_eq!(
        flows
            .finish("the-code", &query["state"], None)
            .await
            .unwrap(),
        "demo"
    );

    let exchange = server.forms.lock().await[0].clone();
    assert_eq!(exchange["code"], "the-code");
    assert!(exchange.contains_key("code_verifier"));

    let stored = vault.get("oauth:demo").await.unwrap().unwrap();
    assert!(stored.expose_secret().contains(TOKEN));

    // The first token expires immediately, so this connection must refresh it.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let session = McpConnector::new(config, vault.clone())
        .session(&tools())
        .await
        .unwrap();
    assert_works(&session).await;
    let forms = server.forms.lock().await;
    assert!(
        forms
            .iter()
            .any(|form| form["grant_type"] == "refresh_token"),
        "token was not refreshed"
    );
    let stored = vault.get("oauth:demo").await.unwrap().unwrap();
    assert!(stored.expose_secret().contains(REFRESHED));
}

#[tokio::test]
async fn oauth_start_rejects_other_connectors() {
    let config = connectors(http_connector(
        "http://127.0.0.1:1/mcp".to_owned(),
        ConnectorAuth::None,
    ));
    let flows = OAuthFlows::new(config, "https://lighthouse.test");
    let vault = vault().await;
    assert!(matches!(
        flows.start("demo", &vault).await,
        Err(McpError::OAuth(_))
    ));
    assert!(matches!(
        flows.start("nobody", &vault).await,
        Err(McpError::UnknownConnector(_))
    ));
}

/// Whether a token request carried the client secret, as a form field or with basic auth.
fn carries_secret(form: &HashMap<String, String>, id: &str, secret: &str) -> bool {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    form.get("client_secret")
        .is_some_and(|value| value == secret)
        || form
            .get("authorization")
            .is_some_and(|value| value == &format!("Basic {basic}"))
}

#[tokio::test]
async fn a_confidential_client_sends_its_secret_on_exchange_and_refresh() {
    let (base, server) = oauth_server().await;
    let mut demo = http_connector(format!("{base}/mcp"), ConnectorAuth::Oauth);
    if let ConnectorTransport::Http { client_id, .. } = &mut demo.transport {
        *client_id = Some("own-app".to_owned());
    }
    let config = connectors(demo);
    let vault = vault().await;
    vault
        .put("oauth-client:demo", &SecretString::from("app-secret"))
        .await
        .unwrap();
    let flows = OAuthFlows::new(config.clone(), "https://lighthouse.test/");
    let authorize = flows.start("demo", &vault).await.unwrap();
    let query: HashMap<_, _> = url::Url::parse(&authorize)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(query["client_id"], "own-app");
    flows
        .finish("the-code", &query["state"], None)
        .await
        .unwrap();
    assert!(carries_secret(
        &server.forms.lock().await[0],
        "own-app",
        "app-secret"
    ));

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let session = McpConnector::new(config, vault)
        .session(&tools())
        .await
        .unwrap();
    assert_works(&session).await;
    let forms = server.forms.lock().await;
    let refresh = forms
        .iter()
        .find(|form| form["grant_type"] == "refresh_token")
        .expect("refreshed");
    assert!(
        carries_secret(refresh, "own-app", "app-secret"),
        "{refresh:?}"
    );
}

#[tokio::test]
async fn headers_of_a_connector_reach_the_server() {
    async fn require_tenant(request: Request, next: Next) -> Response {
        match request.headers().get("x-tenant-id") {
            Some(value) if value == "acme" => next.run(request).await,
            _ => StatusCode::FORBIDDEN.into_response(),
        }
    }
    let router = Router::new()
        .nest_service("/mcp", mcp_service())
        .layer(middleware::from_fn(require_tenant));
    let base = serve(router).await;
    let mut config = http_connector(format!("{base}/mcp"), ConnectorAuth::None);
    let session = McpConnector::new(connectors(config.clone()), vault().await)
        .session(&tools())
        .await;
    assert!(session.is_err());
    if let ConnectorTransport::Http { headers, .. } = &mut config.transport {
        headers.insert("X-Tenant-Id".to_owned(), "acme".to_owned());
    }
    let session = McpConnector::new(connectors(config.clone()), vault().await)
        .session(&tools())
        .await
        .unwrap();
    assert_works(&session).await;
    if let ConnectorTransport::Http { headers, .. } = &mut config.transport {
        headers.insert("bad header".to_owned(), "x".to_owned());
    }
    assert!(
        McpConnector::new(connectors(config), vault().await)
            .session(&tools())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_openapi_description_becomes_tools_that_call_the_api() {
    async fn deals(request: Request) -> Response {
        let query = request.uri().query().unwrap_or_default().to_owned();
        if !query.contains("api_token=k1") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(json!({"data": [{"id": 7, "title": "Acme renewal"}], "query": query})).into_response()
    }
    let router = Router::new().route("/v1/deals", get(deals));
    let base = serve(router).await;
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path().join("spec.yaml");
    std::fs::write(
        &spec,
        format!(
            "openapi: 3.0.0\nservers:\n  - url: {base}/v1\npaths:\n  /deals:\n    get:\n      summary: List deals\n      parameters:\n        - name: status\n          in: query\n          schema: {{type: string}}\n    post:\n      summary: Add a deal\n"
        ),
    )
    .unwrap();
    let config = ConnectorConfig {
        transport: ConnectorTransport::OpenApi {
            spec: spec.to_string_lossy().into_owned(),
            base_url: None,
            key: lighthouse_config::ApiKeyPlacement::Query,
            key_name: Some("api_token".to_owned()),
            headers: Default::default(),
            operations: Vec::new(),
            reads_are_safe: true,
        },
        read_tools: Vec::new(),
        draft_tools: Vec::new(),
    };
    let vault = vault().await;
    let connector = McpConnector::new(
        BTreeMap::from([("crm".to_owned(), config.clone())]),
        vault.clone(),
    );
    let tools = vec!["crm.get_deals".to_owned()];
    assert!(matches!(
        connector.session(&tools).await,
        Err(McpError::NotAuthorized { .. })
    ));
    vault
        .put("mcp:crm", &SecretString::from("k1"))
        .await
        .unwrap();
    let session = connector.session(&tools).await.unwrap();
    let names: Vec<_> = session
        .all_tools("crm")
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names, ["get_deals", "post_deals"]);
    let output = session
        .call_tool("crm.get_deals", json!({"status": "open"}))
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert!(
        output.text.contains("Acme renewal") && output.text.contains("status=open"),
        "{}",
        output.text
    );
    let policy = lighthouse_mcp::ToolPolicy::new(&BTreeMap::from([("crm".to_owned(), config)]));
    assert_eq!(
        policy.level("crm.get_deals"),
        lighthouse_core::agent::ToolLevel::Read
    );
    assert_eq!(
        policy.level("crm.post_deals"),
        lighthouse_core::agent::ToolLevel::Write
    );
}

#[tokio::test]
async fn a_server_that_takes_metadata_documents_gets_the_document_url_as_client_id() {
    let (base, server) = oauth_server().await;
    server.cimd.store(true, std::sync::atomic::Ordering::SeqCst);
    let config = connectors(http_connector(format!("{base}/mcp"), ConnectorAuth::Oauth));
    let vault = vault().await;
    let flows = OAuthFlows::new(config.clone(), "https://lighthouse.test/");
    assert_eq!(
        flows.client_metadata_url().as_deref(),
        Some("https://lighthouse.test/oauth/client-metadata.json")
    );
    let document = flows.client_metadata().unwrap();
    assert_eq!(
        document["redirect_uris"][0],
        "https://lighthouse.test/oauth/callback"
    );
    assert_eq!(
        document["client_id"],
        "https://lighthouse.test/oauth/client-metadata.json"
    );
    let authorize = flows.start("demo", &vault).await.unwrap();
    let query: HashMap<_, _> = url::Url::parse(&authorize)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        query["client_id"],
        "https://lighthouse.test/oauth/client-metadata.json"
    );
    // A server without HTTPS cannot offer a document, so it registers as before.
    assert!(
        OAuthFlows::new(config, "http://localhost:8080")
            .client_metadata_url()
            .is_none()
    );
}
