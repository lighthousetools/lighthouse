//! Providers, models, routing and connectors in layers: the instance's from
//! `lighthouse.toml`, the tenant's and each member's own.
//!
//! Paths name the layer: `tenant` for what owners and admins set for every
//! member, `me` for what the caller sets for themselves. Every member reads
//! the tenant layer and the merged view that applies to them; nobody reads
//! another member's personal layer.

use crate::auth::Caller;
use crate::hooks::Action;
use crate::{ApiError, ApiState, Instance, Scoped, internal};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use lighthouse_config::{ConnectorAuth, ConnectorTransport};
use lighthouse_settings::{
    ChangeRecord, ConnectorInput, Effective, EffectiveConnector, KNOWN_TASKS, Layer, LayerData,
    ModelInput, Owner, Presets, ProviderInput, RouteInput, SettingsError, SwitchInput,
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

const CHANGES: i64 = 100;

/// A model of a provider for a task.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelView {
    /// Task.
    pub task: String,
    /// Model name.
    pub model: String,
    /// Price in EUR per million input tokens.
    pub input_eur_per_mtok: f64,
    /// Price in EUR per million output tokens.
    pub output_eur_per_mtok: f64,
    /// Where it comes from.
    pub source: Layer,
}

/// A provider as it applies to the caller.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ProviderView {
    /// Name.
    pub name: String,
    /// `anthropic`, `openai`, `mock` or `demo`.
    pub kind: String,
    /// Base URL, if not the provider's default.
    pub base_url: Option<String>,
    /// The highest layer with an entry for it.
    pub source: Layer,
    /// The layer that chose its kind and URL.
    pub defined_at: Layer,
    /// The credential that holds its key, for `PUT /credentials/{name}` or
    /// `PUT /me/credentials/{name}`.
    pub key_name: String,
    /// Where the key used for the caller comes from now, if there is one.
    pub key: Option<Layer>,
    /// Whether it needs a key at all.
    pub needs_key: bool,
    /// Whether a member's own key can be used for it. Not for a URL an owner
    /// or admin of the tenant chose.
    pub own_key: bool,
    /// Models by task.
    pub models: Vec<ModelView>,
}

/// The route of a task as it applies to the caller.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RouteView {
    /// Task.
    pub task: String,
    /// Primary provider.
    pub provider: String,
    /// Its model for the task.
    pub model: Option<String>,
    /// Fallback provider.
    pub fallback: Option<String>,
    /// Its model for the task.
    pub fallback_model: Option<String>,
    /// Where the route comes from.
    pub source: Layer,
    /// Why it cannot work, if it cannot.
    pub problem: Option<String>,
}

/// A connector as it applies to the caller.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ConnectorView {
    /// Name.
    pub name: String,
    /// `http` or `stdio`. Only the instance has `stdio` connectors.
    pub transport: String,
    /// Server URL of an `http` connector.
    pub url: Option<String>,
    /// `none`, `bearer`, `oauth` or `process`.
    pub auth: String,
    /// Pre-registered OAuth client ID.
    pub client_id: Option<String>,
    /// OAuth scopes.
    pub scopes: Vec<String>,
    /// Tools known to be read-only.
    pub read_tools: Vec<String>,
    /// Reversible tools.
    pub draft_tools: Vec<String>,
    /// The layer that defines it.
    pub source: Layer,
    /// Whether it is in use for the caller.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
    /// The credential of its bearer token, if it uses one.
    pub token_name: Option<String>,
    /// Where the credential used for the caller comes from now: `personal`,
    /// `tenant`, or `instance` for connectors that need none.
    pub connected: Option<Layer>,
}

/// Everything that applies to the caller, with the layer of every entry.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SettingsView {
    /// Tasks the server routes to models.
    pub tasks: Vec<String>,
    /// Whether the caller may change the tenant layer.
    pub can_manage: bool,
    /// Whether providers and connectors may use private addresses.
    pub allow_private_endpoints: bool,
    /// Providers.
    pub providers: Vec<ProviderView>,
    /// Routes by task.
    pub routing: Vec<RouteView>,
    /// Connectors, including the ones switched off.
    pub connectors: Vec<ConnectorView>,
    /// Connected sources, including the ones switched off.
    pub sources: Vec<SourceView>,
}

