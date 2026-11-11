//! Configuration of Lighthouse.
//!
//! Values come from `lighthouse.toml` and can be overridden by environment
//! variables with the prefix `LIGHTHOUSE_`, where `__` separates nesting levels.
//! Only variables that name a known section count, so other programs may use
//! the prefix for their own variables. Programs built on these crates keep
//! their own settings under `[extensions.<name>]`, which the core ignores.
//! Secrets such as API keys live in the vault, not here. The only exception is
//! the admin token, which should be set through the environment.

pub mod sources;

pub use sources::{
    ActionConfig, CollectionConfig, FieldMap, FieldRule, PublishingConfig, SourceConfig, TalkConfig,
};

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use secrecy::SecretString;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
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
    /// Second Brain.
    #[serde(default)]
    pub brain: BrainConfig,
    /// Pushes to the owner about decisions, finished runs and alerts.
    #[serde(default)]
    pub notifications: NotificationsConfig,
    /// Sources of mail, calendar, issues and pull requests, by name.
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
    /// The tool that publishes posts, if any.
    #[serde(default)]
    pub publishing: Option<PublishingConfig>,
    /// Conversations in the apps.
    #[serde(default)]
    pub talk: TalkConfig,
    /// Where the owner is, for the weather in the Morning Brief.
    #[serde(default)]
    pub weather: Option<WeatherConfig>,
    /// Users, tenants and sign-up.
    #[serde(default)]
    pub tenancy: TenancyConfig,
    /// Settings of programs built on these crates, by program name. The core
    /// ignores them; see [`Config::extension`].
    #[serde(default)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// The top-level sections of [`Config`]. Environment variables whose first
/// segment names none of them are ignored.
const SECTIONS: &[&str] = &[
    "server",
    "database",
    "vault",
    "owner",
    "paths",
    "providers",
    "routing",
    "connectors",
    "delivery",
    "brain",
    "notifications",
    "sources",
    "publishing",
    "talk",
    "weather",
    "tenancy",
    "extensions",
];

/// Who may create an account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignupMode {
    /// Nobody. The configured owner signs in, as in a single-user install.
    #[default]
    Closed,
    /// Only people with an invitation from an owner or admin.
    Invite,
    /// Anyone, who then owns a new tenant. Invitations work too.
    Open,
}

/// Users, tenants and sign-up.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenancyConfig {
    /// Who may create an account.
    #[serde(default)]
    pub signup: SignupMode,
    /// Whether tenants without their own model provider key may use the keys
    /// of the instance, stored in the vault of the default tenant. Hosted
    /// instances turn this off so every tenant brings its own keys.
    #[serde(default = "enabled")]
    pub allow_instance_keys: bool,
    /// Directory with one subdirectory per tenant for its Brain and the agents
    /// and automations created in the apps. The default tenant keeps the
    /// directories of `[paths]` and `[brain]`.
    #[serde(default = "default_tenant_dir")]
    pub data_dir: PathBuf,
    /// Whether people who signed up themselves must confirm their email
    /// address before they sign in. Needs a mailer. The configured owner and
    /// invited people are not affected.
    #[serde(default)]
    pub require_verified_email: bool,
    /// Whether providers and connectors that tenants and people add in the
    /// apps may use private, loopback or link-local addresses and plain HTTP.
    /// Keep it off on a hosted instance; a self-hosted one in a home or office
    /// network may turn it on.
    #[serde(default)]
    pub allow_private_endpoints: bool,
    /// How many providers, models and connectors a tenant and each of its
    /// members may add.
    #[serde(default)]
    pub limits: SettingsLimits,
}

/// Limits of what a tenant and each of its members may add in the apps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsLimits {
    /// Model providers per tenant, and again per person.
    #[serde(default = "default_provider_limit")]
    pub providers: usize,
    /// Models per provider.
    #[serde(default = "default_model_limit")]
    pub models: usize,
    /// MCP connectors per tenant, and again per person.
    #[serde(default = "default_connector_limit")]
    pub connectors: usize,
    /// Connected sources per tenant, and again per person.
    #[serde(default = "default_source_limit")]
    pub sources: usize,
    /// Agents, automations and skills per tenant, and again per person.
    #[serde(default = "default_file_limit")]
    pub files: usize,
    /// Entries of all layers of one tenant together: providers, models,
    /// routes, connectors, sources, files and settings of the tenant and all
    /// its members.
    #[serde(default = "default_tenant_entry_limit")]
    pub tenant_entries: usize,
}

const fn default_provider_limit() -> usize {
    10
}

const fn default_model_limit() -> usize {
    20
}

const fn default_connector_limit() -> usize {
    25
}

const fn default_source_limit() -> usize {
    10
}

const fn default_file_limit() -> usize {
    200
}

