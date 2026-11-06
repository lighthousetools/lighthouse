//! Configuration of Lighthouse.
//!
//! Values come from `lighthouse.toml` and can be overridden by environment
//! variables with the prefix `LIGHTHOUSE_`, where `__` separates nesting levels.
//! Secrets such as API keys live in the vault, not here. The only exception is
//! the admin token, which should be set through the environment.

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use secrecy::SecretString;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Errors while loading the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration could not be read or parsed.
    #[error("invalid configuration: {0}")]
    Invalid(#[from] Box<figment::Error>),
    /// The configuration parsed but is inconsistent.
    #[error("inconsistent configuration: {0}")]
    Inconsistent(String),
}

/// The full configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Database settings.
    pub database: DatabaseConfig,
    /// Vault settings.
    pub vault: VaultConfig,
    /// The person this instance works for.
    pub owner: OwnerConfig,
    /// Directories with agents and skills.
    #[serde(default)]
    pub paths: PathsConfig,
    /// Model providers by name.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Default routing by task name.
    pub routing: BTreeMap<String, RouteConfig>,
    /// MCP connectors by name.
    #[serde(default)]
    pub connectors: BTreeMap<String, ConnectorConfig>,
    /// Delivery channels.
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// HTTP server settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Socket address to bind.
    pub bind: String,
    /// Public base URL, used for OAuth redirects.
    pub public_url: String,
    /// IANA time zone for schedules.
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Token for admin endpoints. Prefer `LIGHTHOUSE_SERVER__ADMIN_TOKEN`.
    #[serde(default)]
    pub admin_token: Option<SecretString>,
}

fn default_timezone() -> String {
    "Europe/Berlin".to_owned()
}

/// Database settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Postgres connection URL.
    pub url: SecretString,
}

/// Vault settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    /// File holding the base64-encoded master key.
    #[serde(default)]
    pub master_key_file: Option<PathBuf>,
    /// Base64-encoded master key. Prefer the file or the environment.
    #[serde(default)]
    pub master_key: Option<SecretString>,
}

/// The person this instance works for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerConfig {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// Further account names by service, for example `linear` or `github`.
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,
}

/// Directories with agents and skills.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    /// Directory with agent files.
    #[serde(default = "default_agents_dir")]
    pub agents: PathBuf,
    /// Directory with skills.
    #[serde(default = "default_skills_dir")]
    pub skills: PathBuf,
}

fn default_agents_dir() -> PathBuf {
    PathBuf::from("agents")
}

fn default_skills_dir() -> PathBuf {
    PathBuf::from("skills")
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            agents: default_agents_dir(),
            skills: default_skills_dir(),
        }
    }
}

/// API family of a model provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// The Anthropic Messages API.
    Anthropic,
    /// The OpenAI Chat Completions API.
    Openai,
    /// A scripted provider for tests and dry runs.
    Mock,
}

/// A model provider.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// API family.
    pub kind: ProviderKind,
    /// Base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Models by task name.
    pub tasks: BTreeMap<String, ModelConfig>,
}

/// A model with its prices.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// Model name as the provider expects it.
    pub model: String,
    /// Price in EUR per million input tokens.
    pub input_eur_per_mtok: f64,
    /// Price in EUR per million output tokens.
    pub output_eur_per_mtok: f64,
}

/// Default routing for a task.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Primary provider.
    pub provider: String,
    /// Fallback provider.
    #[serde(default)]
    pub fallback: Option<String>,
}

/// How a connector authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorAuth {
    /// No authentication.
    #[default]
    None,
    /// A static bearer token from the vault, stored under `mcp:<connector>`.
    Bearer,
    /// The MCP OAuth flow, started with `lht connect <connector>`.
    Oauth,
}

/// How to reach an MCP server.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum ConnectorTransport {
    /// Streamable HTTP.
    Http {
        /// Server URL.
        url: String,
        /// Authentication.
        #[serde(default)]
        auth: ConnectorAuth,
        /// Pre-registered OAuth client ID, if the server has no dynamic registration.
        #[serde(default)]
        client_id: Option<String>,
        /// OAuth scopes to request.
        #[serde(default)]
        scopes: Vec<String>,
        /// Allow OAuth discovery on private addresses, for servers in your own network.
        #[serde(default)]
        allow_private_addresses: bool,
    },
    /// A local child process speaking stdio.
    Stdio {
        /// Program to start.
        command: String,
        /// Program arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables whose values come from vault entries of that name.
        #[serde(default)]
        secret_env: BTreeMap<String, String>,
    },
}

