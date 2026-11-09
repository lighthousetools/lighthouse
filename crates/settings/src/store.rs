//! Storage of the tenant and personal layers.

use crate::{
    ConnectorEntry, LayerData, Owner, ProviderEntry, SettingsError, Snapshot, SwitchKind, layer_of,
};
use chrono::{DateTime, Utc};
use lighthouse_config::{ConnectorAuth, ModelConfig, ProviderKind, RouteConfig};
use lighthouse_core::TenantId;
use lighthouse_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

/// The conflict target of entries with an owner: tenant, person, then the given columns.
fn target(columns: &str) -> String {
    format!(
        "(tenant_id, (COALESCE(user_id, '00000000-0000-0000-0000-000000000000'::uuid)), {columns})"
    )
}

/// A recorded change. Values are not recorded.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ChangeRecord {
    /// When it happened.
    pub at: DateTime<Utc>,
    /// Who changed it, absent for the admin token.
    pub actor: Option<Uuid>,
    /// `tenant` or `personal`.
    pub layer: String,
    /// What kind of entry: `provider`, `model`, `route`, `connector`,
    /// `switch` or `value`.
    pub area: String,
    /// Which entry.
    pub name: String,
    /// `set` or `delete`.
    pub action: String,
}

/// The stored layers of one tenant.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    store: Store,
    tenant: TenantId,
}

fn kind_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Openai => "openai",
        ProviderKind::Mock => "mock",
        ProviderKind::Demo => "demo",
    }
}

fn parse_kind(text: &str) -> ProviderKind {
    match text {
        "openai" => ProviderKind::Openai,
        _ => ProviderKind::Anthropic,
    }
}

/// The name of an authentication in the database and the API.
pub(crate) fn auth_name(auth: ConnectorAuth) -> &'static str {
    match auth {
        ConnectorAuth::None => "none",
        ConnectorAuth::Bearer => "bearer",
        ConnectorAuth::Oauth => "oauth",
    }
}

pub(crate) fn parse_auth(text: &str) -> Option<ConnectorAuth> {
    match text {
        "none" => Some(ConnectorAuth::None),
        "bearer" => Some(ConnectorAuth::Bearer),
        "oauth" => Some(ConnectorAuth::Oauth),
        _ => None,
    }
}

type ProviderRow = (Uuid, Option<Uuid>, String, String, Option<String>);
type ModelRow = (Uuid, String, String, f64, f64);
type RouteRow = (Option<Uuid>, String, String, Option<String>);
type ConnectorRow = (
    Option<Uuid>,
    String,
    String,
    String,
    Option<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    bool,
);
type SwitchRow = (Option<Uuid>, String, String, bool);
type ChangeRow = (DateTime<Utc>, Option<Uuid>, String, String, String);
type ValueRow = (Option<Uuid>, String, serde_json::Value);

impl SettingsStore {
    /// Creates the store of a tenant.
    pub fn new(store: Store, tenant: TenantId) -> Self {
        Self { store, tenant }
    }

    fn pool(&self) -> &sqlx::PgPool {
        self.store.pool()
    }

    fn id(&self) -> Uuid {
        self.tenant.as_uuid()
    }