/// A connected source as it applies to the caller.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SourceView {
    /// Its name.
    pub name: String,
    /// What items, refreshes and actions refer to it by.
    pub key: String,
    /// The connector whose tools fill it.
    pub connector: String,
    /// `mail`, `calendar`, `issues`, `code` and so on, if it names one itself.
    pub domain: Option<String>,
    /// The bundled preset it starts from.
    pub preset: Option<String>,
    /// Refresh interval, such as `15m`.
    pub interval: Option<String>,
    /// The layer that defines it.
    pub source: Layer,
    /// Whether it refreshes.
    pub enabled: bool,
    /// The layer that switched it off, if one did.
    pub disabled_by: Option<Layer>,
    /// Its settings as stored, absent for one of the instance.
    pub definition: Option<serde_json::Value>,
}

/// A provider as one layer stores it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct StoredProvider {
    /// Name.
    pub name: String,
    /// `anthropic` or `openai`.
    pub kind: String,
    /// Base URL.
    pub base_url: Option<String>,
    /// Models of this layer.
    pub models: Vec<ModelView>,
}

/// A route as one layer stores it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct StoredRoute {
    /// Task.
    pub task: String,
    /// Primary provider.
    pub provider: String,
    /// Fallback provider.
    pub fallback: Option<String>,
}

/// An entry of a lower layer that this layer switched.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct StoredSwitch {
    /// `connector`, `agent`, `automation` or `source`.
    pub kind: String,
    /// Name of the entry.
    pub name: String,
    /// Whether it is on.
    pub enabled: bool,
}

/// The entries one layer stores, without what it inherits.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LayerView {
    /// `tenant` or `personal`.
    pub layer: Layer,
    /// Providers.
    pub providers: Vec<StoredProvider>,
    /// Routes.
    pub routing: Vec<StoredRoute>,
    /// Connectors.
    pub connectors: Vec<ConnectorView>,
    /// Switched entries of lower layers.
    pub switches: Vec<StoredSwitch>,
}

/// A single setting as it applies to the caller.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ValueView {
    /// `talk.agent`, `notifications`, `weather`, `timezone` or `language`.
    pub key: String,
    /// The value. Its shape depends on the key, see `docs/api.md`.
    pub value: serde_json::Value,
    /// Where it comes from.
    pub source: Layer,
}

/// Which layer a request touches.
#[derive(Deserialize)]
pub(crate) struct ChangesQuery {
    limit: Option<i64>,
}

fn kind_name(kind: lighthouse_config::ProviderKind) -> &'static str {
    match kind {
        lighthouse_config::ProviderKind::Anthropic => "anthropic",
        lighthouse_config::ProviderKind::Openai => "openai",
        lighthouse_config::ProviderKind::Mock => "mock",
        lighthouse_config::ProviderKind::Demo => "demo",
    }
}

fn auth_name(auth: ConnectorAuth) -> &'static str {
    match auth {
        ConnectorAuth::None => "none",
        ConnectorAuth::Bearer => "bearer",
        ConnectorAuth::Oauth => "oauth",
    }
}

pub(crate) fn settings_error(error: SettingsError) -> ApiError {
    match error {
        SettingsError::Invalid(message) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, message),
        SettingsError::Conflict(message) | SettingsError::Limit(message) => {
            ApiError(StatusCode::CONFLICT, message)
        }
        SettingsError::NotFound(what) => ApiError(StatusCode::NOT_FOUND, format!("unknown {what}")),
        SettingsError::Database(_) => internal(error),
    }
}

/// The owner of a layer named in a path, after the role check: `tenant`
/// needs the owner or admin role to change; `me` is the caller's own.
fn owner(layer: &str, caller: &Caller, change: bool) -> Result<Owner, ApiError> {
    match layer {
        "tenant" => {
            if change {
                caller.require_manager()?;
            }
            Ok(None)
        }
        "me" => caller.user.map(Some).ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "the admin token has no personal settings".to_owned(),
            )
        }),
        _ => Err(ApiError(StatusCode::NOT_FOUND, "unknown layer".to_owned())),
    }
}

