//! The effective settings of one user in one tenant.

use crate::{ConnectorEntry, FileKind, Layer, LayerData, SwitchKind};
use lighthouse_config::{
    ApiKeyPlacement, Config, ConnectorConfig, ConnectorTransport, ModelConfig, ProviderConfig,
    ProviderKind, RouteConfig, SourceConfig,
};
use lighthouse_router::{PlannedProvider, RouterPlan};
use secrecy::SecretString;
use std::collections::BTreeMap;

/// The instance layer: what `lighthouse.toml` says.
#[derive(Clone, Debug, Default)]
pub struct Defaults {
    /// Providers by name.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Routes by task.
    pub routing: BTreeMap<String, RouteConfig>,
    /// Connectors by name.
    pub connectors: BTreeMap<String, ConnectorConfig>,
    /// Single settings by key, as JSON.
    pub values: BTreeMap<String, serde_json::Value>,
    /// Connected sources by name.
    pub sources: BTreeMap<String, SourceConfig>,
    /// OAuth apps of the instance.
    pub oauth_apps: Vec<InstanceApp>,
}

/// An OAuth app the instance registered once for every tenant.
#[derive(Clone, Debug)]
pub struct InstanceApp {
    /// The MCP server URL or REST token URL it is for.
    pub url: String,
    /// Its client ID.
    pub client_id: String,
    /// Its client secret, if it has one.
    pub client_secret: Option<SecretString>,
}

/// Two URLs that name the same endpoint, ignoring a trailing slash.
fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

impl Defaults {
    /// The app of the instance for a connector that reaches `url`, if there is one.
    pub fn app_for(&self, url: &str) -> Option<&InstanceApp> {
        self.oauth_apps.iter().find(|app| same_url(&app.url, url))
    }

    /// The defaults of a configuration file.
    pub fn from_config(config: &Config) -> Self {
        Self {
            providers: config.providers.clone(),
            routing: config.routing.clone(),
            connectors: config.connectors.clone(),
            values: crate::values::instance_values(config),
            sources: config.sources.clone(),
            oauth_apps: config
                .oauth_apps
                .values()
                .filter_map(|app| {
                    Some(InstanceApp {
                        url: app.url.clone()?,
                        client_id: app.client_id.clone()?,
                        client_secret: app.client_secret.clone(),
                    })
                })
                .collect(),
        }
    }

    /// Vault entries that connectors of the instance read: their tokens and
    /// the entries their processes get. A tenant connector must not take a
    /// name whose tokens would be one of them.
    pub fn reserved_vault_entries(&self) -> Vec<String> {
        let mut entries = Vec::new();
        for (name, connector) in &self.connectors {
            entries.push(format!("mcp:{name}"));
            entries.push(format!("oauth:{name}"));
            if let ConnectorTransport::Stdio { secret_env, .. } = &connector.transport {
                entries.extend(secret_env.values().cloned());
            }
        }
        entries
    }
}

/// A model with the layer it comes from.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveModel {
    /// The model and its prices.
    pub config: ModelConfig,
    /// Its layer.
    pub source: Layer,
}

/// A provider as it applies to one user.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveProvider {
    /// API family.
    pub kind: ProviderKind,
    /// Base URL override.
    pub base_url: Option<String>,
    /// The layer that introduced the name and chose kind and URL.
    pub defined_at: Layer,
    /// The highest layer with an entry for it.
    pub source: Layer,
    /// Models by task.
    pub models: BTreeMap<String, EffectiveModel>,
    guarded: bool,
}

impl EffectiveProvider {
    /// The vault entry of its key. Providers of the instance keep their name,
    /// as in earlier versions. Providers added in the apps use
    /// `provider:<name>`, so they never read another entry of the vault.
    pub fn key_name(&self, name: &str) -> String {
        if self.defined_at == Layer::Instance {
            name.to_owned()
        } else {
            format!("provider:{name}")
        }
    }

    /// The vaults its key is read from, the first that has it wins: the
    /// user's own, then the tenant's, then the instance's. A vault is only
    /// read if its layer or a lower one chose the endpoint.
    ///
    /// A member's own key never goes to a URL an owner or admin of the tenant
    /// chose and can change later, or they could collect it. It is only used
    /// for the provider's public endpoint.
    pub fn key_layers(&self) -> &'static [Layer] {
        match (self.defined_at, &self.base_url) {
            (Layer::Instance, _) => &[Layer::Personal, Layer::Tenant, Layer::Instance],
            (Layer::Tenant, None) => &[Layer::Personal, Layer::Tenant],
            (Layer::Tenant, Some(_)) => &[Layer::Tenant],
            (Layer::Personal, _) => &[Layer::Personal],
        }
    }

    /// Whether requests must go through the guarded client.
    pub fn guarded(&self) -> bool {
        self.guarded
    }
}

