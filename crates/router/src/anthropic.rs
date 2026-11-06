//! Adapter for the Anthropic Messages API.

use crate::{Block, ChatRequest, ChatResponse, Provider, Role, RouterError, Usage, shorten};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Anthropic provider.
pub struct Anthropic {
    name: String,
    key: SecretString,
    base_url: String,
    http: reqwest::Client,
}

impl Anthropic {
    /// Creates the adapter.
    pub fn new(name: &str, key: SecretString, base_url: Option<&str>) -> Self {
        Self {
            name: name.to_owned(),
            key,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
            http: reqwest::Client::new(),
        }
    }
}

fn encode_block(block: &Block) -> Value {
    match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Block::ToolResult {
            id,
            content,
            is_error,
        } => {
            json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error})
        }
    }
}

fn encode_request(model: &str, request: &ChatRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({"role": role, "content": message.content.iter().map(encode_block).collect::<Vec<_>>()})
        })
        .collect();
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema}))
        .collect();
    let mut body = json!({"model": model, "max_tokens": request.max_tokens, "messages": messages});
    if !request.system.is_empty() {
        body["system"] = json!(request.system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiBlock>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[async_trait]
impl Provider for Anthropic {
    async fn complete(
        &self,
        model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError> {
        let http_error = |source| RouterError::Http {
            provider: self.name.clone(),
            source,
        };
        let response = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.key.expose_secret())
            .header("anthropic-version", API_VERSION)
            .json(&encode_request(model, request))
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        let body = response.text().await.map_err(http_error)?;
        if !status.is_success() {
            return Err(RouterError::Status {
                provider: self.name.clone(),
                status: status.as_u16(),
                body: shorten(&body),
            });
        }
        let parsed: ApiResponse =
            serde_json::from_str(&body).map_err(|error| RouterError::Decode {
                provider: self.name.clone(),
                detail: error.to_string(),
            })?;
        let content = parsed
            .content
            .into_iter()
            .filter_map(|block| match block {
                ApiBlock::Text { text } => Some(Block::Text { text }),
                ApiBlock::ToolUse { id, name, input } => Some(Block::ToolUse { id, name, input }),
                ApiBlock::Other => None,
            })
            .collect();
        Ok(ChatResponse {
            content,
            usage: Usage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolSpec};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> Anthropic {
        Anthropic::new(
            "anthropic",
            SecretString::from("test-key"),
            Some(&server.uri()),
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            system: "be brief".to_owned(),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![Block::ToolUse {
                        id: "1".to_owned(),
                        name: "lookup".to_owned(),
                        input: json!({}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![Block::ToolResult {
                        id: "1".to_owned(),
                        content: "ok".to_owned(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "lookup".to_owned(),
                description: "Looks up".to_owned(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 100,
        }
    }

    #[test]
    fn encodes_the_wire_format() {
        let body = encode_request("model-x", &request());
        assert_eq!(body["model"], "model-x");
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "1");
    }

    #[test]
    fn omits_empty_system_and_tools() {
        let body = encode_request("model-x", &ChatRequest::default());
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[tokio::test]
    async fn parses_text_tool_calls_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [
                    {"type": "text", "text": "Checking"},
                    {"type": "tool_use", "id": "t1", "name": "lookup", "input": {"q": 1}},
                    {"type": "thinking", "thinking": "ignored"},
                ],
                "usage": {"input_tokens": 12, "output_tokens": 34},
            })))
            .mount(&server)
            .await;
        let response = provider(&server)
            .complete("model-x", &request())
            .await
            .unwrap();
        assert_eq!(response.text_content(), "Checking");
        assert_eq!(response.tool_uses().next().unwrap().1, "lookup");
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 34
            }
        );
    }

    #[tokio::test]
    async fn reports_error_status_without_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;
        let error = provider(&server)
            .complete("model-x", &request())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("429") && error.contains("slow down"),
            "{error}"
        );
        assert!(!error.contains("test-key"));
    }

    #[tokio::test]
    async fn reports_malformed_answers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let error = provider(&server)
            .complete("model-x", &request())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::Decode { .. }));
    }

    #[tokio::test]
    async fn reports_connection_errors() {
        let provider = Anthropic::new(
            "anthropic",
            SecretString::from("test-key"),
            Some("http://127.0.0.1:1/"),
        );
        let error = provider.complete("model-x", &request()).await.unwrap_err();
        assert!(matches!(error, RouterError::Http { .. }));
    }
}
