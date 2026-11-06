//! Model providers, task routing and cost tracking.

pub mod anthropic;
pub mod media;
pub mod mock;
pub mod openai;
mod types;

pub use types::*;

use async_trait::async_trait;
use lighthouse_config::{Config, ModelConfig, ProviderKind};
use secrecy::SecretString;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Errors from model calls. They never contain API keys.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// The HTTP request failed.
    #[error("request to `{provider}` failed: {source}")]
    Http {
        /// Provider name.
        provider: String,
        /// Underlying error.
        #[source]
        source: reqwest::Error,
    },
    /// The provider answered with an error status.
    #[error("`{provider}` returned {status}: {body}")]
    Status {
        /// Provider name.
        provider: String,
        /// HTTP status.
        status: u16,
        /// Shortened response body.
        body: String,
    },
    /// The provider's answer could not be understood.
    #[error("unexpected answer from `{provider}`: {detail}")]
    Decode {
        /// Provider name.
        provider: String,
        /// What went wrong.
        detail: String,
    },
    /// The task has no route.
    #[error("no route for task `{0}`")]
    NoRoute(String),
    /// The provider is not configured or has no model for the task.
    #[error("provider `{provider}` cannot serve task `{task}`")]
    UnknownTarget {
        /// Provider name.
        provider: String,
        /// Task name.
        task: String,
    },
    /// The API key is missing.
    #[error("no API key stored for provider `{0}`")]
    MissingKey(String),
    /// The mock provider ran out of scripted answers.
    #[error("mock provider has no scripted answer left")]
    ScriptExhausted,
}

/// A model provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Sends one request to the given model.
    async fn complete(
        &self,
        model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError>;

    /// Sends a request and passes text to `sink` as it arrives.
    ///
    /// The default delivers the whole answer at once.
    async fn stream(
        &self,
        model: &str,
        request: &ChatRequest,
        sink: &TextSink<'_>,
    ) -> Result<ChatResponse, RouterError> {
        let response = self.complete(model, request).await?;
        sink(response.text_content());
        Ok(response)
    }
}

/// Receives pieces of an answer.
pub type TextSink<'a> = dyn Fn(String) + Send + Sync + 'a;

/// Calls `handle` with the data of every server-sent event in a response.
pub(crate) async fn for_each_event(
    mut response: reqwest::Response,
    provider: &str,
    mut handle: impl FnMut(&str),
) -> Result<(), RouterError> {
    let mut buffer = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|source| RouterError::Http {
        provider: provider.to_owned(),
        source,
    })? {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            if let Some(data) = String::from_utf8_lossy(&line)
                .trim_end()
                .strip_prefix("data: ")
            {
                handle(data);
            }
        }
    }
    Ok(())
}

/// The outcome of a routed call.
#[derive(Clone, Debug)]
pub struct Completion {
    /// The model's answer.
    pub response: ChatResponse,
    /// Provider that answered.
    pub provider: String,
    /// Model that answered.
    pub model: String,
    /// Cost of the call in EUR.
    pub cost_eur: f64,
    /// Whether the fallback answered.
    pub used_fallback: bool,
}

/// Provider preferences of an agent.
#[derive(Clone, Debug, Default)]
pub struct RouteOverride {
    /// Primary provider.
    pub provider: Option<String>,
    /// Fallback provider.
    pub fallback: Option<String>,
}

struct ProviderEntry {
    provider: Arc<dyn Provider>,
    tasks: BTreeMap<String, ModelConfig>,
}

/// Routes tasks to providers.
#[derive(Default)]
pub struct Router {
    providers: HashMap<String, ProviderEntry>,
    routes: HashMap<String, (String, Option<String>)>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

impl Router {
    /// Builds a router from the configuration. `keys` holds the API key per provider name.
    ///
    /// A provider without a key stays registered and fails when called, so the
    /// fallback still works and the error names what is missing.
    pub fn from_config(config: &Config, keys: &HashMap<String, SecretString>) -> Self {
        let mut router = Self::default();
        for (name, provider) in &config.providers {
            let key = keys.get(name).cloned();
            let base_url = provider.base_url.as_deref();
            let adapter: Arc<dyn Provider> = match (provider.kind, key) {
                (ProviderKind::Mock, _) => Arc::new(mock::MockProvider::echo()),
                (ProviderKind::Anthropic, Some(key)) => {
                    Arc::new(anthropic::Anthropic::new(name, key, base_url))
                }
                (ProviderKind::Openai, Some(key)) => {
                    Arc::new(openai::OpenAi::new(name, key, base_url))
                }
                (_, None) => Arc::new(MissingKey(name.clone())),
            };
            router.add_provider(name, adapter, provider.tasks.clone());
        }
        for (task, route) in &config.routing {
            router.set_route(task, &route.provider, route.fallback.as_deref());
        }
        router
    }