/// A route with the layer it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveRoute {
    /// Primary and fallback provider.
    pub route: RouteConfig,
    /// Its layer.
    pub source: Layer,
}

/// A connector as it applies to one user.
#[derive(Clone, Debug)]
pub struct EffectiveConnector {
    /// Its settings.
    pub config: ConnectorConfig,
    /// The layer that defines it.
    pub source: Layer,
    /// Whether it is in use for this user.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
    /// Whether it signs in with an OAuth app of the instance.
    pub instance_app: bool,
}

impl EffectiveConnector {
    /// The vaults its tokens are read from, the first that has one wins: the
    /// user's own, then the tenant's. A personal connector only uses the
    /// user's own vault.
    pub fn token_layers(&self) -> &'static [Layer] {
        match self.source {
            Layer::Instance | Layer::Tenant => &[Layer::Personal, Layer::Tenant],
            Layer::Personal => &[Layer::Personal],
        }
    }

    /// Whether connections must go through the guarded client.
    pub fn guarded(&self) -> bool {
        match &self.config.transport {
            ConnectorTransport::Http {
                allow_private_addresses,
                ..
            } => self.source != Layer::Instance && !allow_private_addresses,
            ConnectorTransport::OpenApi { .. } => self.source != Layer::Instance,
            ConnectorTransport::Stdio { .. } => false,
        }
    }
}

/// A connected source as it applies to one user.
#[derive(Clone, Debug)]
pub struct EffectiveSource {
    /// Its name.
    pub name: String,
    /// What items and statuses are stored under, unique in the tenant.
    pub key: String,
    /// Its settings.
    pub config: SourceConfig,
    /// Its settings as stored, absent for one of the instance.
    pub definition: Option<serde_json::Value>,
    /// The layer that defines it.
    pub source: Layer,
    /// The member it belongs to, for a personal one.
    pub owner: Option<uuid::Uuid>,
    /// Whether it refreshes.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
}

/// An agent, automation or skill created in the apps, as it applies to one user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveFile {
    /// The file.
    pub source: String,
    /// The layer that defines it.
    pub layer: Layer,
    /// Whether it is in use for this user.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
}

/// Everything that applies to one user in one tenant.
#[derive(Clone, Debug, Default)]
pub struct Effective {
    /// Providers by name.
    pub providers: BTreeMap<String, EffectiveProvider>,
    /// Routes by task.
    pub routing: BTreeMap<String, EffectiveRoute>,
    /// Connectors by name, including the ones switched off.
    pub connectors: BTreeMap<String, EffectiveConnector>,
    /// Single settings by key.
    pub values: BTreeMap<String, (serde_json::Value, Layer)>,
    /// Connected sources by name, including the ones switched off.
    pub sources: BTreeMap<String, EffectiveSource>,
    /// Agents, automations and skills created in the apps, by kind and name.
    pub files: BTreeMap<(FileKind, String), EffectiveFile>,
    /// Built-in agents, automations and skills that a layer switched off.
    pub files_off: BTreeMap<(FileKind, String), Layer>,
}

/// A connector of the tenant or personal layer in the shape of the configuration.
pub(crate) fn connector_config(entry: &ConnectorEntry, allow_private: bool) -> ConnectorConfig {
    let transport = match &entry.api {
        Some(api) => ConnectorTransport::OpenApi {
            spec: entry.url.clone(),
            base_url: api.base_url.clone(),
            key: key_placement(&api.key),
            key_name: api.key_name.clone(),
            headers: Default::default(),
            operations: api.operations.clone(),
            reads_are_safe: true,
            // Without a client ID of its own, the instance's app may fill it in; see `use_instance_app`.
            oauth: match (&api.authorize_url, &api.token_url) {
                (Some(authorize_url), Some(token_url))
                    if entry.auth == lighthouse_config::ConnectorAuth::Oauth =>
                {
                    Some(lighthouse_config::OpenApiOAuth {
                        authorize_url: authorize_url.clone(),
                        token_url: token_url.clone(),
                        client_id: entry.client_id.clone().unwrap_or_default(),
                        scopes: entry.scopes.clone(),
                    })
                }
                _ => None,
            },
        },
        None => ConnectorTransport::Http {
            url: entry.url.clone(),
            auth: entry.auth,
            client_id: entry.client_id.clone(),
            scopes: entry.scopes.clone(),
            allow_private_addresses: allow_private,
            headers: Default::default(),
        },
    };
    ConnectorConfig {
        transport,
        read_tools: entry.read_tools.clone(),
        draft_tools: entry.draft_tools.clone(),
        app_secret: None,
    }
}

