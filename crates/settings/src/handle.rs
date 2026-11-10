//! The settings of one tenant in memory, kept current.

use crate::effective::{Defaults, Effective, EffectiveSource};
use crate::store::{ChangeRecord, SettingsStore, parse_auth};
use crate::{
    ConnectorEntry, ConnectorInput, Layer, LayerData, ModelInput, Owner, ProviderInput, RouteInput,
    SettingsError, Snapshot, SwitchKind, validate,
};
use crate::{FileEntry, FileKind, SourceEntry, SourceInput};
use lighthouse_config::{Config, ModelConfig, RouteConfig, SettingsLimits, SourceConfig};
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// What the instance allows tenants and people to set.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Whether endpoints may be private addresses or plain HTTP.
    pub allow_private_endpoints: bool,
    /// How much a tenant and its members may add.
    pub limits: SettingsLimits,
}

impl Options {
    /// The options of a configuration file.
    pub fn from_config(config: &Config) -> Self {
        Self {
            allow_private_endpoints: config.tenancy.allow_private_endpoints,
            limits: config.tenancy.limits,
        }
    }
}

struct Inner {
    store: SettingsStore,
    defaults: Arc<Defaults>,
    options: Options,
    snapshot: RwLock<Arc<Snapshot>>,
    /// Says that a new snapshot is in place.
    changed: tokio::sync::watch::Sender<u64>,
}

/// The settings of one tenant: the instance defaults, the tenant layer and
/// the personal layers of its members. Cheap to clone.
#[derive(Clone)]
pub struct TenantSettings {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TenantSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantSettings")
            .field("revision", &self.snapshot().revision)
            .finish_non_exhaustive()
    }
}

fn conflict(message: impl Into<String>) -> SettingsError {
    SettingsError::Conflict(message.into())
}

/// Why a source cannot refresh with what a user may use, if it cannot: its
/// connector is missing or switched off, or a collection calls a tool the
/// connector does not list as read-only.
pub fn source_problem(source: &lighthouse_sync::Source, view: &Effective) -> Option<String> {
    let connector = view.connectors.get(&source.connector);
    match connector {
        None => return Some(format!("connector `{}` does not exist", source.connector)),
        Some(connector) if !connector.enabled => {
            return Some(format!("connector `{}` is switched off", source.connector));
        }
        Some(_) => {}
    }
    let policy = lighthouse_mcp::ToolPolicy::new(&view.enabled_connectors());
    source.collections.iter().find_map(|(collection, config)| {
        let tool = source.tool(&config.tool);
        (policy.level(&tool) != lighthouse_core::agent::ToolLevel::Read).then(|| {
            format!(
                "collection `{collection}` calls `{tool}`, which is not in the connector's \
                 read_tools. Refreshes only read."
            )
        })
    })
}

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Instance => "the instance",
        Layer::Tenant => "the tenant",
        Layer::Personal => "your own settings",
    }
}