const fn default_tenant_entry_limit() -> usize {
    2000
}

impl Default for SettingsLimits {
    fn default() -> Self {
        Self {
            providers: default_provider_limit(),
            models: default_model_limit(),
            connectors: default_connector_limit(),
            sources: default_source_limit(),
            files: default_file_limit(),
            tenant_entries: default_tenant_entry_limit(),
        }
    }
}

fn default_tenant_dir() -> PathBuf {
    PathBuf::from("tenants")
}

impl Default for TenancyConfig {
    fn default() -> Self {
        Self {
            signup: SignupMode::Closed,
            allow_instance_keys: true,
            data_dir: default_tenant_dir(),
            require_verified_email: false,
            allow_private_endpoints: false,
            limits: SettingsLimits::default(),
        }
    }
}

/// A place for the weather, from Open-Meteo, which needs no key.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeatherConfig {
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
    /// Name of the place, shown with the weather.
    #[serde(default)]
    pub location: Option<String>,
    /// Another Open-Meteo compatible server, for tests or self-hosting.
    #[serde(default)]
    pub base_url: Option<String>,
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
    /// Whether the first address in `X-Forwarded-For` is the client. Turn it on
    /// only behind a reverse proxy that sets the header, since rate limits of
    /// sign-in and sign-up count per client address.
    #[serde(default)]
    pub trust_forwarded_for: bool,
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
    /// Writable directory for agents created in the apps.
    #[serde(default = "default_created_agents_dir")]
    pub created_agents: PathBuf,
    /// Directory with automation files.
    #[serde(default = "default_automations_dir")]
    pub automations: PathBuf,
    /// Writable directory for automations created in the apps.
    #[serde(default = "default_created_automations_dir")]
    pub created_automations: PathBuf,
    /// Directory with the built web app. Without it, the server only offers the API.
    #[serde(default)]
    pub web: Option<PathBuf>,
}

fn default_automations_dir() -> PathBuf {
    PathBuf::from("automations")
}

fn default_created_automations_dir() -> PathBuf {
    PathBuf::from("automations-created")
}

fn default_created_agents_dir() -> PathBuf {
    PathBuf::from("agents-created")
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
            created_agents: default_created_agents_dir(),
            automations: default_automations_dir(),
            created_automations: default_created_automations_dir(),
            web: None,
        }
    }
}

/// API family of a model provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// The Anthropic Messages API.
    Anthropic,
    /// The OpenAI Chat Completions API.
    Openai,
    /// A scripted provider for tests and dry runs.
    Mock,
    /// Plausible answers without a model, for the local demo.
    Demo,
}

/// A model provider.
#[derive(Clone, Debug, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Primary provider.
    pub provider: String,
    /// Fallback provider.
    #[serde(default)]
    pub fallback: Option<String>,
}

/// How a connector authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
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

/// How an OpenAPI connector sends its key.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyPlacement {
    /// No key.
    #[default]
    None,
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// A header named by `key_name`.
    Header,
    /// A query parameter named by `key_name`.
    Query,
}

/// The OAuth authorization code flow of a REST API, with fixed endpoints instead of discovery.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpenApiOAuth {
    /// Authorization endpoint, such as `https://login.example.com/oauth2/authorize`.
    pub authorize_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Registered client ID. A client secret, if the server needs one, is the vault entry `oauth-client:<connector>`.
    pub client_id: String,
    /// Scopes to request. Ask for a refresh token where the server needs a scope for it, such as `offline_access`.
    #[serde(default)]
    pub scopes: Vec<String>,
}

fn reads_are_safe() -> bool {
    true
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
        /// Extra request headers with values that are not secret, such as a tenant ID. Secrets belong in the vault:
        /// a bearer token under `mcp:<connector>`, an OAuth client secret under `oauth-client:<connector>`.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// A REST API with an OpenAPI 3 description, turned into tools by Lighthouse.
    #[serde(rename = "openapi")]
    OpenApi {
        /// URL or file path of the description, in JSON or YAML.
        spec: String,
        /// Base URL of the API, if not the first server of the description.
        #[serde(default)]
        base_url: Option<String>,
        /// How the key from the vault entry `mcp:<connector>` is sent.
        #[serde(default)]
        key: ApiKeyPlacement,
        /// Name of the header or query parameter for `key = "header"` or `key = "query"`.
        #[serde(default)]
        key_name: Option<String>,
        /// Extra headers that are not secret.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Only these tools, such as `get_deals`. Empty means every operation.
        #[serde(default)]
        operations: Vec<String>,
        /// Whether `GET` and `HEAD` operations count as read-only without listing them.
        #[serde(default = "reads_are_safe")]
        reads_are_safe: bool,
        /// Signs in with OAuth instead of a key. The access token is sent as a bearer token.
        #[serde(default)]
        oauth: Option<OpenApiOAuth>,
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
    /// Allowlist of reversible tools without external effect, such as saving a draft.
    #[serde(default)]
    pub draft_tools: Vec<String>,
}

/// Second Brain settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrainConfig {
    /// Directory with the Markdown files.
    #[serde(default = "default_brain_dir")]
    pub path: PathBuf,
    /// Provider for embeddings, images and audio. It needs an OpenAI-compatible
    /// API and uses the key of that provider. Without it, search is lexical and
    /// images and audio are refused.
    #[serde(default)]
    pub provider: Option<String>,
    /// Embedding model.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Vision model for images.
    #[serde(default)]
    pub vision_model: Option<String>,
    /// Transcription model for audio.
    #[serde(default)]
    pub transcription_model: Option<String>,
}