    /// The current revision. Zero before the first change.
    pub async fn revision(&self) -> Result<i64, SettingsError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT revision FROM settings_revisions WHERE tenant_id = $1")
                .bind(self.id())
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map_or(0, |(revision,)| revision))
    }

    /// Raises the revision and returns the new one.
    pub async fn bump(&self) -> Result<i64, SettingsError> {
        let (revision,): (i64,) = sqlx::query_as(
            "INSERT INTO settings_revisions (tenant_id, revision) VALUES ($1, 1) \
             ON CONFLICT (tenant_id) DO UPDATE SET revision = settings_revisions.revision + 1 \
             RETURNING revision",
        )
        .bind(self.id())
        .fetch_one(self.pool())
        .await?;
        Ok(revision)
    }

    /// Reads every layer of the tenant.
    pub async fn load(&self) -> Result<Snapshot, SettingsError> {
        let revision = self.revision().await?;
        let mut snapshot = Snapshot {
            revision,
            ..Snapshot::default()
        };
        let providers: Vec<ProviderRow> = sqlx::query_as(
            "SELECT id, user_id, name, kind, base_url FROM settings_providers WHERE tenant_id = $1",
        )
        .bind(self.id())
        .fetch_all(self.pool())
        .await?;
        let mut owners: HashMap<Uuid, (Owner, String)> = HashMap::new();
        for (id, user, name, kind, base_url) in providers {
            owners.insert(id, (user, name.clone()));
            layer(&mut snapshot, user).providers.insert(
                name,
                ProviderEntry {
                    kind: parse_kind(&kind),
                    base_url,
                    models: std::collections::BTreeMap::new(),
                },
            );
        }
        let models: Vec<ModelRow> = sqlx::query_as(
            "SELECT provider_id, task, model, input_eur_per_mtok, output_eur_per_mtok \
             FROM settings_models WHERE tenant_id = $1",
        )
        .bind(self.id())
        .fetch_all(self.pool())
        .await?;
        for (provider, task, model, input, output) in models {
            let Some((user, name)) = owners.get(&provider) else {
                continue;
            };
            if let Some(entry) = layer(&mut snapshot, *user).providers.get_mut(name) {
                entry.models.insert(
                    task,
                    ModelConfig {
                        model,
                        input_eur_per_mtok: input,
                        output_eur_per_mtok: output,
                    },
                );
            }
        }
        let routes: Vec<RouteRow> = sqlx::query_as(
            "SELECT user_id, task, provider, fallback FROM settings_routes WHERE tenant_id = $1",
        )
        .bind(self.id())
        .fetch_all(self.pool())
        .await?;
        for (user, task, provider, fallback) in routes {
            layer(&mut snapshot, user)
                .routes
                .insert(task, RouteConfig { provider, fallback });
        }
        let connectors: Vec<ConnectorRow> = sqlx::query_as(
            "SELECT user_id, name, url, auth, client_id, scopes, read_tools, draft_tools, enabled \
             FROM settings_connectors WHERE tenant_id = $1",
        )
        .bind(self.id())
        .fetch_all(self.pool())
        .await?;
        for (user, name, url, auth, client_id, scopes, read_tools, draft_tools, enabled) in
            connectors
        {
            layer(&mut snapshot, user).connectors.insert(
                name,
                ConnectorEntry {
                    url,
                    auth: parse_auth(&auth).unwrap_or_default(),
                    client_id,
                    scopes,
                    read_tools,
                    draft_tools,
                    enabled,
                },
            );
        }
        let switches: Vec<SwitchRow> = sqlx::query_as(
            "SELECT user_id, kind, name, enabled FROM settings_switches WHERE tenant_id = $1",
        )
        .bind(self.id())
        .fetch_all(self.pool())
        .await?;
        for (user, kind, name, enabled) in switches {
            if let Some(kind) = SwitchKind::parse(&kind) {
                layer(&mut snapshot, user)
                    .switches
                    .insert((kind, name), enabled);
            }
        }
        let values: Vec<ValueRow> =
            sqlx::query_as("SELECT user_id, key, value FROM settings_values WHERE tenant_id = $1")
                .bind(self.id())
                .fetch_all(self.pool())
                .await?;
        for (user, key, value) in values {
            layer(&mut snapshot, user).values.insert(key, value);
        }
        Ok(snapshot)
    }

    /// Adds a provider or changes its kind and URL. Its models stay.
    pub async fn put_provider(
        &self,
        owner: Owner,
        name: &str,
        kind: ProviderKind,
        base_url: Option<&str>,
    ) -> Result<(), SettingsError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO settings_providers (id, tenant_id, user_id, name, kind, base_url) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT {} \
             DO UPDATE SET kind = $5, base_url = $6, updated_at = now()",
            target("name")
        )))
        .bind(Uuid::now_v7())
        .bind(self.id())
        .bind(owner)
        .bind(name)
        .bind(kind_name(kind))
        .bind(base_url)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Deletes a provider with its models. Returns whether it existed.
    pub async fn delete_provider(&self, owner: Owner, name: &str) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_providers WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND name = $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(name)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Sets the model of a provider for a task. Returns `false` if the
    /// provider does not exist in this layer.
    pub async fn put_model(
        &self,
        owner: Owner,
        provider: &str,
        task: &str,
        model: &ModelConfig,
    ) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "INSERT INTO settings_models \
             (tenant_id, provider_id, task, model, input_eur_per_mtok, output_eur_per_mtok) \
             SELECT tenant_id, id, $4, $5, $6, $7 FROM settings_providers \
             WHERE tenant_id = $1 AND user_id IS NOT DISTINCT FROM $2 AND name = $3 \
             ON CONFLICT (provider_id, task) DO UPDATE SET model = $5, \
             input_eur_per_mtok = $6, output_eur_per_mtok = $7, updated_at = now()",
        )
        .bind(self.id())
        .bind(owner)
        .bind(provider)
        .bind(task)
        .bind(&model.model)
        .bind(model.input_eur_per_mtok)
        .bind(model.output_eur_per_mtok)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes the model of a provider for a task. Returns whether it existed.
    pub async fn delete_model(
        &self,
        owner: Owner,
        provider: &str,
        task: &str,
    ) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_models WHERE tenant_id = $1 AND task = $4 AND provider_id IN \
             (SELECT id FROM settings_providers WHERE tenant_id = $1 \
              AND user_id IS NOT DISTINCT FROM $2 AND name = $3)",
        )
        .bind(self.id())
        .bind(owner)
        .bind(provider)
        .bind(task)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Sets the route of a task.
    pub async fn put_route(
        &self,
        owner: Owner,
        task: &str,
        route: &RouteConfig,
    ) -> Result<(), SettingsError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO settings_routes (tenant_id, user_id, task, provider, fallback) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT {} \
             DO UPDATE SET provider = $4, fallback = $5, updated_at = now()",
            target("task")
        )))
        .bind(self.id())
        .bind(owner)
        .bind(task)
        .bind(&route.provider)
        .bind(&route.fallback)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Deletes the route of a task. Returns whether it existed.
    pub async fn delete_route(&self, owner: Owner, task: &str) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_routes WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND task = $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(task)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Adds or replaces a connector.
    pub async fn put_connector(
        &self,
        owner: Owner,
        name: &str,
        connector: &ConnectorEntry,
    ) -> Result<(), SettingsError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO settings_connectors (id, tenant_id, user_id, name, url, auth, client_id, \
             scopes, read_tools, draft_tools, enabled) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT {} \
             DO UPDATE SET url = $5, auth = $6, client_id = $7, scopes = $8, read_tools = $9, \
             draft_tools = $10, enabled = $11, updated_at = now()",
            target("name")
        )))
        .bind(Uuid::now_v7())
        .bind(self.id())
        .bind(owner)
        .bind(name)
        .bind(&connector.url)
        .bind(auth_name(connector.auth))
        .bind(&connector.client_id)
        .bind(&connector.scopes)
        .bind(&connector.read_tools)
        .bind(&connector.draft_tools)
        .bind(connector.enabled)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Switches a connector of this layer on or off. Returns whether it exists.
    pub async fn enable_connector(
        &self,
        owner: Owner,
        name: &str,
        enabled: bool,
    ) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "UPDATE settings_connectors SET enabled = $4, updated_at = now() WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND name = $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(name)
        .bind(enabled)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes a connector. Returns whether it existed.
    pub async fn delete_connector(&self, owner: Owner, name: &str) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_connectors WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND name = $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(name)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Removes members' own tokens of a connector name, so they never reach an
    /// endpoint they did not connect to. With an owner, only theirs.
    pub async fn remove_personal_tokens(
        &self,
        owner: Owner,
        name: &str,
    ) -> Result<(), SettingsError> {
        sqlx::query(
            "DELETE FROM credentials WHERE tenant_id = $1 AND user_id IS NOT NULL \
             AND ($2::uuid IS NULL OR user_id = $2) AND name = ANY($3)",
        )
        .bind(self.id())
        .bind(owner)
        .bind([format!("mcp:{name}"), format!("oauth:{name}")].as_slice())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Switches an entry of a lower layer on or off for this layer.
    pub async fn put_switch(
        &self,
        owner: Owner,
        kind: SwitchKind,
        name: &str,
        enabled: bool,
    ) -> Result<(), SettingsError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO settings_switches (tenant_id, user_id, kind, name, enabled) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT {} \
             DO UPDATE SET enabled = $5, updated_at = now()",
            target("kind, name")
        )))
        .bind(self.id())
        .bind(owner)
        .bind(kind.as_str())
        .bind(name)
        .bind(enabled)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Removes a switch, so the entry is as the lower layer has it.
    pub async fn delete_switch(
        &self,
        owner: Owner,
        kind: SwitchKind,
        name: &str,
    ) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_switches WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND kind = $3 AND name = $4",
        )
        .bind(self.id())
        .bind(owner)
        .bind(kind.as_str())
        .bind(name)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Sets a single setting.
    pub async fn put_value(
        &self,
        owner: Owner,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), SettingsError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO settings_values (tenant_id, user_id, key, value) VALUES ($1, $2, $3, $4) \
             ON CONFLICT {} DO UPDATE SET value = $4, updated_at = now()",
            target("key")
        )))
        .bind(self.id())
        .bind(owner)
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Deletes a single setting. Returns whether it existed.
    pub async fn delete_value(&self, owner: Owner, key: &str) -> Result<bool, SettingsError> {
        let result = sqlx::query(
            "DELETE FROM settings_values WHERE tenant_id = $1 \
             AND user_id IS NOT DISTINCT FROM $2 AND key = $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(key)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records a change. Values are not recorded.
    pub async fn record(
        &self,
        owner: Owner,
        actor: Option<Uuid>,
        area: &str,
        name: &str,
        action: &str,
    ) -> Result<(), SettingsError> {
        sqlx::query(
            "INSERT INTO settings_changes (id, tenant_id, user_id, actor, area, name, action) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(self.id())
        .bind(owner)
        .bind(actor)
        .bind(area)
        .bind(name)
        .bind(action)
        .execute(self.pool())
        .await?;
        tracing::info!(
            tenant = %self.id(),
            layer = layer_of(owner).as_str(),
            area,
            name,
            action,
            "settings changed"
        );
        Ok(())
    }

    /// The latest changes of one layer, newest first.
    pub async fn changes(
        &self,
        owner: Owner,
        limit: i64,
    ) -> Result<Vec<ChangeRecord>, SettingsError> {
        let rows: Vec<ChangeRow> = sqlx::query_as(
            "SELECT created_at, actor, area, name, action FROM settings_changes \
             WHERE tenant_id = $1 AND user_id IS NOT DISTINCT FROM $2 \
             ORDER BY created_at DESC LIMIT $3",
        )
        .bind(self.id())
        .bind(owner)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(at, actor, area, name, action)| ChangeRecord {
                at,
                actor,
                layer: layer_of(owner).as_str().to_owned(),
                area,
                name,
                action,
            })
            .collect())
    }
}

fn layer(snapshot: &mut Snapshot, user: Owner) -> &mut LayerData {
    match user {
        None => &mut snapshot.tenant,
        Some(user) => snapshot.personal.entry(user).or_default(),
    }
}