/// Checks the role and the entitlements before a change.
async fn may_change(
    instance: &Instance,
    caller: &Caller,
    layer: &str,
    area: &str,
) -> Result<Owner, ApiError> {
    let owner = owner(layer, caller, true)?;
    let action = Action::ChangeSettings {
        user: caller.user,
        personal: owner.is_some(),
        area: area.to_owned(),
    };
    instance
        .entitlements
        .check(caller.tenant, &action)
        .await
        .map_err(|message| ApiError(StatusCode::FORBIDDEN, message))?;
    Ok(owner)
}

/// Names of credentials in the vaults a caller's requests use.
struct Stored {
    personal: BTreeSet<String>,
    tenant: BTreeSet<String>,
    instance: BTreeSet<String>,
}

impl Stored {
    async fn of(state: &ApiState, caller: &Caller) -> Result<Self, ApiError> {
        let list = |vault: lighthouse_vault::Vault| async move {
            vault
                .list()
                .await
                .map(|names| names.into_iter().collect::<BTreeSet<_>>())
                .map_err(internal)
        };
        Ok(Self {
            personal: match caller.user {
                Some(user) => list(state.vault.personal(user)).await?,
                None => BTreeSet::new(),
            },
            tenant: list(state.vault.clone()).await?,
            instance: match &state.instance_vault {
                Some(vault) => list(vault.clone()).await?,
                None => BTreeSet::new(),
            },
        })
    }

    fn find(&self, layers: &[Layer], entry: &str) -> Option<Layer> {
        layers.iter().copied().find(|layer| {
            match layer {
                Layer::Personal => &self.personal,
                Layer::Tenant => &self.tenant,
                Layer::Instance => &self.instance,
            }
            .contains(entry)
        })
    }
}

fn connector_view(
    name: &str,
    connector: &EffectiveConnector,
    stored: Option<&Stored>,
) -> ConnectorView {
    let (transport, url, auth, client_id, scopes) = match &connector.config.transport {
        ConnectorTransport::Http {
            url,
            auth,
            client_id,
            scopes,
            ..
        } => (
            "http",
            Some(url.clone()),
            auth_name(*auth),
            client_id.clone(),
            scopes.clone(),
        ),
        ConnectorTransport::Stdio { .. } => ("stdio", None, "process", None, Vec::new()),
    };
    let token = match auth {
        "bearer" => Some(format!("mcp:{name}")),
        "oauth" => Some(format!("oauth:{name}")),
        _ => None,
    };
    let connected = match (&token, stored) {
        (None, _) => Some(Layer::Instance),
        (Some(token), Some(stored)) => stored.find(connector.token_layers(), token),
        (Some(_), None) => None,
    };
    ConnectorView {
        name: name.to_owned(),
        transport: transport.to_owned(),
        url,
        auth: auth.to_owned(),
        client_id,
        scopes,
        read_tools: connector.config.read_tools.clone(),
        draft_tools: connector.config.draft_tools.clone(),
        source: connector.source,
        enabled: connector.enabled,
        disabled_by: connector.disabled_by,
        token_name: (auth == "bearer").then(|| format!("mcp:{name}")),
        connected,
    }
}

