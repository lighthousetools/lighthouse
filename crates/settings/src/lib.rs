//! Settings in three layers.
//!
//! The **instance** layer is `lighthouse.toml`: the defaults every tenant
//! starts with, read-only through the API. The **tenant** layer holds what
//! owners and admins set for all members of a tenant. The **personal** layer
//! holds what each member sets for themselves in that tenant. The effective
//! value for a request is resolved per tenant and user: personal over tenant
//! over instance. Every effective entry names the layer it comes from.
//!
//! Keys and tokens are never stored here. They stay in the vault: the tenant's
//! for shared entries, the member's personal vault for personal ones. A layer
//! never decides where a lower layer's secret is sent: a provider keeps the
//! kind and URL of the layer that introduced its name, and secrets of a vault
//! only reach endpoints its own layer or a lower one chose.
//!
//! [`TenantSettings`] is the handle of one tenant. It keeps the stored layers
//! in memory, swaps them atomically after every change and when another
//! server process changed them, so runs started after a change see it and
//! runs already under way keep what they started with.

mod effective;
mod handle;
mod presets;
mod store;
mod validate;
mod values;

pub use effective::{
    Defaults, Effective, EffectiveConnector, EffectiveFile, EffectiveModel, EffectiveProvider,
    EffectiveRoute, EffectiveSource, InstanceApp,
};
pub use handle::{Options, TenantSettings, source_problem};
pub use presets::{PresetConnector, PresetModel, PresetProvider, PresetSource, Presets, presets};
pub use store::{ChangeRecord, SettingsStore};
pub use validate::{KNOWN_TASKS, is_name};
pub use values::{
    ACCOUNTS, ADDRESSES, BRAIN, LANGUAGE, NOTIFICATIONS, PUBLISHING, SLACK_CHANNELS, TALK_AGENT,
    TIMEZONE, VALUE_KEYS, WEATHER, check_value,
};

use lighthouse_config::{ConnectorAuth, ModelConfig, ProviderKind, RouteConfig};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use utoipa::ToSchema;
use uuid::Uuid;

/// Where a setting comes from.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// `lighthouse.toml`, the defaults of the instance.
    Instance,
    /// Set by an owner or admin for the whole tenant.
    Tenant,
    /// Set by a member for themselves.
    Personal,
}

impl Layer {
    /// The name used in the API and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Tenant => "tenant",
            Self::Personal => "personal",
        }
    }
}

/// Whose entries a change touches: the tenant's (`None`) or one member's.
pub type Owner = Option<Uuid>;

/// The layer of an owner.
pub fn layer_of(owner: Owner) -> Layer {
    if owner.is_some() {
        Layer::Personal
    } else {
        Layer::Tenant
    }
}

/// Errors of settings.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The input is invalid. The message is safe to show.
    #[error("{0}")]
    Invalid(String),
    /// The change conflicts with other entries. The message is safe to show.
    #[error("{0}")]
    Conflict(String),
    /// The entry does not exist.
    #[error("unknown {0}")]
    NotFound(String),
    /// A limit of the instance is reached.
    #[error("{0}")]
    Limit(String),
    /// The database failed.
    #[error("settings storage error: {0}")]
    Database(#[from] sqlx::Error),
}

/// What can be switched off for a layer without deleting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchKind {
    /// An MCP connector.
    Connector,
    /// An agent.
    Agent,
    /// An automation.
    Automation,
    /// A connected source.
    Source,
    /// A skill.
    Skill,
}

impl SwitchKind {
    /// The name used in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connector => "connector",
            Self::Agent => "agent",
            Self::Automation => "automation",
            Self::Source => "source",
            Self::Skill => "skill",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "connector" => Some(Self::Connector),
            "agent" => Some(Self::Agent),
            "automation" => Some(Self::Automation),
            "source" => Some(Self::Source),
            "skill" => Some(Self::Skill),
            _ => None,
        }
    }
}

