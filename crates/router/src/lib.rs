//! Model providers, task routing and cost tracking.

pub mod anthropic;
pub mod mock;
pub mod openai;
mod types;

pub use types::*;

use async_trait::async_trait;
use lighthouse_config::{Config, ModelConfig, ProviderKind};
use secrecy::SecretString;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

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
    pub fn from_config(
        config: &Config,
        keys: &HashMap<String, SecretString>,
    ) -> Result<Self, RouterError> {
        let mut router = Self::default();
        for (name, provider) in &config.providers {
            let key = || {
                keys.get(name)
                    .cloned()
                    .ok_or_else(|| RouterError::MissingKey(name.clone()))
            };
            let adapter: Arc<dyn Provider> = match provider.kind {
                ProviderKind::Anthropic => Arc::new(anthropic::Anthropic::new(
                    name,
                    key()?,
                    provider.base_url.as_deref(),
                )),
                ProviderKind::Openai => Arc::new(openai::OpenAi::new(
                    name,
                    key()?,
                    provider.base_url.as_deref(),
                )),
                ProviderKind::Mock => Arc::new(mock::MockProvider::echo()),
            };
            router.add_provider(name, adapter, provider.tasks.clone());
        }
        for (task, route) in &config.routing {
            router.set_route(task, &route.provider, route.fallback.as_deref());
        }
        Ok(router)
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
    async fn unknown_task_and_provider_are_errors() {
        let router = router(MockProvider::new([]), MockProvider::new([]));
        let error = router
            .complete("image", &RouteOverride::default(), &ChatRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::NoRoute(_)));
        let preference = RouteOverride {
            provider: Some("nobody".to_owned()),
            fallback: None,
        };
        let error = router
            .complete("text", &preference, &ChatRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(error, RouterError::UnknownTarget { .. }));
    }
}