fn view(
    effective: &Effective,
    stored: &Stored,
    caller: &Caller,
    allow_private: bool,
) -> SettingsView {
    let providers = effective
        .providers
        .iter()
        .map(|(name, provider)| {
            let key_name = provider.key_name(name);
            let needs_key = matches!(
                provider.kind,
                lighthouse_config::ProviderKind::Anthropic
                    | lighthouse_config::ProviderKind::Openai
            );
            ProviderView {
                name: name.clone(),
                kind: kind_name(provider.kind).to_owned(),
                base_url: provider.base_url.clone(),
                source: provider.source,
                defined_at: provider.defined_at,
                key: if needs_key {
                    stored.find(provider.key_layers(), &key_name)
                } else {
                    Some(Layer::Instance)
                },
                key_name,
                needs_key,
                own_key: provider.key_layers().contains(&Layer::Personal),
                models: provider
                    .models
                    .iter()
                    .map(|(task, model)| ModelView {
                        task: task.clone(),
                        model: model.config.model.clone(),
                        input_eur_per_mtok: model.config.input_eur_per_mtok,
                        output_eur_per_mtok: model.config.output_eur_per_mtok,
                        source: model.source,
                    })
                    .collect(),
            }
        })
        .collect();
    let routing = effective
        .routing
        .iter()
        .map(|(task, route)| {
            let model = |provider: &str| {
                effective
                    .model(provider, task)
                    .map(|model| model.model.clone())
            };
            RouteView {
                task: task.clone(),
                provider: route.route.provider.clone(),
                model: model(&route.route.provider),
                fallback: route.route.fallback.clone(),
                fallback_model: route.route.fallback.as_deref().and_then(model),
                source: route.source,
                problem: effective.route_problem(task),
            }
        })
        .collect();
    SettingsView {
        tasks: KNOWN_TASKS.iter().map(|task| (*task).to_owned()).collect(),
        can_manage: caller.role.can_manage(),
        allow_private_endpoints: allow_private,
        providers,
        routing,
        connectors: effective
            .connectors
            .iter()
            .map(|(name, connector)| connector_view(name, connector, Some(stored)))
            .collect(),
        sources: effective
            .sources
            .values()
            .map(|source| SourceView {
                name: source.name.clone(),
                key: source.key.clone(),
                connector: source.config.connector.clone(),
                domain: source.config.domain.clone(),
                preset: source.config.preset.clone(),
                interval: source.config.interval.clone(),
                source: source.source,
                enabled: source.enabled,
                disabled_by: source.disabled_by,
                definition: source.definition.clone(),
            })
            .collect(),
    }
}

fn layer_view(layer: Layer, data: &LayerData, allow_private: bool) -> LayerView {
    let effective = Effective::resolve(
        &lighthouse_settings::Defaults::default(),
        (layer == Layer::Tenant).then_some(data),
        (layer == Layer::Personal).then_some(data),
        allow_private,
    );
    LayerView {
        layer,
        providers: data
            .providers
            .iter()
            .map(|(name, provider)| StoredProvider {
                name: name.clone(),
                kind: kind_name(provider.kind).to_owned(),
                base_url: provider.base_url.clone(),
                models: provider
                    .models
                    .iter()
                    .map(|(task, model)| ModelView {
                        task: task.clone(),
                        model: model.model.clone(),
                        input_eur_per_mtok: model.input_eur_per_mtok,
                        output_eur_per_mtok: model.output_eur_per_mtok,
                        source: layer,
                    })
                    .collect(),
            })
            .collect(),
        routing: data
            .routes
            .iter()
            .map(|(task, route)| StoredRoute {
                task: task.clone(),
                provider: route.provider.clone(),
                fallback: route.fallback.clone(),
            })
            .collect(),
        connectors: effective
            .connectors
            .iter()
            .map(|(name, connector)| connector_view(name, connector, None))
            .collect(),
        switches: data
            .switches
            .iter()
            .map(|((kind, name), enabled)| StoredSwitch {
                kind: kind.as_str().to_owned(),
                name: name.clone(),
                enabled: *enabled,
            })
            .collect(),
    }
}

/// Lists suggested providers, models and connectors to prefill forms. The
/// suggestions are data of the server, not settings.
#[utoipa::path(
    get,
    path = "/settings/presets",
    responses((status = 200, body = Presets)),
    security(("bearer" = []))
)]
pub(crate) async fn presets() -> Json<Presets> {
    Json(lighthouse_settings::presets().clone())
}

/// Shows the providers, models, routes and connectors that apply to the
/// caller, each with the layer it comes from.
#[utoipa::path(
    get,
    path = "/settings/effective",
    responses((status = 200, body = SettingsView)),
    security(("bearer" = []))
)]
pub(crate) async fn effective(
    Scoped(state): Scoped,
    caller: Caller,
) -> Result<Json<SettingsView>, ApiError> {
    let effective = state.settings.effective(caller.user);
    let stored = Stored::of(&state, &caller).await?;
    let allow_private = state.settings.options().allow_private_endpoints;
    Ok(Json(view(&effective, &stored, &caller, allow_private)))
}