impl TenantSettings {
    /// Settings with nothing stored yet. Call [`TenantSettings::reload`] to read
    /// the database.
    pub fn new(store: Store, tenant: TenantId, defaults: Arc<Defaults>, options: Options) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: SettingsStore::new(store, tenant),
                defaults,
                options,
                snapshot: RwLock::new(Arc::default()),
                changed: tokio::sync::watch::Sender::new(0),
            }),
        }
    }

    /// Opens the settings of a tenant and reads them.
    pub async fn open(
        store: Store,
        tenant: TenantId,
        defaults: Arc<Defaults>,
        options: Options,
    ) -> Result<Self, SettingsError> {
        let settings = Self::new(store, tenant, defaults, options);
        settings.reload().await?;
        Ok(settings)
    }

    /// The instance layer.
    pub fn defaults(&self) -> &Defaults {
        &self.inner.defaults
    }

    /// What the instance allows.
    pub fn options(&self) -> Options {
        self.inner.options
    }

    /// The stored layers as they are now.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn resolve(&self, snapshot: &Snapshot, user: Option<Uuid>) -> Effective {
        let mut effective = Effective::resolve(
            &self.inner.defaults,
            Some(&snapshot.tenant),
            user.and_then(|user| snapshot.personal.get(&user)),
            self.inner.options.allow_private_endpoints,
        );
        if let Some(user) = user {
            for source in effective.sources.values_mut() {
                if source.source == Layer::Personal {
                    source.owner = Some(user);
                    source.key = lighthouse_sync::personal_key(&source.name, user);
                }
            }
        }
        effective
    }

    /// Every source of the tenant in use: the shared ones and those of each
    /// member.
    pub fn all_sources(&self) -> Vec<EffectiveSource> {
        let snapshot = self.snapshot();
        let mut sources: Vec<EffectiveSource> = self
            .resolve(&snapshot, None)
            .sources
            .into_values()
            .filter(|source| source.enabled)
            .collect();
        for member in &snapshot.members {
            if !snapshot.personal.contains_key(&member.id) {
                continue;
            }
            sources.extend(
                self.resolve(&snapshot, Some(member.id))
                    .sources
                    .into_values()
                    .filter(|source| source.enabled && source.source == Layer::Personal),
            );
        }
        sources
    }

    /// What applies to a user, or to the tenant as a whole for `None`.
    pub fn effective(&self, user: Option<Uuid>) -> Effective {
        self.resolve(&self.snapshot(), user)
    }

    /// The stored entries of one layer.
    pub fn layer(&self, owner: Owner) -> LayerData {
        self.snapshot().layer(owner).cloned().unwrap_or_default()
    }

    /// Reads every layer from the database and swaps them in at once.
    pub async fn reload(&self) -> Result<(), SettingsError> {
        let snapshot = Arc::new(self.inner.store.load().await?);
        *self
            .inner
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        self.inner.changed.send_modify(|version| *version += 1);
        Ok(())
    }

    /// Wakes up whenever the settings changed, here or in another process.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    /// Reloads if another process changed something, or members came or went.
    /// Returns whether it did.
    pub async fn refresh(&self) -> Result<bool, SettingsError> {
        let revision = self.inner.store.revision().await?;
        let members = self.inner.store.members().await?;
        let current = self.snapshot();
        if revision == current.revision && members == current.members {
            return Ok(false);
        }
        self.reload().await?;
        Ok(true)
    }

    /// The latest changes of one layer, newest first.
    pub async fn changes(
        &self,
        owner: Owner,
        limit: i64,
    ) -> Result<Vec<ChangeRecord>, SettingsError> {
        self.inner.store.changes(owner, limit).await
    }

    /// What a layer builds on: the layers below it.
    fn below(&self, snapshot: &Snapshot, owner: Owner) -> Effective {
        let tenant = owner.map(|_| &snapshot.tenant);
        Effective::resolve(
            &self.inner.defaults,
            tenant,
            None,
            self.inner.options.allow_private_endpoints,
        )
    }

    /// What a layer sees, itself included.
    fn view(&self, snapshot: &Snapshot, owner: Owner) -> Effective {
        self.resolve(snapshot, owner)
    }

    fn check_total(&self, snapshot: &Snapshot) -> Result<(), SettingsError> {
        let limit = self.inner.options.limits.tenant_entries;
        if snapshot.entries() >= limit {
            return Err(SettingsError::Limit(format!(
                "this tenant has reached the limit of {limit} settings"
            )));
        }
        Ok(())
    }

    /// Runs a change in one transaction: takes the tenant's lock, checks against
    /// the latest state, writes and records. A failed step leaves nothing behind,
    /// and the lock keeps limits from racing across server processes. The new
    /// state is swapped in after the commit.
    async fn change(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        area: &str,
        name: &str,
        action: &str,
        write: impl AsyncFnOnce(Arc<Snapshot>, &mut sqlx::PgConnection) -> Result<(), SettingsError>,
    ) -> Result<(), SettingsError> {
        let store = &self.inner.store;
        let mut transaction = store.pool().begin().await?;
        store.bump(&mut transaction).await?;
        let snapshot = Arc::new(store.load().await?);
        write(snapshot, &mut transaction).await?;
        store
            .record(&mut transaction, owner, actor, area, name, action)
            .await?;
        transaction.commit().await?;
        self.reload().await
    }

    fn routes_still_work(
        &self,
        snapshot: &Snapshot,
        owner: Owner,
        changed: impl FnOnce(&mut LayerData),
    ) -> Result<(), SettingsError> {
        let mut after = snapshot.clone();
        let layer = match owner {
            None => &mut after.tenant,
            Some(user) => after.personal.entry(user).or_default(),
        };
        changed(layer);
        let routes: Vec<String> = layer.routes.keys().cloned().collect();
        let view = self.view(&after, owner);
        for task in routes {
            if let Some(problem) = view.route_problem(&task) {
                return Err(conflict(format!(
                    "the route of `{task}` needs it: {problem}; change the route first"
                )));
            }
        }
        Ok(())
    }

    /// Adds a provider or changes its kind and URL. A provider whose name a
    /// lower layer already has keeps that kind and URL; only its models change.
    pub async fn set_provider(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
        input: &ProviderInput,
    ) -> Result<(), SettingsError> {
        validate::name("provider", name)?;
        let kind = validate::kind(&input.kind)?;
        let base_url = match input.base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => {
                Some(validate::endpoint(url, self.inner.options.allow_private_endpoints).await?)
            }
            _ => None,
        };
        self.change(owner, actor, "provider", name, "set", async move |snapshot, conn| {
            if let Some(lower) = self.below(&snapshot, owner).providers.get(name)
                && (lower.kind != kind || lower.base_url != base_url)
            {
                return Err(conflict(format!(
                    "`{name}` comes from {}: here you can change its models, not its kind or URL",
                    layer_name(lower.defined_at)
                )));
            }
            let current = snapshot.layer(owner).cloned().unwrap_or_default();
            if !current.providers.contains_key(name) {
                let limit = self.inner.options.limits.providers;
                if current.providers.len() >= limit {
                    return Err(SettingsError::Limit(format!(
                        "at most {limit} providers per layer"
                    )));
                }
                self.check_total(&snapshot)?;
            }
            self.inner
                .store
                .put_provider(conn, owner, name, kind, base_url.as_deref())
                .await
        })
        .await
    }

    /// Deletes a provider of a layer with its models.
    pub async fn delete_provider(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "provider",
            name,
            "delete",
            async move |snapshot, conn| {
                self.routes_still_work(&snapshot, owner, |layer| {
                    layer.providers.remove(name);
                })?;
                if self.inner.store.delete_provider(conn, owner, name).await? {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound("provider".to_owned()))
                }
            },
        )
        .await
    }

    /// Sets the model of a provider for a task. For a provider of a lower
    /// layer, an entry with its kind and URL is added first.
    pub async fn set_model(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        provider: &str,
        task: &str,
        input: &ModelInput,
    ) -> Result<(), SettingsError> {
        validate::task(task)?;
        validate::model(&input.model)?;
        validate::price("input_eur_per_mtok", input.input_eur_per_mtok)?;
        validate::price("output_eur_per_mtok", input.output_eur_per_mtok)?;
        let model = ModelConfig {
            model: input.model.trim().to_owned(),
            input_eur_per_mtok: input.input_eur_per_mtok,
            output_eur_per_mtok: input.output_eur_per_mtok,
        };
        let name = format!("{provider}.{task}");
        self.change(
            owner,
            actor,
            "model",
            &name,
            "set",
            async move |snapshot, conn| {
                let current = snapshot.layer(owner).cloned().unwrap_or_default();
                match current.providers.get(provider) {
                    Some(entry) => {
                        let limit = self.inner.options.limits.models;
                        if !entry.models.contains_key(task) && entry.models.len() >= limit {
                            return Err(SettingsError::Limit(format!(
                                "at most {limit} models per provider"
                            )));
                        }
                    }
                    None => {
                        let lower = self.below(&snapshot, owner);
                        let lower = lower
                            .providers
                            .get(provider)
                            .ok_or_else(|| SettingsError::NotFound("provider".to_owned()))?;
                        self.inner
                            .store
                            .put_provider(
                                conn,
                                owner,
                                provider,
                                lower.kind,
                                lower.base_url.as_deref(),
                            )
                            .await?;
                    }
                }
                self.check_total(&snapshot)?;
                self.inner
                    .store
                    .put_model(conn, owner, provider, task, &model)
                    .await?;
                Ok(())
            },
        )
        .await
    }

    /// Deletes the model of a provider for a task.
    pub async fn delete_model(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        provider: &str,
        task: &str,
    ) -> Result<(), SettingsError> {
        let name = format!("{provider}.{task}");
        self.change(
            owner,
            actor,
            "model",
            &name,
            "delete",
            async move |snapshot, conn| {
                self.routes_still_work(&snapshot, owner, |layer| {
                    if let Some(entry) = layer.providers.get_mut(provider) {
                        entry.models.remove(task);
                    }
                })?;
                if self
                    .inner
                    .store
                    .delete_model(conn, owner, provider, task)
                    .await?
                {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound("model".to_owned()))
                }
            },
        )
        .await
    }

    /// Sets the route of a task for a layer.
    pub async fn set_route(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        task: &str,
        input: &RouteInput,
    ) -> Result<(), SettingsError> {
        validate::task(task)?;
        let route = RouteConfig {
            provider: input.provider.clone(),
            fallback: input
                .fallback
                .clone()
                .filter(|fallback| !fallback.is_empty()),
        };
        if route.fallback.as_ref() == Some(&route.provider) {
            return Err(SettingsError::Invalid(
                "the fallback must be another provider".to_owned(),
            ));
        }
        self.change(
            owner,
            actor,
            "route",
            task,
            "set",
            async move |snapshot, conn| {
                let view = self.view(&snapshot, owner);
                for name in std::iter::once(&route.provider).chain(&route.fallback) {
                    if view.model(name, task).is_none() {
                        return Err(SettingsError::Invalid(format!(
                            "`{}` has no model for `{task}`",
                            name.chars().take(40).collect::<String>()
                        )));
                    }
                }
                self.check_total(&snapshot)?;
                self.inner.store.put_route(conn, owner, task, &route).await
            },
        )
        .await
    }

    /// Deletes the route of a task in a layer, so the lower layer's applies.
    pub async fn delete_route(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        task: &str,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "route",
            task,
            "delete",
            async move |_, conn| {
                if self.inner.store.delete_route(conn, owner, task).await? {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound("route".to_owned()))
                }
            },
        )
        .await
    }

    /// Adds or replaces a connector of a layer.
    pub async fn set_connector(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
        input: &ConnectorInput,
    ) -> Result<(), SettingsError> {
        validate::name("connector", name)?;
        let auth = parse_auth(&input.auth).ok_or_else(|| {
            SettingsError::Invalid("`auth` must be `none`, `bearer` or `oauth`".to_owned())
        })?;
        validate::client_id(input.client_id.as_deref())?;
        validate::scopes(&input.scopes)?;
        validate::tools("read_tools", &input.read_tools)?;
        validate::tools("draft_tools", &input.draft_tools)?;
        if input
            .read_tools
            .iter()
            .any(|tool| input.draft_tools.contains(tool))
        {
            return Err(SettingsError::Invalid(
                "a tool is either in `read_tools` or in `draft_tools`".to_owned(),
            ));
        }
        let allow_private = self.inner.options.allow_private_endpoints;
        let url = validate::endpoint(&input.url, allow_private).await?;
        let api = match &input.api {
            Some(api) => Some(validate::rest_api(api, auth, allow_private).await?),
            None => None,
        };
        let entry = ConnectorEntry {
            url,
            auth,
            client_id: input.client_id.clone(),
            scopes: input.scopes.clone(),
            read_tools: input.read_tools.clone(),
            draft_tools: input.draft_tools.clone(),
            enabled: input.enabled,
            api,
        };
        self.change(
            owner,
            actor,
            "connector",
            name,
            "set",
            async move |snapshot, conn| {
                if let Some(lower) = self.below(&snapshot, owner).connectors.get(name) {
                    return Err(conflict(format!(
                        "`{name}` comes from {}; switch it off there or choose another name",
                        layer_name(lower.source)
                    )));
                }
                if owner.is_none()
                    && self
                        .inner
                        .defaults
                        .reserved_vault_entries()
                        .iter()
                        .any(|entry| {
                            entry == &format!("mcp:{name}") || entry == &format!("oauth:{name}")
                        })
                {
                    return Err(conflict(format!("`{name}` is reserved by the instance")));
                }
                let current = snapshot.layer(owner).cloned().unwrap_or_default();
                if !current.connectors.contains_key(name) {
                    let limit = self.inner.options.limits.connectors;
                    if current.connectors.len() >= limit {
                        return Err(SettingsError::Limit(format!(
                            "at most {limit} connectors per layer"
                        )));
                    }
                    self.check_total(&snapshot)?;
                }
                // Members connected to the old URL. Their tokens must not go to
                // a new one that an owner or admin chose.
                let moved = current
                    .connectors
                    .get(name)
                    .is_none_or(|old| old.url != entry.url);
                if owner.is_none() && moved {
                    self.inner
                        .store
                        .remove_personal_tokens(conn, None, name)
                        .await?;
                }
                self.inner
                    .store
                    .put_connector(conn, owner, name, &entry)
                    .await
            },
        )
        .await
    }

    /// Adds or replaces a connected source of a layer. It may only call tools
    /// its connector lists as read-only.
    pub async fn set_source(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
        input: &SourceInput,
    ) -> Result<(), SettingsError> {
        validate::name("source", name)?;
        let config: SourceConfig = serde_json::from_value(input.definition.clone())
            .map_err(|error| SettingsError::Invalid(format!("invalid source: {error}")))?;
        let resolved = lighthouse_sync::sources::resolve(name, &config, Vec::new())
            .map_err(|error| SettingsError::Invalid(error.to_string()))?;
        let entry = SourceEntry {
            definition: input.definition.clone(),
            enabled: input.enabled,
        };
        self.change(
            owner,
            actor,
            "source",
            name,
            "set",
            async move |snapshot, conn| {
                if let Some(lower) = self.below(&snapshot, owner).sources.get(name) {
                    return Err(conflict(format!(
                        "`{name}` comes from {}; choose another name",
                        layer_name(lower.source)
                    )));
                }
                let view = self.view(&snapshot, owner);
                if let Some(problem) = source_problem(&resolved, &view) {
                    return Err(SettingsError::Invalid(problem));
                }
                let current = snapshot.layer(owner).cloned().unwrap_or_default();
                if !current.sources.contains_key(name) {
                    let limit = self.inner.options.limits.sources;
                    if current.sources.len() >= limit {
                        return Err(SettingsError::Limit(format!(
                            "at most {limit} sources per layer"
                        )));
                    }
                    self.check_total(&snapshot)?;
                }
                self.inner.store.put_source(conn, owner, name, &entry).await
            },
        )
        .await
    }

    /// Deletes a connected source of a layer, with its items.
    pub async fn delete_source(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
    ) -> Result<(), SettingsError> {
        let key = match owner {
            Some(user) => lighthouse_sync::personal_key(name, user),
            None => name.to_owned(),
        };
        self.change(
            owner,
            actor,
            "source",
            name,
            "delete",
            async move |_, conn| {
                if self
                    .inner
                    .store
                    .delete_source(conn, owner, name, &key)
                    .await?
                {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound("source".to_owned()))
                }
            },
        )
        .await
    }

    /// Switches a source on or off: one of the layer itself, or one of the
    /// instance for the whole tenant. A shared source keeps refreshing for the
    /// others, so a person cannot switch it off.
    pub async fn switch_source(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
        enabled: bool,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "switch",
            name,
            "set",
            async move |snapshot, conn| {
                if let Some(mut entry) = snapshot
                    .layer(owner)
                    .and_then(|layer| layer.sources.get(name))
                    .cloned()
                {
                    entry.enabled = enabled;
                    return self.inner.store.put_source(conn, owner, name, &entry).await;
                }
                if !self.below(&snapshot, owner).sources.contains_key(name) {
                    return Err(SettingsError::NotFound("source".to_owned()));
                }
                if owner.is_some() {
                    return Err(conflict(
                        "this source is shared in the workspace; owners and admins switch it",
                    ));
                }
                if enabled {
                    self.inner
                        .store
                        .delete_switch(conn, owner, SwitchKind::Source, name)
                        .await?;
                } else {
                    self.check_total(&snapshot)?;
                    self.inner
                        .store
                        .put_switch(conn, owner, SwitchKind::Source, name, false)
                        .await?;
                }
                Ok(())
            },
        )
        .await
    }

    /// Adds or replaces an agent, automation or skill of a layer. The caller
    /// checks the file. A layer may override one of a lower layer by name.
    pub async fn set_file(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        kind: FileKind,
        name: &str,
        source: &str,
    ) -> Result<(), SettingsError> {
        let entry = FileEntry {
            source: source.to_owned(),
            enabled: true,
        };
        self.change(
            owner,
            actor,
            kind.as_str(),
            name,
            "set",
            async move |snapshot, conn| {
                let current = snapshot.layer(owner).cloned().unwrap_or_default();
                if !current.files.contains_key(&(kind, name.to_owned())) {
                    let limit = self.inner.options.limits.files;
                    if current
                        .files
                        .keys()
                        .filter(|(other, _)| *other == kind)
                        .count()
                        >= limit
                    {
                        return Err(SettingsError::Limit(format!(
                            "at most {limit} of these per layer"
                        )));
                    }
                    self.check_total(&snapshot)?;
                }
                self.inner
                    .store
                    .put_file(conn, owner, kind, name, &entry)
                    .await
            },
        )
        .await
    }

    /// Takes over a file created before these lived in the database. A file
    /// that is already there stays, and limits do not apply, so an upgrade
    /// loses nothing. Returns whether it was added.
    pub async fn import_file(
        &self,
        kind: FileKind,
        name: &str,
        source: &str,
    ) -> Result<bool, SettingsError> {
        let mut added = false;
        let added_ref = &mut added;
        let entry = FileEntry {
            source: source.to_owned(),
            enabled: true,
        };
        self.change(
            None,
            None,
            kind.as_str(),
            name,
            "import",
            async move |snapshot, conn| {
                if !snapshot.tenant.files.contains_key(&(kind, name.to_owned())) {
                    self.inner
                        .store
                        .put_file(conn, None, kind, name, &entry)
                        .await?;
                    *added_ref = true;
                }
                Ok(())
            },
        )
        .await?;
        Ok(added)
    }

    /// Deletes an agent, automation or skill of a layer.
    pub async fn delete_file(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        kind: FileKind,
        name: &str,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            kind.as_str(),
            name,
            "delete",
            async move |_, conn| {
                if self
                    .inner
                    .store
                    .delete_file(conn, owner, kind, name)
                    .await?
                {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound(kind.as_str().to_owned()))
                }
            },
        )
        .await
    }

    /// Switches an agent, automation or skill on or off: one of the layer
    /// itself, or one of a lower layer for this layer only. The caller checks
    /// that a lower one exists.
    pub async fn switch_file(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        kind: FileKind,
        name: &str,
        enabled: bool,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "switch",
            name,
            "set",
            async move |snapshot, conn| {
                if let Some(mut entry) = snapshot
                    .layer(owner)
                    .and_then(|layer| layer.files.get(&(kind, name.to_owned())))
                    .cloned()
                {
                    entry.enabled = enabled;
                    return self
                        .inner
                        .store
                        .put_file(conn, owner, kind, name, &entry)
                        .await;
                }
                if enabled {
                    self.inner
                        .store
                        .delete_switch(conn, owner, kind.switch(), name)
                        .await?;
                } else {
                    self.check_total(&snapshot)?;
                    self.inner
                        .store
                        .put_switch(conn, owner, kind.switch(), name, false)
                        .await?;
                }
                Ok(())
            },
        )
        .await
    }

    /// Deletes a connector of a layer.
    pub async fn delete_connector(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "connector",
            name,
            "delete",
            async move |_, conn| {
                if self.inner.store.delete_connector(conn, owner, name).await? {
                    self.inner
                        .store
                        .remove_personal_tokens(conn, owner, name)
                        .await
                } else {
                    Err(SettingsError::NotFound("connector".to_owned()))
                }
            },
        )
        .await
    }

    /// Switches a connector on or off for a layer: its own one directly, one
    /// of a lower layer for this layer only.
    pub async fn switch_connector(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        name: &str,
        enabled: bool,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "switch",
            name,
            "set",
            async move |snapshot, conn| {
                let own = snapshot
                    .layer(owner)
                    .is_some_and(|layer| layer.connectors.contains_key(name));
                if own {
                    self.inner
                        .store
                        .enable_connector(conn, owner, name, enabled)
                        .await?;
                    return Ok(());
                }
                if !self.below(&snapshot, owner).connectors.contains_key(name) {
                    return Err(SettingsError::NotFound("connector".to_owned()));
                }
                if enabled {
                    self.inner
                        .store
                        .delete_switch(conn, owner, SwitchKind::Connector, name)
                        .await?;
                } else {
                    self.check_total(&snapshot)?;
                    self.inner
                        .store
                        .put_switch(conn, owner, SwitchKind::Connector, name, false)
                        .await?;
                }
                Ok(())
            },
        )
        .await
    }

    /// Sets a single setting of a layer. The caller checks the value.
    pub async fn set_value(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "value",
            key,
            "set",
            async move |snapshot, conn| {
                if !snapshot
                    .layer(owner)
                    .is_some_and(|layer| layer.values.contains_key(key))
                {
                    self.check_total(&snapshot)?;
                }
                self.inner.store.put_value(conn, owner, key, value).await
            },
        )
        .await
    }

    /// Deletes a single setting of a layer, so the lower layer's applies.
    pub async fn delete_value(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        key: &str,
    ) -> Result<(), SettingsError> {
        self.change(
            owner,
            actor,
            "value",
            key,
            "delete",
            async move |_, conn| {
                if self.inner.store.delete_value(conn, owner, key).await? {
                    Ok(())
                } else {
                    Err(SettingsError::NotFound("setting".to_owned()))
                }
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests;
