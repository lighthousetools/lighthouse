//! Turns a REST API with an OpenAPI 3 description into MCP tools.
//!
//! Every operation becomes a tool named after its method and path, such as `get_deals_id` for `GET /deals/{id}`.
//! Path, query and header parameters become properties of the input, and a JSON request body becomes `body`. The
//! bridge runs as an MCP server inside Lighthouse, so these tools go through the same allowlists, approvals and logs as
//! the tools of any other connector.

use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

/// The longest tool name. MCP clients commonly accept 64 characters.
const MAX_NAME: usize = 64;
/// The longest tool description in characters.
const MAX_DESCRIPTION: usize = 1_000;
/// The longest answer in characters.
const MAX_RESULT: usize = 60_000;
/// How deep `$ref` chains are followed before a schema becomes a plain object.
const MAX_DEPTH: usize = 8;
/// How long one API call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// HTTP methods that become tools.
const METHODS: [&str; 7] = ["get", "head", "post", "put", "patch", "delete", "options"];

/// Where a parameter goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Header,
}

#[derive(Clone, Debug)]
struct Parameter {
    name: String,
    location: Location,
}

/// One operation of the API.
#[derive(Clone, Debug)]
pub struct Operation {
    /// Tool name.
    pub name: String,
    description: String,
    method: Method,
    path: String,
    parameters: Vec<Parameter>,
    body: bool,
    schema: Map<String, Value>,
}

/// How the API key is sent.
#[derive(Clone, Debug)]
pub enum ApiKey {
    /// No key.
    None,
    /// `Authorization: Bearer <key>`.
    Bearer(SecretString),
    /// A header of this name.
    Header(String, SecretString),
    /// A query parameter of this name.
    Query(String, SecretString),
}

/// The bridge for one API.
#[derive(Clone, Debug)]
pub struct OpenApiServer {
    operations: Arc<Vec<Operation>>,
    base: String,
    key: ApiKey,
    headers: HeaderMap,
    http: reqwest::Client,
}

fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(limit).collect();
    short.push_str("\n[cut]");
    short
}

/// Follows a local `$ref` such as `#/components/schemas/Deal`.
fn pointer<'a>(spec: &'a Value, reference: &str) -> Option<&'a Value> {
    let path = reference.strip_prefix('#')?;
    spec.pointer(path)
}