/// Shows what one layer stores: `tenant` for the whole tenant, `me` for the
/// caller's own settings.
#[utoipa::path(
    get,
    path = "/settings/{layer}/entries",
    params(("layer" = String, Path, description = "`tenant` or `me`")),
    responses((status = 200, body = LayerView), (status = 404, description = "Unknown layer")),
    security(("bearer" = []))
)]
pub(crate) async fn entries(
    Scoped(state): Scoped,
    caller: Caller,
    Path(layer): Path<String>,
) -> Result<Json<LayerView>, ApiError> {
    let owner = owner(&layer, &caller, false)?;
    let data = state.settings.layer(owner);
    let allow_private = state.settings.options().allow_private_endpoints;
    Ok(Json(layer_view(
        lighthouse_settings::layer_of(owner),
        &data,
        allow_private,
    )))
}

/// Lists the latest changes of a layer: who changed what and when. Values are
/// not recorded. The tenant layer's changes need the owner or admin role.
#[utoipa::path(
    get,
    path = "/settings/{layer}/changes",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("limit" = Option<i64>, Query, description = "Maximum number of changes")),
    responses((status = 200, body = Vec<ChangeRecord>),
        (status = 403, description = "Needs the owner or admin role")),
    security(("bearer" = []))
)]
pub(crate) async fn changes(
    Scoped(state): Scoped,
    caller: Caller,
    Path(layer): Path<String>,
    Query(query): Query<ChangesQuery>,
) -> Result<Json<Vec<ChangeRecord>>, ApiError> {
    let owner = owner(&layer, &caller, true)?;
    let limit = query.limit.unwrap_or(CHANGES).clamp(1, 500);
    Ok(Json(
        state
            .settings
            .changes(owner, limit)
            .await
            .map_err(settings_error)?,
    ))
}

fn changed(state: &ApiState) {
    state
        .events
        .emit("settings.changed", serde_json::json!({"key": "layers"}));
}

/// Adds a provider to a layer or changes its kind and URL. A provider whose
/// name a lower layer has keeps that kind and URL; a layer can then only
/// change its models.
#[utoipa::path(
    put,
    path = "/settings/{layer}/providers/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Provider name")),
    request_body = ProviderInput,
    responses((status = 204), (status = 403, description = "Needs the owner or admin role"),
        (status = 409, description = "Conflicts with a lower layer, or a limit is reached"),
        (status = 422, description = "Invalid input or an endpoint that is not public")),
    security(("bearer" = []))
)]
pub(crate) async fn put_provider(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
    Json(input): Json<ProviderInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "provider").await?;
    state
        .settings
        .set_provider(owner, caller.user, &name, &input)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a provider of a layer with its models. Its key stays in the vault.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/providers/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Provider name")),
    responses((status = 204), (status = 404, description = "Not in this layer"),
        (status = 409, description = "A route of the layer needs it")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_provider(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "provider").await?;
    state
        .settings
        .delete_provider(owner, caller.user, &name)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Sets the model of a provider for a task, with its prices. For a provider of
/// a lower layer, the layer gets its own entry with the same kind and URL.
#[utoipa::path(
    put,
    path = "/settings/{layer}/providers/{name}/models/{task}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Provider name"),
        ("task" = String, Path, description = "Task, such as `text`")),
    request_body = ModelInput,
    responses((status = 204), (status = 404, description = "Unknown provider"),
        (status = 422, description = "Invalid model, price or task")),
    security(("bearer" = []))
)]
pub(crate) async fn put_model(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name, task)): Path<(String, String, String)>,
    Json(input): Json<ModelInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "model").await?;
    state
        .settings
        .set_model(owner, caller.user, &name, &task, &input)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes the model of a provider for a task in a layer.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/providers/{name}/models/{task}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Provider name"),
        ("task" = String, Path, description = "Task")),
    responses((status = 204), (status = 404, description = "Not in this layer"),
        (status = 409, description = "A route of the layer needs it")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_model(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name, task)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "model").await?;
    state
        .settings
        .delete_model(owner, caller.user, &name, &task)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Sets the primary provider and fallback of a task in a layer. Both need a
