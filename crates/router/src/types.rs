use lighthouse_config::ModelConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The caller.
    User,
    /// The model.
    Assistant,
}

/// A piece of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// The model asks to call a tool.
    ToolUse {
        /// Call ID.
        id: String,
        /// Tool name.
        name: String,
        /// Tool arguments.
        input: Value,
    },
    /// The result of a tool call.
    ToolResult {
        /// ID of the call.
        id: String,
        /// Result text.
        content: String,
        /// Whether the call failed.
        is_error: bool,
    },
}

/// One message in a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Author.
    pub role: Role,
    /// Content.
    pub content: Vec<Block>,
}

impl Message {
    /// Creates a user message with plain text.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }
    }
}

/// A tool offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// Description for the model.
    pub description: String,
    /// JSON schema of the arguments.
    pub input_schema: Value,
}

/// A request to a model.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    /// System prompt.
    pub system: String,
    /// Conversation so far.
    pub messages: Vec<Message>,
    /// Available tools.
    pub tools: Vec<ToolSpec>,
    /// Upper limit for the answer.
    pub max_tokens: u32,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 16_384,
        }
    }
}

/// Token usage of one call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl Usage {
    /// Computes the cost in EUR with the given prices.
    pub fn cost_eur(&self, model: &ModelConfig) -> f64 {
        (self.input_tokens as f64 * model.input_eur_per_mtok
            + self.output_tokens as f64 * model.output_eur_per_mtok)
            / 1_000_000.0
    }
}

/// A model's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatResponse {
    /// Answer content.
    pub content: Vec<Block>,
    /// Token usage.
    pub usage: Usage,
}

impl ChatResponse {
    /// Creates a plain text answer.
    pub fn text(text: impl Into<String>, usage: Usage) -> Self {
        Self {
            content: vec![Block::Text { text: text.into() }],
            usage,
        }
    }

    /// Returns all text blocks joined together.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns the requested tool calls.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|block| match block {
            Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}