/// Signs a connector without a client of its own in with the instance's app for its URL. An MCP server matches by
/// its URL, a REST API by its token URL, so the secret only goes where the app was registered. A REST API with OAuth
/// that has no client ID afterwards loses its OAuth settings, since it cannot sign in. Whether an app was used.
fn use_instance_app(config: &mut ConnectorConfig, defaults: &Defaults) -> bool {
    let app = match &mut config.transport {
        ConnectorTransport::Http {
            url,
            auth: lighthouse_config::ConnectorAuth::Oauth,
            client_id,
            ..
        } if client_id.as_deref().is_none_or(|id| id.trim().is_empty()) => {
            let Some(app) = defaults.app_for(url) else {
                return false;
            };
            *client_id = Some(app.client_id.clone());
            app
        }
        ConnectorTransport::OpenApi { oauth, .. } => {
            let Some(settings) = oauth else {
                return false;
            };
            if !settings.client_id.trim().is_empty() {
                return false;
            }
            let Some(app) = defaults.app_for(&settings.token_url) else {
                *oauth = None;
                return false;
            };
            settings.client_id = app.client_id.clone();
            app
        }
        _ => return false,
    };
    config.app_secret = app.client_secret.clone();
    true
}

/// How a REST API connector sends its key. Unknown values send none.
fn key_placement(key: &str) -> ApiKeyPlacement {
    match key {
        "bearer" => ApiKeyPlacement::Bearer,
        "header" => ApiKeyPlacement::Header,
        "query" => ApiKeyPlacement::Query,
        _ => ApiKeyPlacement::None,
    }
}

impl Effective {
    /// Merges the layers: personal over tenant over instance.
    pub fn resolve(
        defaults: &Defaults,
        tenant: Option<&LayerData>,
        personal: Option<&LayerData>,
        allow_private: bool,
    ) -> Self {
        let mut effective = Self::default();
        for (name, provider) in &defaults.providers {
            effective.providers.insert(
                name.clone(),
                EffectiveProvider {
                    kind: provider.kind,
                    base_url: provider.base_url.clone(),
                    defined_at: Layer::Instance,
                    source: Layer::Instance,
                    models: provider
                        .tasks
                        .iter()
                        .map(|(task, model)| {
                            let model = EffectiveModel {
                                config: model.clone(),
                                source: Layer::Instance,
                            };
                            (task.clone(), model)
                        })
                        .collect(),
                    guarded: false,
                },
            );
        }
        for (task, route) in &defaults.routing {
            effective.routing.insert(
                task.clone(),
                EffectiveRoute {
                    route: route.clone(),
                    source: Layer::Instance,
                },
            );
        }
        for (name, connector) in &defaults.connectors {
            effective.connectors.insert(
                name.clone(),
                EffectiveConnector {
                    config: connector.clone(),
                    source: Layer::Instance,
                    enabled: true,
                    disabled_by: None,
                    instance_app: false,
                },
            );
        }
        for (key, value) in &defaults.values {
            effective
                .values
                .insert(key.clone(), (value.clone(), Layer::Instance));
        }
        for (name, config) in &defaults.sources {
            effective.sources.insert(
                name.clone(),
                EffectiveSource {
                    name: name.clone(),
                    key: name.clone(),
                    config: config.clone(),
                    definition: None,
                    source: Layer::Instance,
                    owner: None,
                    enabled: true,
                    disabled_by: None,
                },
            );
        }
        for (layer, data) in [(Layer::Tenant, tenant), (Layer::Personal, personal)] {
            if let Some(data) = data {
                effective.apply(layer, data, allow_private);
            }
        }
        for connector in effective.connectors.values_mut() {
            connector.instance_app = use_instance_app(&mut connector.config, defaults);
        }
        effective
    }