/// model for the task.
#[utoipa::path(
    put,
    path = "/settings/{layer}/routing/{task}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("task" = String, Path, description = "Task, such as `text`")),
    request_body = RouteInput,
    responses((status = 204), (status = 422, description = "Unknown task or provider without a model")),
    security(("bearer" = []))
)]
pub(crate) async fn put_route(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, task)): Path<(String, String)>,
    Json(input): Json<RouteInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "route").await?;
    state
        .settings
        .set_route(owner, caller.user, &task, &input)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes the route of a task in a layer, so the lower layer's applies again.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/routing/{task}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("task" = String, Path, description = "Task")),
    responses((status = 204), (status = 404, description = "Not in this layer")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_route(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, task)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "route").await?;
    state
        .settings
        .delete_route(owner, caller.user, &task)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Adds or replaces an MCP connector over streamable HTTP in a layer. The URL
/// must be a public `https` URL unless the instance allows private endpoints.
/// A personal connector's tokens live in the caller's own vault.
#[utoipa::path(
    put,
    path = "/settings/{layer}/connectors/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Connector name")),
    request_body = ConnectorInput,
    responses((status = 204), (status = 409, description = "The name exists in a lower layer, or a limit is reached"),
        (status = 422, description = "Invalid input or an endpoint that is not public")),
    security(("bearer" = []))
)]
pub(crate) async fn put_connector(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
    Json(input): Json<ConnectorInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "connector").await?;
    state
        .settings
        .set_connector(owner, caller.user, &name, &input)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a connector of a layer. Its tokens stay in the vault until deleted.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/connectors/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Connector name")),
    responses((status = 204), (status = 404, description = "Not in this layer")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_connector(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "connector").await?;
    state
        .settings
        .delete_connector(owner, caller.user, &name)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Switches a connector on or off in a layer: one of the layer itself, or one
/// of a lower layer for this layer only, such as an instance connector the
/// tenant does not use.
#[utoipa::path(
    put,
    path = "/settings/{layer}/connectors/{name}/enabled",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Connector name")),
    request_body = SwitchInput,
    responses((status = 204), (status = 404, description = "Unknown connector")),
    security(("bearer" = []))
)]
pub(crate) async fn switch_connector(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
    Json(input): Json<SwitchInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "connector").await?;
    state
        .settings
        .switch_connector(owner, caller.user, &name, input.enabled)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Adds or replaces a connected source of a layer. A shared source's mail and
/// calendar items belong to the tenant's first owner; all items of a personal
/// source belong to the caller and reach nobody else. It syncs with the
/// connector and tokens its owner has, and may only call read-only tools.
#[utoipa::path(
    put,
    path = "/settings/{layer}/sources/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Source name")),
    request_body = lighthouse_settings::SourceInput,
    responses((status = 204), (status = 403, description = "Needs the owner or admin role"),
        (status = 409, description = "The name exists in a lower layer, or a limit is reached"),
        (status = 422, description = "Invalid source, or a tool that is not read-only")),
    security(("bearer" = []))
)]
pub(crate) async fn put_source(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
    Json(input): Json<lighthouse_settings::SourceInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "source").await?;
    state
        .settings
        .set_source(owner, caller.user, &name, &input)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a connected source of a layer with its items.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/sources/{name}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Source name")),
    responses((status = 204), (status = 404, description = "Not in this layer")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_source(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "source").await?;
    state
        .settings
        .delete_source(owner, caller.user, &name)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Switches a source on or off: one of the layer itself, or for the tenant one
/// of the instance. A person cannot switch a shared source off, since it keeps
/// refreshing for the others.
#[utoipa::path(
    put,
    path = "/settings/{layer}/sources/{name}/enabled",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("name" = String, Path, description = "Source name")),
    request_body = SwitchInput,
    responses((status = 204), (status = 404, description = "Unknown source"),
        (status = 409, description = "A shared source")),
    security(("bearer" = []))
)]
pub(crate) async fn switch_source(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, name)): Path<(String, String)>,
    Json(input): Json<SwitchInput>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "source").await?;
    state
        .settings
        .switch_source(owner, caller.user, &name, input.enabled)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the single settings that apply to the caller, each with its source.