/// An MCP connector.
#[derive(Clone, Debug, Deserialize)]
pub struct ConnectorConfig {
    /// Transport settings.
    #[serde(flatten)]
    pub transport: ConnectorTransport,
    /// Allowlist of tools known to be read-only. Every other tool counts as `write`.
    #[serde(default)]
    pub read_tools: Vec<String>,
}

/// Delivery channels.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryConfig {
    /// SMTP settings. The password is stored in the vault as `smtp`.
    #[serde(default)]
    pub smtp: Option<SmtpConfig>,
    /// Slack aliases such as `#weekly`. Each webhook URL is stored in the vault
    /// as `slack:<alias>`.
    #[serde(default)]
    pub slack_channels: Vec<String>,
}

/// SMTP settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    /// SMTP host.
    pub host: String,
    /// SMTP port.
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// SMTP user.
    pub username: String,
    /// Sender address.
    pub from: String,
}

fn default_smtp_port() -> u16 {
    587
}

impl Config {
    /// Loads the configuration from a file and the environment.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_figment(
            Figment::new()
                .merge(Toml::file_exact(path))
                .merge(Env::prefixed("LIGHTHOUSE_").ignore(&["CONFIG"]).split("__")),
        )
    }

    fn from_figment(figment: Figment) -> Result<Self, ConfigError> {
        let config: Self = figment.extract().map_err(Box::new)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (task, route) in &self.routing {
            for name in std::iter::once(&route.provider).chain(&route.fallback) {
                let provider = self.providers.get(name).ok_or_else(|| {
                    ConfigError::Inconsistent(format!(
                        "routing for task `{task}` names unknown provider `{name}`"
                    ))
                })?;
                if !provider.tasks.contains_key(task) {
                    return Err(ConfigError::Inconsistent(format!(
                        "provider `{name}` has no model for task `{task}`"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;
    use secrecy::ExposeSecret;

    const EXAMPLE: &str = include_str!("../../../lighthouse.example.toml");

    fn with_config<T>(toml: &str, check: impl FnOnce(Result<Config, ConfigError>) -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lighthouse.toml");
        std::fs::write(&path, toml).unwrap();
        check(Config::load(&path))
    }

    #[test]
    fn example_configuration_is_valid() {
        with_config(EXAMPLE, |config| {
            let config = config.unwrap();
            assert_eq!(config.server.timezone, "Europe/Berlin");
            assert_eq!(config.paths.agents, PathBuf::from("agents"));
            assert!(config.routing.contains_key("text"));
            assert!(config.connectors.contains_key("linear"));
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn environment_overrides_the_file() {
        Jail::expect_with(|jail| {
            jail.create_file("lighthouse.toml", EXAMPLE)?;
            jail.set_env("LIGHTHOUSE_DATABASE__URL", "postgres://override");
            let config = Config::load(Path::new("lighthouse.toml")).unwrap();
            assert_eq!(config.database.url.expose_secret(), "postgres://override");
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn config_path_variable_is_not_a_setting() {
        Jail::expect_with(|jail| {
            jail.create_file("lighthouse.toml", EXAMPLE)?;
            jail.set_env("LIGHTHOUSE_CONFIG", "lighthouse.toml");
            Config::load(Path::new("lighthouse.toml")).unwrap();
            Ok(())
        });
    }

    #[test]
    fn unknown_route_provider_is_rejected() {
        let toml = EXAMPLE.replace(
            "[routing.text]\nprovider = \"anthropic\"",
            "[routing.text]\nprovider = \"nobody\"",
        );
        with_config(&toml, |config| {
            let error = config.unwrap_err().to_string();
            assert!(error.contains("unknown provider `nobody`"), "{error}");
        });
    }

    #[test]
    fn route_needs_a_model_for_its_task() {
        let toml = EXAMPLE.replace("[routing.text]", "[routing.image]");
        with_config(&toml, |config| {
            let error = config.unwrap_err().to_string();
            assert!(error.contains("has no model for task `image`"), "{error}");
        });
    }

    #[test]
    fn missing_file_is_an_error() {
        let error = Config::load(Path::new("/nonexistent/lighthouse.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)));
    }
}