/// Inlines local references, so the schema stands on its own.
fn resolve(value: &Value, spec: &Value, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return json!({"type": "object"});
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                return pointer(spec, reference).map_or_else(
                    || json!({"type": "object"}),
                    |target| resolve(target, spec, depth + 1),
                );
            }
            Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), resolve(value, spec, depth + 1)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| resolve(item, spec, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Descriptions that ship with Lighthouse, named `lighthouse:<name>` in place of a URL. They cover the parts of
/// large APIs that the bundled presets use.
const BUNDLED: [(&str, &str); 1] = [(
    "microsoft-graph-calendar",
    include_str!("../specs/microsoft-graph-calendar.yaml"),
)];

/// The text of a bundled description, for a spec such as `lighthouse:microsoft-graph-calendar`.
pub fn bundled(spec: &str) -> Option<&'static str> {
    let name = spec.strip_prefix("lighthouse:")?;
    BUNDLED
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// `GET /v1/deals/{id}` becomes `get_v1_deals_id`.
fn tool_name(method: &str, path: &str) -> String {
    let mut name = method.to_owned();
    for part in path.split(|c: char| !c.is_ascii_alphanumeric()) {
        if !part.is_empty() {
            name.push('_');
            name.push_str(&part.to_ascii_lowercase());
        }
    }
    name
}

fn unique(name: String, taken: &mut BTreeSet<String>) -> String {
    let mut base: String = name.chars().take(MAX_NAME).collect();
    if taken.insert(base.clone()) {
        return base;
    }
    base = name.chars().take(MAX_NAME - 4).collect();
    for counter in 2.. {
        let candidate = format!("{base}_{counter}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("the counter never ends")
}

fn parameters_of(value: Option<&Value>, spec: &Value) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .map(|list| list.iter().map(|item| resolve(item, spec, 0)).collect())
        .unwrap_or_default()
}

/// Reads the operations of a spec. With an allowlist, only those tools are kept.
pub fn operations(spec: &Value, allow: &[String]) -> Result<Vec<Operation>, String> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "the description has no paths".to_owned())?;
    let mut taken = BTreeSet::new();
    let mut found = Vec::new();
    for (path, item) in paths {
        let shared = parameters_of(item.get("parameters"), spec);
        for method in METHODS {
            let Some(operation) = item.get(method) else {
                continue;
            };
            let name = unique(tool_name(method, path), &mut taken);
            if !allow.is_empty() && !allow.contains(&name) {
                continue;
            }
            let mut properties = Map::new();
            let mut required = Vec::new();
            let mut parameters = Vec::new();
            let mut all = shared.clone();
            all.extend(parameters_of(operation.get("parameters"), spec));
            for parameter in all {
                let (Some(param), Some(location)) =
                    (parameter["name"].as_str(), parameter["in"].as_str())
                else {
                    continue;
                };
                let location = match location {
                    "path" => Location::Path,
                    "query" => Location::Query,
                    "header" => Location::Header,
                    _ => continue,
                };
                if parameters
                    .iter()
                    .any(|known: &Parameter| known.name == param)
                {
                    continue;
                }
                let mut schema = parameter
                    .get("schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "string"}));
                if let (Some(description), Value::Object(map)) =
                    (parameter["description"].as_str(), &mut schema)
                {
                    map.insert("description".to_owned(), json!(cut(description, 300)));
                }
                properties.insert(param.to_owned(), schema);
                if location == Location::Path || parameter["required"].as_bool().unwrap_or(false) {
                    required.push(json!(param));
                }
                parameters.push(Parameter {
                    name: param.to_owned(),
                    location,
                });
            }
            let body_schema = operation
                .pointer("/requestBody")
                .map(|body| resolve(body, spec, 0))
                .and_then(|body| {
                    let content = body.get("content")?.as_object()?;
                    let json_content = content
                        .iter()
                        .find(|(kind, _)| kind.contains("json"))
                        .map(|(_, value)| value.clone())?;
                    let schema = json_content
                        .get("schema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"}));
                    if body
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        required.push(json!("body"));
                    }
                    Some(schema)
                });
            let body = body_schema.is_some();
            if let Some(schema) = body_schema {
                properties.insert("body".to_owned(), schema);
            }
            let summary = [
                operation["summary"].as_str(),
                operation["description"].as_str(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(". ");
            let description = if summary.is_empty() {
                format!("{} {path}", method.to_uppercase())
            } else {
                cut(
                    &format!("{summary} ({} {path})", method.to_uppercase()),
                    MAX_DESCRIPTION,
                )
            };
            let mut schema = Map::new();
            schema.insert("type".to_owned(), json!("object"));
            schema.insert("properties".to_owned(), Value::Object(properties));
            schema.insert("required".to_owned(), Value::Array(required));
            found.push(Operation {
                name,
                description,
                method: Method::from_bytes(method.to_uppercase().as_bytes())
                    .map_err(|error| error.to_string())?,
                path: path.clone(),
                parameters,
                body,
                schema,
            });
        }
    }
    Ok(found)
}

/// The API's base URL: an override, or the first server of the spec, resolved against the spec's own URL.
pub fn base_url(
    spec: &Value,
    spec_location: &str,
    configured: Option<&str>,
) -> Result<String, String> {
    if let Some(base) = configured {
        return Ok(base.trim_end_matches('/').to_owned());
    }
    let server = spec
        .pointer("/servers/0")
        .ok_or_else(|| "the description names no server, set base_url".to_owned())?;
    let mut url = server["url"].as_str().unwrap_or_default().to_owned();
    if let Some(variables) = server.get("variables").and_then(Value::as_object) {
        for (name, variable) in variables {
            let default = variable["default"].as_str().unwrap_or_default();
            url = url.replace(&format!("{{{name}}}"), default);
        }
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(url.trim_end_matches('/').to_owned());
    }
    let origin = url::Url::parse(spec_location)
        .map_err(|_| "a relative server URL needs base_url".to_owned())?;
    let joined = origin.join(&url).map_err(|error| error.to_string())?;
    Ok(joined.as_str().trim_end_matches('/').to_owned())
}

