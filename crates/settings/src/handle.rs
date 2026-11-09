//! The settings of one tenant in memory, kept current.

use crate::effective::{Defaults, Effective};
use crate::store::{ChangeRecord, SettingsStore, parse_auth};
use crate::{
    ConnectorEntry, ConnectorInput, Layer, LayerData, ModelInput, Owner, ProviderInput, RouteInput,
    SettingsError, Snapshot, SwitchKind, validate,
};
use lighthouse_config::{Config, ModelConfig, RouteConfig, SettingsLimits};
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
    /// One change at a time in this process, so checks see the latest state.
    writing: tokio::sync::Mutex<()>,
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
                writing: tokio::sync::Mutex::new(()),
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
        Effective::resolve(
            &self.inner.defaults,
            Some(&snapshot.tenant),
            user.and_then(|user| snapshot.personal.get(&user)),
            self.inner.options.allow_private_endpoints,
        )
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
        Ok(())
    }

    /// Reloads if another process changed something. Returns whether it did.
    pub async fn refresh(&self) -> Result<bool, SettingsError> {
        let revision = self.inner.store.revision().await?;
        if revision == self.snapshot().revision {
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

    /// Runs a change: checks against the latest state, writes, records and
    /// swaps the new state in.
    async fn change<F, Fut>(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        area: &str,
        name: &str,
        action: &str,
        write: F,
    ) -> Result<(), SettingsError>
    where
        F: FnOnce(Arc<Snapshot>) -> Fut,
        Fut: std::future::Future<Output = Result<(), SettingsError>>,
    {
        let _writing = self.inner.writing.lock().await;
        self.refresh().await?;
        write(self.snapshot()).await?;
        self.inner
            .store
            .record(owner, actor, area, name, action)
            .await?;
        self.inner.store.bump().await?;
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
        self.change(owner, actor, "provider", name, "set", |snapshot| async move {
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
                .put_provider(owner, name, kind, base_url.as_deref())
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
            |snapshot| async move {
                self.routes_still_work(&snapshot, owner, |layer| {
                    layer.providers.remove(name);
                })?;
                if self.inner.store.delete_provider(owner, name).await? {
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
        self.change(owner, actor, "model", &name, "set", |snapshot| async move {
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
                        .put_provider(owner, provider, lower.kind, lower.base_url.as_deref())
                        .await?;
                }
            }
            self.check_total(&snapshot)?;
            self.inner
                .store
                .put_model(owner, provider, task, &model)
                .await?;
            Ok(())
        })
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
            |snapshot| async move {
                self.routes_still_work(&snapshot, owner, |layer| {
                    if let Some(entry) = layer.providers.get_mut(provider) {
                        entry.models.remove(task);
                    }
                })?;
                if self.inner.store.delete_model(owner, provider, task).await? {
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
        self.change(owner, actor, "route", task, "set", |snapshot| async move {
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
            self.inner.store.put_route(owner, task, &route).await
        })
        .await
    }

    /// Deletes the route of a task in a layer, so the lower layer's applies.
    pub async fn delete_route(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        task: &str,
    ) -> Result<(), SettingsError> {
        self.change(owner, actor, "route", task, "delete", |_| async move {
            if self.inner.store.delete_route(owner, task).await? {
                Ok(())
            } else {
                Err(SettingsError::NotFound("route".to_owned()))
            }
        })
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
        let url =
            validate::endpoint(&input.url, self.inner.options.allow_private_endpoints).await?;
        let entry = ConnectorEntry {
            url,
            auth,
            client_id: input.client_id.clone(),
            scopes: input.scopes.clone(),
            read_tools: input.read_tools.clone(),
            draft_tools: input.draft_tools.clone(),
            enabled: input.enabled,
        };
        self.change(
            owner,
            actor,
            "connector",
            name,
            "set",
            |snapshot| async move {
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
                self.inner.store.put_connector(owner, name, &entry).await
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
        self.change(owner, actor, "connector", name, "delete", |_| async move {
            if self.inner.store.delete_connector(owner, name).await? {
                Ok(())
            } else {
                Err(SettingsError::NotFound("connector".to_owned()))
            }
        })
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
        self.change(owner, actor, "switch", name, "set", |snapshot| async move {
            let own = snapshot
                .layer(owner)
                .is_some_and(|layer| layer.connectors.contains_key(name));
            if own {
                self.inner
                    .store
                    .enable_connector(owner, name, enabled)
                    .await?;
                return Ok(());
            }
            if !self.below(&snapshot, owner).connectors.contains_key(name) {
                return Err(SettingsError::NotFound("connector".to_owned()));
            }
            if enabled {
                self.inner
                    .store
                    .delete_switch(owner, SwitchKind::Connector, name)
                    .await?;
            } else {
                self.check_total(&snapshot)?;
                self.inner
                    .store
                    .put_switch(owner, SwitchKind::Connector, name, false)
                    .await?;
            }
            Ok(())
        })
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
        self.change(owner, actor, "value", key, "set", |snapshot| async move {
            if !snapshot
                .layer(owner)
                .is_some_and(|layer| layer.values.contains_key(key))
            {
                self.check_total(&snapshot)?;
            }
            self.inner.store.put_value(owner, key, value).await
        })
        .await
    }

    /// Deletes a single setting of a layer, so the lower layer's applies.
    pub async fn delete_value(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        key: &str,
    ) -> Result<(), SettingsError> {
        self.change(owner, actor, "value", key, "delete", |_| async move {
            if self.inner.store.delete_value(owner, key).await? {
                Ok(())
            } else {
                Err(SettingsError::NotFound("setting".to_owned()))
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