fn default_brain_dir() -> PathBuf {
    PathBuf::from("brain")
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            path: default_brain_dir(),
            provider: None,
            embedding_model: None,
            vision_model: None,
            transcription_model: None,
        }
    }
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

/// Pushes to the owner. The apps always get live events; these settings only
/// decide what is also sent through the delivery channels.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Targets like in agent files: `owner`, `mail:<address>` or `slack:<alias>`.
    /// Without targets, nothing is sent.
    #[serde(default)]
    pub deliver: Vec<String>,
    /// Push when a run waits for a decision.
    #[serde(default = "enabled")]
    pub decisions: bool,
    /// Push when a run finished.
    #[serde(default)]
    pub done: bool,
    /// Lowest severity of new alerts to push: `info`, `warning` or `critical`.
    #[serde(default = "default_alert_severity")]
    pub alerts: String,
}

fn enabled() -> bool {
    true
}

fn default_alert_severity() -> String {
    "critical".to_owned()
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            deliver: Vec::new(),
            decisions: true,
            done: false,
            alerts: default_alert_severity(),
        }
    }
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

/// Whether an environment key, with `.` between its levels, belongs to a
/// known section.
fn is_section_key(key: &str) -> bool {
    key.split_once('.').is_some_and(|(section, _)| {
        SECTIONS
            .iter()
            .any(|known| section.eq_ignore_ascii_case(known))
    })
}