    /// Registers a provider.
    pub fn add_provider(
        &mut self,
        name: &str,
        provider: Arc<dyn Provider>,
        tasks: BTreeMap<String, ModelConfig>,
    ) {
        self.providers
            .insert(name.to_owned(), ProviderEntry { provider, tasks });
    }

    /// Sets the default route of a task.
    pub fn set_route(&mut self, task: &str, provider: &str, fallback: Option<&str>) {
        self.routes.insert(
            task.to_owned(),
            (provider.to_owned(), fallback.map(str::to_owned)),
        );
    }

    async fn call(
        &self,
        name: &str,
        task: &str,
        request: &ChatRequest,
    ) -> Result<Completion, RouterError> {
        let unknown = || RouterError::UnknownTarget {
            provider: name.to_owned(),
            task: task.to_owned(),
        };
        let entry = self.providers.get(name).ok_or_else(unknown)?;
        let model = entry.tasks.get(task).ok_or_else(unknown)?;
        let response = entry.provider.complete(&model.model, request).await?;
        Ok(Completion {
            cost_eur: response.usage.cost_eur(model),
            response,
            provider: name.to_owned(),
            model: model.model.clone(),
            used_fallback: false,
        })
    }

    /// Runs a request for a task, using the fallback if the primary provider fails.
    pub async fn complete(
        &self,
        task: &str,
        preference: &RouteOverride,
        request: &ChatRequest,
    ) -> Result<Completion, RouterError> {
        let default = self.routes.get(task);
        let primary = preference
            .provider
            .as_ref()
            .or(default.map(|route| &route.0))
            .ok_or_else(|| RouterError::NoRoute(task.to_owned()))?;
        let fallback = preference
            .fallback
            .as_ref()
            .or(default.and_then(|route| route.1.as_ref()));
        match (self.call(primary, task, request).await, fallback) {
            (Ok(completion), _) => Ok(completion),
            (Err(error), Some(fallback)) if fallback != primary => {
                tracing::warn!(provider = %primary, %error, "falling back");
                let mut completion = self.call(fallback, task, request).await?;
                completion.used_fallback = true;
                Ok(completion)
            }
            (Err(error), _) => Err(error),
        }
    }
}

impl Router {
    async fn call_stream(
        &self,
        name: &str,
        task: &str,
        request: &ChatRequest,
        sink: &TextSink<'_>,
    ) -> Result<Completion, RouterError> {
        let unknown = || RouterError::UnknownTarget {
            provider: name.to_owned(),
            task: task.to_owned(),
        };
        let entry = self.providers.get(name).ok_or_else(unknown)?;
        let model = entry.tasks.get(task).ok_or_else(unknown)?;
        let response = entry.provider.stream(&model.model, request, sink).await?;
        Ok(Completion {
            cost_eur: response.usage.cost_eur(model),
            response,
            provider: name.to_owned(),
            model: model.model.clone(),
            used_fallback: false,
        })
    }

