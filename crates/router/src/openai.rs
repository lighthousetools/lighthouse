//! Adapter for the OpenAI Chat Completions API.

use crate::{
    Block, ChatRequest, ChatResponse, Provider, Role, RouterError, TextSink, Usage, for_each_event,
    shorten,
};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// OpenAI provider.
pub struct OpenAi {
    name: String,
    key: SecretString,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAi {
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

fn encode_messages(request: &ChatRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        let mut text = Vec::new();
        let mut calls = Vec::new();
        for block in &message.content {
            match block {
                Block::Text { text: part } => text.push(part.as_str()),
                Block::ToolUse { id, name, input } => calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": input.to_string()},
                })),
                Block::ToolResult { id, content, .. } => {
                    messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                }
            }
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        if !text.is_empty() || !calls.is_empty() {
            let mut entry = json!({"role": role, "content": text.join("\n")});
            if !calls.is_empty() {
                entry["tool_calls"] = json!(calls);
            }
            messages.push(entry);
        }
    }
    messages
}

fn encode_request(model: &str, request: &ChatRequest) -> Value {
    let mut body = json!({
        "model": model,
        "max_completion_tokens": request.max_tokens,
        "messages": encode_messages(request),
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({"type": "function", "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }})
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiMessage,
}

#[derive(Deserialize)]
struct ApiMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ApiToolCall>,
}

#[derive(Deserialize)]
struct ApiToolCall {
    id: String,
    function: ApiFunction,
}

#[derive(Deserialize)]
struct ApiFunction {
    name: String,
    arguments: String,
}

/// Reads one streamed chunk into the text and usage so far.
fn apply_chunk(data: &str, text: &mut String, usage: &mut Usage, sink: &TextSink<'_>) {
    let Ok(chunk) = serde_json::from_str::<Value>(data) else {
        return;
    };
    if let Some(piece) = chunk["choices"][0]["delta"]["content"].as_str() {
        text.push_str(piece);
        sink(piece.to_owned());
    }
    if let Some(input) = chunk["usage"]["prompt_tokens"].as_u64() {
        usage.input_tokens = input;
        usage.output_tokens = chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    }
}

#[async_trait]
impl Provider for OpenAi {
    async fn stream(
        &self,
        model: &str,
        request: &ChatRequest,
        sink: &TextSink<'_>,
    ) -> Result<ChatResponse, RouterError> {
        let mut body = encode_request(model, request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(self.key.expose_secret())
            .json(&body)
            .send()
            .await
            .map_err(|source| RouterError::Http {
                provider: self.name.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RouterError::Status {
                provider: self.name.clone(),
                status: status.as_u16(),
                body: shorten(&body),
            });
        }
        let mut text = String::new();
        let mut usage = Usage::default();
        for_each_event(response, &self.name, |data| {
            apply_chunk(data, &mut text, &mut usage, sink);
        })
        .await?;
        Ok(ChatResponse::text(text, usage))
    }

    async fn complete(
        &self,
        model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError> {
        let http_error = |source| RouterError::Http {
            provider: self.name.clone(),
            source,
        };
        let decode_error = |detail: String| RouterError::Decode {
            provider: self.name.clone(),
            detail,
        };
        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(self.key.expose_secret())
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
            serde_json::from_str(&body).map_err(|error| decode_error(error.to_string()))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| decode_error("no choices".to_owned()))?;
        let mut content = Vec::new();
        if let Some(text) = choice.message.content.filter(|text| !text.is_empty()) {
            content.push(Block::Text { text });
        }
        for call in choice.message.tool_calls {
            let input = serde_json::from_str(&call.function.arguments)
                .map_err(|error| decode_error(format!("tool arguments: {error}")))?;
            content.push(Block::ToolUse {
                id: call.id,
                name: call.function.name,
                input,
            });
        }
        Ok(ChatResponse {
            content,
            usage: Usage {
                input_tokens: parsed.usage.prompt_tokens,
                output_tokens: parsed.usage.completion_tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolSpec};
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> OpenAi {
        OpenAi::new(
            "openai",
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
                        input: json!({"a": 1}),
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
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "1");
        assert_eq!(messages.len(), 4);
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn parses_text_tool_calls_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(bearer_token("test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {
                    "content": "Checking",
                    "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":1}"}}],
                }}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 7},
            })))
            .mount(&server)
            .await;
        let response = provider(&server)
            .complete("model-x", &request())
            .await
            .unwrap();
        assert_eq!(response.text_content(), "Checking");
        let (id, name, input) = response.tool_uses().next().unwrap();
        assert_eq!((id, name, input), ("c1", "lookup", &json!({"q": 1})));
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 5,
                output_tokens: 7
            }
        );
    }

    #[tokio::test]
    async fn reports_error_status_without_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let error = provider(&server)
            .complete("model-x", &request())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("500") && !error.contains("test-key"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn reports_empty_choices_and_bad_arguments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [], "usage": {"prompt_tokens": 0, "completion_tokens": 0},
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": null, "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "x", "arguments": "nope"}}
                ]}}],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0},
            })))
            .mount(&server)
            .await;
        let provider = provider(&server);
        assert!(matches!(
            provider.complete("m", &request()).await,
            Err(RouterError::Decode { .. })
        ));
        assert!(matches!(
            provider.complete("m", &request()).await,
            Err(RouterError::Decode { .. })
        ));
    }

    #[test]
    fn stream_chunks_build_text_and_usage() {
        let sink = |_: String| {};
        let (mut text, mut usage) = (String::new(), Usage::default());
        for data in [
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
            "[DONE]",
        ] {
            apply_chunk(data, &mut text, &mut usage, &sink);
        }
        assert_eq!(text, "Hi");
        assert_eq!((usage.input_tokens, usage.output_tokens), (3, 1));
    }
}