impl Config {
    /// Loads the configuration from a file and the environment.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::from_figment(
            Figment::new().merge(Toml::file_exact(path)).merge(
                Env::prefixed("LIGHTHOUSE_")
                    .split("__")
                    .filter(|key| is_section_key(key.as_str())),
            ),
        )
    }

    /// The settings under `[extensions.<name>]`, read into the program's own
    /// type. `None` if the table is missing.
    pub fn extension<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, ConfigError> {
        self.extensions
            .get(name)
            .map(|value| {
                T::deserialize(value).map_err(|error| {
                    ConfigError::Inconsistent(format!("extensions.{name}: {error}"))
                })
            })
            .transpose()
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
        for (name, source) in &self.sources {
            if !self.connectors.contains_key(&source.connector) {
                return Err(ConfigError::Inconsistent(format!(
                    "source `{name}` names unknown connector `{}`",
                    source.connector
                )));
            }
        }
        if let Some(publishing) = &self.publishing
            && !self.connectors.contains_key(&publishing.connector)
        {
            return Err(ConfigError::Inconsistent(format!(
                "publishing names unknown connector `{}`",
                publishing.connector
            )));
        }
        if !["info", "warning", "critical"].contains(&self.notifications.alerts.as_str()) {
            return Err(ConfigError::Inconsistent(format!(
                "notifications.alerts is `{}`, not info, warning or critical",
                self.notifications.alerts
            )));
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
            assert_eq!(config.paths.automations, PathBuf::from("automations"));
            assert!(config.routing.contains_key("text"));
            assert!(config.connectors.contains_key("linear"));
            assert_eq!(config.notifications.deliver, ["owner"]);
            assert!(config.notifications.decisions);
        });
    }

    #[test]
    fn sources_map_fields_and_need_a_known_connector() {
        let with_source = format!(
            "{EXAMPLE}\n[sources.mail]\nconnector = \"linear\"\ndomain = \"mail\"\n\
             [sources.mail.collections.inbox]\ntool = \"list_threads\"\nitems = \"threads\"\n\
             kind = \"thread\"\ninput = {{ query = \"after:{{{{since_date}}}}\" }}\n\
             [sources.mail.collections.inbox.fields]\nexternal_id = \"id\"\n\
             important = {{ path = \"labels\", contains = \"IMPORTANT\" }}\n"
        );
        with_config(&with_source, |config| {
            let config = config.unwrap();
            assert_eq!(config.sources["github"].preset.as_deref(), Some("github"));
            let inbox = &config.sources["mail"].collections["inbox"];
            assert_eq!(inbox.fields["external_id"], FieldMap::Path("id".to_owned()));
            let FieldMap::Rule(rule) = &inbox.fields["important"] else {
                panic!("not a rule")
            };
            assert_eq!(rule.contains.as_deref(), Some("IMPORTANT"));
            assert_eq!(
                inbox.input.as_ref().unwrap()["query"],
                "after:{{since_date}}"
            );
        });
        let unknown = format!("{EXAMPLE}\n[sources.mail]\nconnector = \"nowhere\"\n");
        with_config(&unknown, |config| {
            assert!(matches!(config, Err(ConfigError::Inconsistent(_))));
        });
    }

    #[test]
    fn notifications_default_to_silence_and_need_a_known_severity() {
        let without = EXAMPLE.replace("[notifications]", "[unused]");
        let without = without.split("[unused]").next().unwrap().to_owned();
        with_config(&without, |config| {
            let notifications = config.unwrap().notifications;
            assert!(notifications.deliver.is_empty());
            assert!(!notifications.done);
            assert_eq!(notifications.alerts, "critical");
        });
        let wrong = format!("{without}\n[notifications]\nalerts = \"loud\"\n");
        with_config(&wrong, |config| {
            assert!(matches!(config, Err(ConfigError::Inconsistent(_))));
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
    #[allow(clippy::result_large_err)]
    fn variables_of_other_programs_are_ignored() {
        Jail::expect_with(|jail| {
            jail.create_file("lighthouse.toml", EXAMPLE)?;
            jail.set_env("LIGHTHOUSE_OTHER_KEY", "x");
            jail.set_env("LIGHTHOUSE_SOMETHING__ELSE", "y");
            jail.set_env("LIGHTHOUSE_EXTENSIONS__ACME__LIMIT", "7");
            let config = Config::load(Path::new("lighthouse.toml")).unwrap();
            assert_eq!(config.extensions["acme"]["limit"], 7);
            Ok(())
        });
    }

    #[test]
    fn extensions_are_kept_for_other_programs() {
        #[derive(Deserialize)]
        struct Acme {
            limit: u32,
            #[serde(default)]
            names: Vec<String>,
        }
        with_config(EXAMPLE, |config| {
            let config = config.unwrap();
            assert!(config.extensions.is_empty());
            assert!(config.extension::<Acme>("acme").unwrap().is_none());
        });
        let with = format!(
            "{EXAMPLE}\n[extensions.acme]\nlimit = 3\nnames = [\"a\"]\n\
             [extensions.other.nested]\nanything = true\n"
        );
        with_config(&with, |config| {
            let config = config.unwrap();
            let acme: Acme = config.extension("acme").unwrap().unwrap();
            assert_eq!((acme.limit, acme.names.len()), (3, 1));
            assert_eq!(config.extensions["other"]["nested"]["anything"], true);
            let error = config.extension::<Acme>("other").err().unwrap();
            assert!(error.to_string().contains("extensions.other"), "{error}");
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
    fn tenancy_defaults_keep_a_single_user_install_closed() {
        with_config(EXAMPLE, |config| {
            let config = config.unwrap();
            assert_eq!(config.tenancy.signup, SignupMode::Closed);
            assert!(config.tenancy.allow_instance_keys);
            assert_eq!(config.tenancy.data_dir, PathBuf::from("tenants"));
            assert!(!config.tenancy.require_verified_email);
            assert!(!config.server.trust_forwarded_for);
            assert!(!config.tenancy.allow_private_endpoints);
            assert_eq!(config.tenancy.limits, SettingsLimits::default());
        });
        let hosted = format!(
            "{EXAMPLE}\n[tenancy]\nsignup = \"open\"\nallow_instance_keys = false\n\
             data_dir = \"/var/lib/lighthouse/tenants\"\nrequire_verified_email = true\n\
             allow_private_endpoints = true\n[tenancy.limits]\nconnectors = 3\n"
        );
        with_config(&hosted, |config| {
            let tenancy = config.unwrap().tenancy;
            assert!(tenancy.require_verified_email);
            assert_eq!(tenancy.signup, SignupMode::Open);
            assert!(!tenancy.allow_instance_keys);
            assert!(tenancy.allow_private_endpoints);
            assert_eq!(
                (tenancy.limits.connectors, tenancy.limits.providers),
                (3, 10)
            );
        });
        let wrong = format!("{EXAMPLE}\n[tenancy]\nsignup = \"everyone\"\n");
        with_config(&wrong, |config| assert!(config.is_err()));
    }

    #[test]
    fn missing_file_is_an_error() {
        let error = Config::load(Path::new("/nonexistent/lighthouse.toml")).unwrap_err();
        assert!(matches!(error, ConfigError::Invalid(_)));
    }
}