/// A model provider of the tenant or personal layer.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderEntry {
    /// API family: `anthropic` or `openai`.
    pub kind: ProviderKind,
    /// Base URL, if not the provider's default.
    pub base_url: Option<String>,
    /// Models by task.
    pub models: BTreeMap<String, ModelConfig>,
}

/// A connector of the tenant or personal layer: an MCP server over streamable
/// HTTP, or a REST API with an OpenAPI description when `api` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorEntry {
    /// Server URL, or the URL of the OpenAPI description.
    pub url: String,
    /// Authentication.
    pub auth: ConnectorAuth,
    /// Pre-registered OAuth client ID.
    pub client_id: Option<String>,
    /// OAuth scopes.
    pub scopes: Vec<String>,
    /// Tools known to be read-only.
    pub read_tools: Vec<String>,
    /// Reversible tools without external effect.
    pub draft_tools: Vec<String>,
    /// Whether it is in use.
    pub enabled: bool,
    /// The REST API settings, for a connector that is not an MCP server.
    pub api: Option<RestApi>,
}

/// A REST API with an OpenAPI 3 description, used like an MCP server: every
/// operation becomes a tool named after its method and path, such as
/// `get_deals_id` for `GET /deals/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RestApi {
    /// Base URL of the API, if not the first server of the description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// How the key from the vault entry `mcp:<name>` is sent: `none`, `bearer`,
    /// `header` or `query`.
    #[serde(default = "no_key")]
    pub key: String,
    /// The header or query parameter for `header` and `query`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Only these operations, such as `get_deals`. Empty means every operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    /// Authorization endpoint for `auth = "oauth"`. The connector's `client_id` and `scopes` go with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    /// Token endpoint for `auth = "oauth"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
}

fn no_key() -> String {
    "none".to_owned()
}

/// The stored entries of one layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerData {
    /// Providers by name.
    pub providers: BTreeMap<String, ProviderEntry>,
    /// Routes by task.
    pub routes: BTreeMap<String, RouteConfig>,
    /// Connectors by name.
    pub connectors: BTreeMap<String, ConnectorEntry>,
    /// Entries of lower layers switched on or off, by kind and name.
    pub switches: BTreeMap<(SwitchKind, String), bool>,
    /// Single settings by key.
    pub values: BTreeMap<String, serde_json::Value>,
    /// Connected sources by name.
    pub sources: BTreeMap<String, SourceEntry>,
    /// Agents, automations and skills by kind and name.
    pub files: BTreeMap<(FileKind, String), FileEntry>,
}

/// What can be created in the apps as a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// An agent file.
    Agent,
    /// An automation file.
    Automation,
    /// A skill file.
    Skill,
}

impl FileKind {
    /// The name used in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Automation => "automation",
            Self::Skill => "skill",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "agent" => Some(Self::Agent),
            "automation" => Some(Self::Automation),
            "skill" => Some(Self::Skill),
            _ => None,
        }
    }

    pub(crate) fn switch(self) -> SwitchKind {
        match self {
            Self::Agent => SwitchKind::Agent,
            Self::Automation => SwitchKind::Automation,
            Self::Skill => SwitchKind::Skill,
        }
    }
}

/// An agent, automation or skill of the tenant or personal layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// The file.
    pub source: String,
    /// Whether it is in use.
    pub enabled: bool,
}

/// A connected source of the tenant or personal layer.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceEntry {
    /// The source as JSON, in the shape of `[sources.<name>]`.
    pub definition: serde_json::Value,
    /// Whether it refreshes.
    pub enabled: bool,
}

impl LayerData {
    /// Number of stored entries, for the limits.
    pub fn entries(&self) -> usize {
        self.providers.len()
            + self
                .providers
                .values()
                .map(|provider| provider.models.len())
                .sum::<usize>()
            + self.routes.len()
            + self.connectors.len()
            + self.switches.len()
            + self.values.len()
            + self.sources.len()
            + self.files.len()
    }
}