    fn apply(&mut self, layer: Layer, data: &LayerData, allow_private: bool) {
        for (name, entry) in &data.providers {
            let provider =
                self.providers
                    .entry(name.clone())
                    .or_insert_with(|| EffectiveProvider {
                        kind: entry.kind,
                        base_url: entry.base_url.clone(),
                        defined_at: layer,
                        source: layer,
                        models: BTreeMap::new(),
                        guarded: entry.base_url.is_some() && !allow_private,
                    });
            // A higher layer never changes where a lower layer's key goes;
            // validation refuses such entries, and stored ones from before
            // are ignored here.
            if provider.kind != entry.kind || provider.base_url != entry.base_url {
                continue;
            }
            provider.source = layer;
            for (task, model) in &entry.models {
                provider.models.insert(
                    task.clone(),
                    EffectiveModel {
                        config: model.clone(),
                        source: layer,
                    },
                );
            }
        }
        for (task, route) in &data.routes {
            self.routing.insert(
                task.clone(),
                EffectiveRoute {
                    route: route.clone(),
                    source: layer,
                },
            );
        }
        for ((kind, name), enabled) in &data.switches {
            if *kind != SwitchKind::Connector || *enabled {
                continue;
            }
            if let Some(connector) = self.connectors.get_mut(name)
                && connector.enabled
            {
                connector.enabled = false;
                connector.disabled_by = Some(layer);
            }
        }
        for ((kind, name), enabled) in &data.switches {
            if *kind != SwitchKind::Source || *enabled {
                continue;
            }
            if let Some(source) = self.sources.get_mut(name)
                && source.enabled
            {
                source.enabled = false;
                source.disabled_by = Some(layer);
            }
        }
        for (name, entry) in &data.sources {
            let Ok(config) = serde_json::from_value::<SourceConfig>(entry.definition.clone())
            else {
                tracing::warn!(source = %name, "cannot read a stored source");
                continue;
            };
            self.sources.insert(
                name.clone(),
                EffectiveSource {
                    name: name.clone(),
                    key: name.clone(),
                    config,
                    definition: Some(entry.definition.clone()),
                    source: layer,
                    owner: None,
                    enabled: entry.enabled,
                    disabled_by: (!entry.enabled).then_some(layer),
                },
            );
        }
        for (key, entry) in &data.files {
            self.files.insert(
                key.clone(),
                EffectiveFile {
                    source: entry.source.clone(),
                    layer,
                    enabled: entry.enabled,
                    disabled_by: (!entry.enabled).then_some(layer),
                },
            );
        }
        for ((kind, name), enabled) in &data.switches {
            let kind = match kind {
                SwitchKind::Agent => FileKind::Agent,
                SwitchKind::Automation => FileKind::Automation,
                SwitchKind::Skill => FileKind::Skill,
                SwitchKind::Connector | SwitchKind::Source => continue,
            };
            if *enabled {
                continue;
            }
            let key = (kind, name.clone());
            match self.files.get_mut(&key) {
                Some(file) if file.layer == layer => {}
                Some(file) => {
                    if file.enabled {
                        file.enabled = false;
                        file.disabled_by = Some(layer);
                    }
                }
                None => {
                    self.files_off.entry(key).or_insert(layer);
                }
            }
        }
        for (name, entry) in &data.connectors {
            self.connectors.insert(
                name.clone(),
                EffectiveConnector {
                    config: connector_config(entry, allow_private),
                    source: layer,
                    enabled: entry.enabled,
                    disabled_by: (!entry.enabled).then_some(layer),
                    instance_app: false,
                },
            );
        }
        for (key, value) in &data.values {
            self.values.insert(key.clone(), (value.clone(), layer));
        }
    }

    /// Why the route of a task cannot work, if it cannot.
    pub fn route_problem(&self, task: &str) -> Option<String> {
        let route = &self.routing.get(task)?.route;
        for name in std::iter::once(&route.provider).chain(&route.fallback) {
            match self.providers.get(name) {
                None => return Some(format!("provider `{name}` does not exist")),
                Some(provider) if !provider.models.contains_key(task) => {
                    return Some(format!("provider `{name}` has no model for `{task}`"));
                }
                Some(_) => {}
            }
        }
        None
    }