#[utoipa::path(
    get,
    path = "/settings/values",
    responses((status = 200, body = Vec<ValueView>)),
    security(("bearer" = []))
)]
pub(crate) async fn values(Scoped(state): Scoped, caller: Caller) -> Json<Vec<ValueView>> {
    let effective = state.settings.effective(caller.user);
    Json(
        lighthouse_settings::VALUE_KEYS
            .iter()
            .filter_map(|key| {
                effective.values.get(*key).map(|(value, source)| ValueView {
                    key: (*key).to_owned(),
                    value: value.clone(),
                    source: *source,
                })
            })
            .collect(),
    )
}

/// Sets a single setting in a layer: `talk.agent`, `notifications`, `weather`,
/// `timezone` or `language`.
#[utoipa::path(
    put,
    path = "/settings/{layer}/values/{key}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("key" = String, Path, description = "Setting key")),
    request_body = serde_json::Value,
    responses((status = 204), (status = 404, description = "Unknown key"),
        (status = 422, description = "Invalid value")),
    security(("bearer" = []))
)]
pub(crate) async fn put_value(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, key)): Path<(String, String)>,
    Json(value): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "value").await?;
    let value =
        lighthouse_settings::check_value(&key, &value, owner.is_some()).map_err(settings_error)?;
    state
        .settings
        .set_value(owner, caller.user, &key, &value)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a single setting of a layer, so the lower layer's applies again.
#[utoipa::path(
    delete,
    path = "/settings/{layer}/values/{key}",
    params(("layer" = String, Path, description = "`tenant` or `me`"),
        ("key" = String, Path, description = "Setting key")),
    responses((status = 204), (status = 404, description = "Not in this layer")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_value(
    State(instance): State<Instance>,
    Scoped(state): Scoped,
    caller: Caller,
    Path((layer, key)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let owner = may_change(&instance, &caller, &layer, "value").await?;
    state
        .settings
        .delete_value(owner, caller.user, &key)
        .await
        .map_err(settings_error)?;
    changed(&state);
    Ok(StatusCode::NO_CONTENT)
}

fn personal_vault(state: &ApiState, caller: &Caller) -> Result<lighthouse_vault::Vault, ApiError> {
    caller
        .user
        .map(|user| state.vault.personal(user))
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "the admin token has no personal credentials".to_owned(),
            )
        })
}

fn personal_name(name: &str) -> Result<(), ApiError> {
    let valid = !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '-' | '_'))
        && !name.starts_with("oauth:");
    if valid {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "use a provider key name such as `anthropic` or `provider:<name>`, or `mcp:<connector>`"
                .to_owned(),
        ))
    }
}

/// Lists the names of the caller's own credentials in this tenant.
#[utoipa::path(
    get,
    path = "/me/credentials",
    responses((status = 200, body = Vec<String>)),
    security(("bearer" = []))
)]
pub(crate) async fn list_personal_credentials(
    Scoped(state): Scoped,
    caller: Caller,
) -> Result<Json<Vec<String>>, ApiError> {
    let vault = personal_vault(&state, &caller)?;
    Ok(Json(vault.list().await.map_err(internal)?))
}

/// Stores one of the caller's own credentials, such as their own provider key
/// or the bearer token of a connector. Other members never use it.
#[utoipa::path(
    put,
    path = "/me/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    request_body = crate::CredentialValue,
    responses((status = 204), (status = 400, description = "Empty value"),
        (status = 422, description = "Invalid name")),
    security(("bearer" = []))
)]
pub(crate) async fn put_personal_credential(
    Scoped(state): Scoped,
    caller: Caller,
    Path(name): Path<String>,
    Json(request): Json<crate::CredentialValue>,
) -> Result<StatusCode, ApiError> {
    let vault = personal_vault(&state, &caller)?;
    personal_name(&name)?;
    if request.value.expose_secret().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "the value is empty".to_owned(),
        ));
    }
    vault.put(&name, &request.value).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes one of the caller's own credentials, including the tokens of a
/// connector they connected themselves (`oauth:<connector>`).
#[utoipa::path(
    delete,
    path = "/me/credentials/{name}",
    params(("name" = String, Path, description = "Credential name")),
    responses((status = 204), (status = 404, description = "Unknown credential")),
    security(("bearer" = []))
)]
pub(crate) async fn delete_personal_credential(
    Scoped(state): Scoped,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let vault = personal_vault(&state, &caller)?;
    if vault.delete(&name).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "unknown credential".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests;