/// A member of the tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The user.
    pub id: Uuid,
    /// Their email address.
    pub email: String,
    /// Their name.
    pub name: String,
    /// Whether they are an owner.
    pub owner: bool,
}

/// Every stored layer of a tenant at one revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Raised by every change.
    pub revision: i64,
    /// The members, earliest first.
    pub members: Vec<Member>,
    /// The tenant layer.
    pub tenant: LayerData,
    /// The personal layers by member.
    pub personal: HashMap<Uuid, LayerData>,
}

impl Snapshot {
    /// The person agents work for in the tenant when no one else is named:
    /// its earliest owner.
    pub fn primary(&self) -> Option<&Member> {
        self.members.iter().find(|member| member.owner)
    }

    /// A member by user.
    pub fn member(&self, user: Uuid) -> Option<&Member> {
        self.members.iter().find(|member| member.id == user)
    }

    /// The stored entries of an owner.
    pub fn layer(&self, owner: Owner) -> Option<&LayerData> {
        match owner {
            None => Some(&self.tenant),
            Some(user) => self.personal.get(&user),
        }
    }

    /// Entries of all layers together.
    pub fn entries(&self) -> usize {
        self.tenant.entries()
            + self
                .personal
                .values()
                .map(LayerData::entries)
                .sum::<usize>()
    }
}

/// A provider to add or replace.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ProviderInput {
    /// `anthropic` or `openai`.
    pub kind: String,
    /// Base URL, for example of an OpenAI compatible service. Must be a public
    /// `https` URL unless the instance allows private endpoints.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// A model of a provider for a task, with its prices.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelInput {
    /// Model name as the provider expects it.
    pub model: String,
    /// Price in EUR per million input tokens.
    pub input_eur_per_mtok: f64,
    /// Price in EUR per million output tokens.
    pub output_eur_per_mtok: f64,
}

/// The route of a task.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RouteInput {
    /// Primary provider.
    pub provider: String,
    /// Fallback provider.
    #[serde(default)]
    pub fallback: Option<String>,
}

/// An MCP connector over streamable HTTP, or a REST API with an OpenAPI
/// description when `api` is set. Connectors that start a process on the
/// server exist only in `lighthouse.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ConnectorInput {
    /// Server URL, or the URL of the OpenAPI description when `api` is set.
    /// Must be a public `https` URL unless the instance allows private endpoints.
    pub url: String,
    /// `none`, `bearer` (token in the vault as `mcp:<name>`) or `oauth`.
    #[serde(default = "default_auth")]
    pub auth: String,
    /// Pre-registered OAuth client ID, if the server has no dynamic registration.
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth scopes to request.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Tools known to be read-only. Every other tool counts as `write`.
    #[serde(default)]
    pub read_tools: Vec<String>,
    /// Reversible tools without external effect, such as saving a draft.
    #[serde(default)]
    pub draft_tools: Vec<String>,
    /// Whether it is in use. Defaults to on.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Makes it a REST API connector. `auth` is then `none`, or `bearer` for a
    /// key in the vault as `mcp:<name>`.
    #[serde(default)]
    pub api: Option<RestApi>,
}

fn default_auth() -> String {
    "none".to_owned()
}

const fn enabled() -> bool {
    true
}

/// A connected source to add or replace.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SourceInput {
    /// The source in the shape of `[sources.<name>]`: `connector`, and `domain`,
    /// `preset`, `service`, `account`, `interval`, `collections`, `actions`.
    /// See `docs/connected.md`.
    pub definition: serde_json::Value,
    /// Whether it refreshes. Defaults to on.
    #[serde(default = "enabled")]
    pub enabled: bool,
}

/// Switches an entry on or off.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
pub struct SwitchInput {
    /// Whether it is in use.
    pub enabled: bool,
}
