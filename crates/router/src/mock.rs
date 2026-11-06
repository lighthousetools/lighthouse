//! A scripted provider for tests and dry runs.

use crate::{ChatRequest, ChatResponse, Provider, RouterError, Usage};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Answers with scripted responses in order, or echoes when configured to.
pub struct MockProvider {
    script: Mutex<VecDeque<ChatResponse>>,
    echo: bool,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    /// Creates a provider with a fixed script.
    pub fn new(script: impl IntoIterator<Item = ChatResponse>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            echo: false,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Creates a provider that answers with a fixed text.
    pub fn echo() -> Self {
        Self {
            echo: true,
            ..Self::new([])
        }
    }

    /// Returns all requests seen so far.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("mock lock").clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError> {
        self.requests
            .lock()
            .expect("mock lock")
            .push(request.clone());
        if let Some(response) = self.script.lock().expect("mock lock").pop_front() {
            return Ok(response);
        }
        if self.echo {
            return Ok(ChatResponse::text("mock answer", Usage::default()));
        }
        Err(RouterError::ScriptExhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_answers_and_records_requests() {
        let mock = MockProvider::echo();
        let response = mock.complete("any", &ChatRequest::default()).await.unwrap();
        assert_eq!(response.text_content(), "mock answer");
        assert_eq!(mock.requests().len(), 1);
    }
}