    /// Runs a request and streams its text. The fallback only takes over while
    /// nothing has been delivered, so a reader never sees two answers.
    pub async fn stream(
        &self,
        task: &str,
        request: &ChatRequest,
        sink: &TextSink<'_>,
    ) -> Result<Completion, RouterError> {
        let route = self
            .routes
            .get(task)
            .ok_or_else(|| RouterError::NoRoute(task.to_owned()))?;
        let delivered = AtomicBool::new(false);
        let tracking = |text: String| {
            delivered.store(true, Ordering::Relaxed);
            sink(text);
        };
        match (
            self.call_stream(&route.0, task, request, &tracking).await,
            &route.1,
        ) {
            (Ok(completion), _) => Ok(completion),
            (Err(error), Some(fallback))
                if fallback != &route.0 && !delivered.load(Ordering::Relaxed) =>
            {
                tracing::warn!(provider = %route.0, %error, "falling back");
                let mut completion = self.call_stream(fallback, task, request, sink).await?;
                completion.used_fallback = true;
                Ok(completion)
            }
            (Err(error), _) => Err(error),
        }
    }
}

struct MissingKey(String);

#[async_trait]
impl Provider for MissingKey {
    async fn complete(
        &self,
        _model: &str,
        _request: &ChatRequest,
    ) -> Result<ChatResponse, RouterError> {
        Err(RouterError::MissingKey(self.0.clone()))
    }
}

/// Shortens a response body for error messages.
pub(crate) fn shorten(body: &str) -> String {
    body.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::mock::MockProvider;
    use super::*;

    fn tasks(model: &str) -> BTreeMap<String, ModelConfig> {
        BTreeMap::from([(
            "text".to_owned(),
            ModelConfig {
                model: model.to_owned(),
                input_eur_per_mtok: 3.0,
                output_eur_per_mtok: 15.0,
            },
        )])
    }

    fn router(primary: MockProvider, fallback: MockProvider) -> Router {
        let mut router = Router::default();
        router.add_provider("primary", Arc::new(primary), tasks("model-a"));
        router.add_provider("backup", Arc::new(fallback), tasks("model-b"));
        router.set_route("text", "primary", Some("backup"));
        router
    }

    #[tokio::test]
    async fn uses_the_route_and_computes_costs() {
        let primary = MockProvider::new([ChatResponse::text(
            "hello",
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
            },
        )]);
        let router = router(primary, MockProvider::new([]));
        let completion = router
            .complete("text", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap();
        assert_eq!(completion.provider, "primary");
        assert_eq!(completion.model, "model-a");
        assert!(!completion.used_fallback);
        assert!((completion.cost_eur - 18.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn falls_back_when_the_primary_fails() {
        let backup = MockProvider::new([ChatResponse::text("from backup", Usage::default())]);
        let router = router(MockProvider::new([]), backup);
        let completion = router
            .complete("text", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap();
        assert_eq!(completion.provider, "backup");
        assert!(completion.used_fallback);
        assert_eq!(completion.response.text_content(), "from backup");
    }

    #[tokio::test]
    async fn reports_the_error_without_a_fallback() {
        let mut router = Router::default();
        router.add_provider("primary", Arc::new(MockProvider::new([])), tasks("model-a"));
        router.set_route("text", "primary", None);
        let error = router
            .complete("text", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::ScriptExhausted));
    }

    #[tokio::test]
    async fn agent_preference_overrides_the_route() {
        let backup = MockProvider::new([ChatResponse::text("chosen", Usage::default())]);
        let router = router(MockProvider::new([]), backup);
        let preference = RouteOverride {
            provider: Some("backup".to_owned()),
            fallback: None,
        };
        let completion = router
            .complete("text", &preference, &ChatRequest::default())
            .await
            .unwrap();
        assert_eq!(completion.provider, "backup");
        assert!(!completion.used_fallback);
    }

    #[tokio::test]
    async fn provider_without_a_key_fails_with_a_clear_error() {
        let mut router = Router::default();
        router.add_provider(
            "primary",
            Arc::new(MissingKey("primary".to_owned())),
            tasks("model-a"),
        );
        router.set_route("text", "primary", None);
        let error = router
            .complete("text", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "no API key stored for provider `primary`"
        );
    }

    #[tokio::test]
    async fn unknown_task_and_provider_are_errors() {
        let router = router(MockProvider::new([]), MockProvider::new([]));
        let error = router
            .complete("image", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::NoRoute(_)));
        let preference = RouteOverride {
            provider: Some("nobody".to_owned()),
            fallback: Some("nobody".to_owned()),
        };
        let error = router
            .complete("text", &preference, &ChatRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::UnknownTarget { .. }));
    }

    #[tokio::test]
    async fn streaming_delivers_text_and_falls_back_before_any_text() {
        let answer = || ChatResponse::text("from backup", Usage::default());
        let router = router(MockProvider::new([]), MockProvider::new([answer()]));
        let seen = std::sync::Mutex::new(Vec::new());
        let completion = router
            .stream("text", &ChatRequest::default(), &|text| {
                seen.lock().unwrap().push(text);
            })
            .await
            .unwrap();
        assert!(completion.used_fallback);
        assert_eq!(*seen.lock().unwrap(), ["from backup"]);
    }
}