    /// The model a provider uses for a task.
    pub fn model(&self, provider: &str, task: &str) -> Option<&ModelConfig> {
        self.providers
            .get(provider)?
            .models
            .get(task)
            .map(|model| &model.config)
    }

    /// The providers and routes a router is built from.
    pub fn router_plan(&self) -> RouterPlan {
        RouterPlan {
            providers: self
                .providers
                .iter()
                .map(|(name, provider)| {
                    let planned = PlannedProvider {
                        kind: provider.kind,
                        base_url: provider.base_url.clone(),
                        tasks: provider
                            .models
                            .iter()
                            .map(|(task, model)| (task.clone(), model.config.clone()))
                            .collect(),
                        guarded: provider.guarded,
                    };
                    (name.clone(), planned)
                })
                .collect(),
            routing: self
                .routing
                .iter()
                .map(|(task, route)| (task.clone(), route.route.clone()))
                .collect(),
        }
    }

    /// The connectors in use.
    pub fn enabled_connectors(&self) -> BTreeMap<String, ConnectorConfig> {
        self.connectors
            .iter()
            .filter(|(_, connector)| connector.enabled)
            .map(|(name, connector)| (name.clone(), connector.config.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderEntry;
    use lighthouse_config::ConnectorAuth;

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            model: name.to_owned(),
            input_eur_per_mtok: 1.0,
            output_eur_per_mtok: 2.0,
        }
    }

    fn defaults() -> Defaults {
        let provider = ProviderConfig {
            kind: ProviderKind::Anthropic,
            base_url: None,
            tasks: BTreeMap::from([("text".to_owned(), model("instance-model"))]),
        };
        let connector = ConnectorConfig {
            transport: ConnectorTransport::Stdio {
                command: "x".to_owned(),
                args: Vec::new(),
                secret_env: BTreeMap::from([("KEY".to_owned(), "grafana_key".to_owned())]),
            },
            read_tools: vec!["list".to_owned()],
            draft_tools: Vec::new(),
            app_secret: None,
        };
        Defaults {
            providers: BTreeMap::from([("anthropic".to_owned(), provider)]),
            routing: BTreeMap::from([(
                "text".to_owned(),
                RouteConfig {
                    provider: "anthropic".to_owned(),
                    fallback: None,
                },
            )]),
            connectors: BTreeMap::from([("grafana".to_owned(), connector)]),
            values: BTreeMap::from([("talk.agent".to_owned(), serde_json::json!("brief"))]),
            sources: BTreeMap::new(),
            oauth_apps: Vec::new(),
        }
    }

    fn connector(url: &str) -> ConnectorEntry {
        ConnectorEntry {
            url: url.to_owned(),
            auth: ConnectorAuth::Bearer,
            client_id: None,
            scopes: Vec::new(),
            read_tools: vec!["list_threads".to_owned()],
            draft_tools: Vec::new(),
            enabled: true,
            api: None,
        }
    }

    fn with_apps() -> Defaults {
        let mut defaults = defaults();
        defaults.oauth_apps = vec![
            InstanceApp {
                url: "https://mcp.example.com/mcp/".to_owned(),
                client_id: "shared-app".to_owned(),
                client_secret: Some(SecretString::from("s3")),
            },
            InstanceApp {
                url: "https://login.example.com/token".to_owned(),
                client_id: "shared-rest-app".to_owned(),
                client_secret: Some(SecretString::from("s4")),
            },
        ];
        defaults
    }

    fn oauth_connector(url: &str, client_id: Option<&str>) -> ConnectorEntry {
        ConnectorEntry {
            auth: ConnectorAuth::Oauth,
            client_id: client_id.map(str::to_owned),
            ..connector(url)
        }
    }

    fn rest_oauth_connector(token_url: &str) -> ConnectorEntry {
        ConnectorEntry {
            auth: ConnectorAuth::Oauth,
            api: Some(crate::RestApi {
                key: "none".to_owned(),
                authorize_url: Some("https://login.example.com/authorize".to_owned()),
                token_url: Some(token_url.to_owned()),
                ..Default::default()
            }),
            ..connector("https://api.example.com/openapi.yaml")
        }
    }

    #[test]
    fn connectors_without_a_client_sign_in_with_the_instance_app_for_their_url() {
        use secrecy::ExposeSecret;
        let mut tenant = LayerData::default();
        tenant.connectors.insert(
            "code".to_owned(),
            oauth_connector("https://mcp.example.com/mcp", None),
        );
        tenant.connectors.insert(
            "own".to_owned(),
            oauth_connector("https://mcp.example.com/mcp", Some("their-app")),
        );
        tenant.connectors.insert(
            "elsewhere".to_owned(),
            oauth_connector("https://evil.example.com/mcp", None),
        );
        tenant.connectors.insert(
            "calendar".to_owned(),
            rest_oauth_connector("https://login.example.com/token"),
        );
        tenant.connectors.insert(
            "unknown-rest".to_owned(),
            rest_oauth_connector("https://other.example.com/token"),
        );
        let effective = Effective::resolve(&with_apps(), Some(&tenant), None, false);
        let client_id = |name: &str| match &effective.connectors[name].config.transport {
            ConnectorTransport::Http { client_id, .. } => client_id.clone(),
            ConnectorTransport::OpenApi { oauth, .. } => {
                oauth.as_ref().map(|oauth| oauth.client_id.clone())
            }
            ConnectorTransport::Stdio { .. } => None,
        };

        let code = &effective.connectors["code"];
        assert!(code.instance_app);
        assert_eq!(client_id("code").as_deref(), Some("shared-app"));
        assert_eq!(
            code.config.app_secret.as_ref().unwrap().expose_secret(),
            "s3"
        );

        // A client of its own wins, and the app's secret never goes to another server.
        for name in ["own", "elsewhere"] {
            let connector = &effective.connectors[name];
            assert!(!connector.instance_app, "{name}");
            assert!(connector.config.app_secret.is_none(), "{name}");
        }
        assert_eq!(client_id("own").as_deref(), Some("their-app"));
        assert_eq!(client_id("elsewhere"), None);

        let calendar = &effective.connectors["calendar"];
        assert!(calendar.instance_app);
        assert_eq!(client_id("calendar").as_deref(), Some("shared-rest-app"));
        assert_eq!(
            calendar.config.app_secret.as_ref().unwrap().expose_secret(),
            "s4"
        );

        // A REST API without a client cannot sign in, so it has no OAuth settings.
        assert_eq!(client_id("unknown-rest"), None);
        assert!(!effective.connectors["unknown-rest"].instance_app);
    }

    #[test]
    fn instance_apps_come_from_the_configuration() {
        let toml = format!(
            "{}\n[oauth_apps.code]\nurl = \"https://mcp.example.com/mcp\"\nclient_id = \"shared-app\"\n",
            include_str!("../../../lighthouse.example.toml")
        );
        let dir = std::env::temp_dir().join(format!("lighthouse-apps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lighthouse.toml");
        std::fs::write(&path, toml).unwrap();
        let config = Config::load(&path);
        std::fs::remove_dir_all(&dir).ok();
        let defaults = Defaults::from_config(&config.unwrap());
        let app = defaults.app_for("https://mcp.example.com/mcp/").unwrap();
        assert_eq!(app.client_id, "shared-app");
        assert!(app.client_secret.is_none());
        assert!(defaults.app_for("https://mcp.example.com/other").is_none());
    }

    #[test]
    fn the_instance_alone_is_what_the_configuration_says() {
        let effective = Effective::resolve(&defaults(), None, None, false);
        let provider = &effective.providers["anthropic"];
        assert_eq!(provider.defined_at, Layer::Instance);
        assert_eq!(provider.key_name("anthropic"), "anthropic");
        assert_eq!(provider.key_layers().len(), 3);
        assert_eq!(
            effective.model("anthropic", "text").unwrap().model,
            "instance-model"
        );
        assert!(effective.route_problem("text").is_none());
        assert!(effective.route_problem("image").is_none());
        assert_eq!(effective.enabled_connectors().len(), 1);
        assert!(!effective.connectors["grafana"].guarded());
        assert_eq!(effective.values["talk.agent"].1, Layer::Instance);
        let plan = effective.router_plan();
        assert_eq!(plan.routing["text"].provider, "anthropic");
        assert!(!plan.providers["anthropic"].guarded);
        assert_eq!(
            defaults().reserved_vault_entries(),
            ["mcp:grafana", "oauth:grafana", "grafana_key"]
        );
    }

    #[test]
    fn personal_overrides_tenant_overrides_instance() {
        let mut tenant = LayerData::default();
        tenant.providers.insert(
            "anthropic".to_owned(),
            ProviderEntry {
                kind: ProviderKind::Anthropic,
                base_url: None,
                models: BTreeMap::from([("text".to_owned(), model("tenant-model"))]),
            },
        );
        tenant.providers.insert(
            "local".to_owned(),
            ProviderEntry {
                kind: ProviderKind::Openai,
                base_url: Some("https://llm.example.com".to_owned()),
                models: BTreeMap::from([("text".to_owned(), model("tenant-local"))]),
            },
        );
        tenant
            .switches
            .insert((SwitchKind::Connector, "grafana".to_owned()), false);
        tenant
            .connectors
            .insert("mail".to_owned(), connector("https://mail.example.com/mcp"));
        let mut personal = LayerData::default();
        personal.routes.insert(
            "text".to_owned(),
            RouteConfig {
                provider: "local".to_owned(),
                fallback: Some("anthropic".to_owned()),
            },
        );
        personal
            .values
            .insert("talk.agent".to_owned(), serde_json::json!("mine"));
        let mut own = connector("https://calendar.example.com/mcp");
        own.enabled = false;
        personal.connectors.insert("calendar".to_owned(), own);
        let effective = Effective::resolve(&defaults(), Some(&tenant), Some(&personal), false);
        let anthropic = &effective.providers["anthropic"];
        assert_eq!(
            (anthropic.defined_at, anthropic.source),
            (Layer::Instance, Layer::Tenant)
        );
        assert_eq!(anthropic.models["text"].config.model, "tenant-model");
        let local = &effective.providers["local"];
        assert_eq!(local.key_name("local"), "provider:local");
        assert_eq!(
            local.key_layers(),
            [Layer::Tenant],
            "no member's key for this URL"
        );
        assert!(local.guarded());
        assert_eq!(effective.routing["text"].source, Layer::Personal);
        assert!(effective.route_problem("text").is_none());
        let grafana = &effective.connectors["grafana"];
        assert!(!grafana.enabled);
        assert_eq!(grafana.disabled_by, Some(Layer::Tenant));
        let mail = &effective.connectors["mail"];
        assert!(mail.enabled && mail.guarded());
        assert_eq!(mail.token_layers(), [Layer::Personal, Layer::Tenant]);
        let calendar = &effective.connectors["calendar"];
        assert_eq!(calendar.token_layers(), [Layer::Personal]);
        assert_eq!(calendar.disabled_by, Some(Layer::Personal));
        assert_eq!(
            effective.enabled_connectors().keys().collect::<Vec<_>>(),
            ["mail"]
        );
        assert_eq!(effective.values["talk.agent"].1, Layer::Personal);
        let allowed = Effective::resolve(&defaults(), Some(&tenant), None, true);
        assert!(!allowed.providers["local"].guarded());
        assert!(!allowed.connectors["mail"].guarded());
    }

    #[test]
    fn a_higher_layer_cannot_move_a_lower_providers_endpoint() {
        let mut tenant = LayerData::default();
        tenant.providers.insert(
            "anthropic".to_owned(),
            ProviderEntry {
                kind: ProviderKind::Anthropic,
                base_url: Some("https://evil.example.com".to_owned()),
                models: BTreeMap::from([("text".to_owned(), model("stolen"))]),
            },
        );
        let effective = Effective::resolve(&defaults(), Some(&tenant), None, false);
        let anthropic = &effective.providers["anthropic"];
        assert_eq!(anthropic.base_url, None);
        assert_eq!(anthropic.models["text"].config.model, "instance-model");
    }

    #[test]
    fn broken_routes_are_reported() {
        let mut tenant = LayerData::default();
        tenant.routes.insert(
            "text".to_owned(),
            RouteConfig {
                provider: "anthropic".to_owned(),
                fallback: Some("gone".to_owned()),
            },
        );
        tenant.providers.insert(
            "empty".to_owned(),
            ProviderEntry {
                kind: ProviderKind::Openai,
                base_url: None,
                models: BTreeMap::new(),
            },
        );
        let effective = Effective::resolve(&defaults(), Some(&tenant), None, false);
        assert!(effective.route_problem("text").unwrap().contains("`gone`"));
        let mut tenant = tenant.clone();
        tenant.routes.get_mut("text").unwrap().fallback = Some("empty".to_owned());
        let effective = Effective::resolve(&defaults(), Some(&tenant), None, false);
        assert!(
            effective
                .route_problem("text")
                .unwrap()
                .contains("no model")
        );
    }
}