/// Reads a spec in JSON or YAML.
pub fn parse_spec(text: &str) -> Result<Value, String> {
    serde_json::from_str(text)
        .or_else(|_| serde_norway::from_str(text))
        .map_err(|error| format!("not an OpenAPI description: {error}"))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl OpenApiServer {
    /// Builds the bridge.
    pub fn new(
        operations: Vec<Operation>,
        base: String,
        key: ApiKey,
        headers: HeaderMap,
        http: reqwest::Client,
    ) -> Self {
        Self {
            operations: Arc::new(operations),
            base,
            key,
            headers,
            http,
        }
    }

    /// The tools it offers.
    pub fn tool_names(&self) -> Vec<String> {
        self.operations
            .iter()
            .map(|operation| operation.name.clone())
            .collect()
    }

    /// Builds the request of a call: the URL with query, the extra headers and the body.
    fn request(
        &self,
        operation: &Operation,
        arguments: &Map<String, Value>,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut path = operation.path.clone();
        let mut query = Vec::new();
        let mut headers = self.headers.clone();
        for parameter in &operation.parameters {
            let Some(value) = arguments
                .get(&parameter.name)
                .filter(|value| !value.is_null())
            else {
                if parameter.location == Location::Path {
                    return Err(format!("`{}` is required", parameter.name));
                }
                continue;
            };
            match parameter.location {
                Location::Path => {
                    path = path.replace(&format!("{{{}}}", parameter.name), &encode(&scalar(value)))
                }
                Location::Query => match value {
                    Value::Array(items) => {
                        for item in items {
                            query.push(format!(
                                "{}={}",
                                encode(&parameter.name),
                                encode(&scalar(item))
                            ));
                        }
                    }
                    other => query.push(format!(
                        "{}={}",
                        encode(&parameter.name),
                        encode(&scalar(other))
                    )),
                },
                Location::Header => {
                    let name = HeaderName::try_from(parameter.name.as_str())
                        .map_err(|error| error.to_string())?;
                    let value =
                        HeaderValue::try_from(scalar(value)).map_err(|error| error.to_string())?;
                    headers.insert(name, value);
                }
            }
        }
        match &self.key {
            ApiKey::None => {}
            ApiKey::Bearer(key) => {
                let value = HeaderValue::try_from(format!("Bearer {}", key.expose_secret()))
                    .map_err(|_| "invalid key")?;
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
            ApiKey::Header(name, key) => {
                let name =
                    HeaderName::try_from(name.as_str()).map_err(|error| error.to_string())?;
                headers.insert(
                    name,
                    HeaderValue::try_from(key.expose_secret()).map_err(|_| "invalid key")?,
                );
            }
            ApiKey::Query(name, key) => {
                query.push(format!("{}={}", encode(name), encode(key.expose_secret())))
            }
        }
        let mut url = format!("{}{path}", self.base);
        if !query.is_empty() {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&query.join("&"));
        }
        let mut request = self
            .http
            .request(operation.method.clone(), url)
            .headers(headers)
            .timeout(CALL_TIMEOUT);
        if operation.body
            && let Some(body) = arguments.get("body")
        {
            request = request.json(body);
        }
        Ok(request)
    }
}

impl ServerHandler for OpenApiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            self.operations
                .iter()
                .map(|operation| {
                    Tool::new(
                        operation.name.clone(),
                        operation.description.clone(),
                        Arc::new(operation.schema.clone()),
                    )
                })
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let Some(operation) = self
            .operations
            .iter()
            .find(|operation| operation.name == name)
        else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {name}"),
                None,
            ));
        };
        let arguments = request.arguments.clone().unwrap_or_default();
        let failure =
            |text: String| Ok(CallToolResult::error(vec![ContentBlock::text(text)]).into());
        let built = match self.request(operation, &arguments) {
            Ok(built) => built,
            Err(message) => return failure(message),
        };
        let response = match built.send().await {
            Ok(response) => response,
            Err(error) => return failure(format!("request failed: {}", error.without_url())),
        };
        let status = response.status();
        let text = cut(&response.text().await.unwrap_or_default(), MAX_RESULT);
        if status.is_success() {
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
        } else {
            failure(format!("{status}: {text}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Value {
        json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://{region}.example.com/v1", "variables": {"region": {"default": "api"}}}],
            "paths": {
                "/deals": {
                    "get": {"summary": "List deals", "parameters": [
                        {"name": "status", "in": "query", "schema": {"type": "string"}},
                        {"$ref": "#/components/parameters/Limit"}
                    ]},
                    "post": {"summary": "Add a deal", "requestBody": {"required": true, "content": {
                        "application/json": {"schema": {"$ref": "#/components/schemas/Deal"}}}}}
                },
                "/deals/{id}": {
                    "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                    "get": {"operationId": "getDeal"},
                    "delete": {"summary": "Delete a deal"}
                }
            },
            "components": {
                "parameters": {"Limit": {"name": "limit", "in": "query", "schema": {"type": "integer"}}},
                "schemas": {"Deal": {"type": "object", "properties": {"title": {"type": "string"},
                    "next": {"$ref": "#/components/schemas/Deal"}}}}
            }
        })
    }

    #[test]
    fn operations_become_named_tools_with_inputs() {
        let found = operations(&spec(), &[]).unwrap();
        let names: Vec<&str> = found
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["get_deals", "post_deals", "get_deals_id", "delete_deals_id"]
        );
        let list = &found[0];
        assert!(
            list.schema["properties"]["limit"].is_object(),
            "references are followed"
        );
        assert_eq!(list.description, "List deals (GET /deals)");
        let add = &found[1];
        assert_eq!(add.schema["required"], json!(["body"]));
        assert_eq!(
            add.schema["properties"]["body"]["properties"]["title"]["type"],
            "string"
        );
        let one = &found[2];
        assert_eq!(one.schema["required"], json!(["id"]));
        let only = operations(&spec(), &["get_deals".to_owned()]).unwrap();
        assert_eq!(only.len(), 1);
    }

    #[test]
    fn the_base_url_comes_from_the_spec_or_the_configuration() {
        assert_eq!(
            base_url(&spec(), "https://x.test/spec.json", None).unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            base_url(&spec(), "", Some("https://own.test/")).unwrap(),
            "https://own.test"
        );
        let relative = json!({"servers": [{"url": "/api"}]});
        assert_eq!(
            base_url(&relative, "https://x.test/docs/spec.json", None).unwrap(),
            "https://x.test/api"
        );
        assert!(base_url(&json!({}), "", None).is_err());
    }

    #[test]
    fn long_and_colliding_names_stay_unique() {
        let mut taken = BTreeSet::new();
        let long = "get_".to_owned() + &"a".repeat(80);
        let first = unique(long.clone(), &mut taken);
        let second = unique(long, &mut taken);
        assert_eq!(first.len(), MAX_NAME);
        assert_ne!(first, second);
        assert!(second.len() <= MAX_NAME);
    }

    #[test]
    fn yaml_and_json_parse() {
        assert!(parse_spec("openapi: 3.0.0\npaths: {}\n").unwrap()["paths"].is_object());
        assert!(parse_spec("{\"paths\": {}}").is_ok());
        assert!(parse_spec(": : :").is_err());
    }

    #[test]
    fn requests_carry_path_query_key_and_body() {
        let found = operations(&spec(), &[]).unwrap();
        let server = OpenApiServer::new(
            found.clone(),
            "https://api.test".to_owned(),
            ApiKey::Query("api_token".to_owned(), SecretString::from("k1")),
            HeaderMap::new(),
            reqwest::Client::new(),
        );
        let arguments = |value: Value| value.as_object().cloned().unwrap();
        let request = server
            .request(&found[2], &arguments(json!({"id": "a/b"})))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://api.test/deals/a%2Fb?api_token=k1"
        );
        assert!(server.request(&found[2], &arguments(json!({}))).is_err());
        let request = server
            .request(&found[0], &arguments(json!({"status": "open", "limit": 5})))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().query(),
            Some("status=open&limit=5&api_token=k1")
        );
        let request = server
            .request(&found[1], &arguments(json!({"body": {"title": "T"}})))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.method(), Method::POST);
        assert_eq!(
            request.body().and_then(|body| body.as_bytes()),
            Some(&b"{\"title\":\"T\"}"[..])
        );
        let bearer = OpenApiServer::new(
            found.clone(),
            "https://api.test".to_owned(),
            ApiKey::Bearer(SecretString::from("t")),
            HeaderMap::new(),
            reqwest::Client::new(),
        );
        let request = bearer
            .request(&found[0], &arguments(json!({})))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer t");
    }
}
