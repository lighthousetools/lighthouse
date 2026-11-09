//! The providers and connectors of a tenant, resolved for the user the
//! current task acts for.
//!
//! Requests act for their caller, and runs for the user they work for (see
//! `Runtime::as_run_user`). Background work without a user, such as source
//! refreshes, sees the tenant layer. Each call answers from one snapshot of
//! the settings, so a run that asked once keeps a consistent set.

use lighthouse_audit::acting_user;
use lighthouse_mcp::{Connection, ConnectorSource};
use lighthouse_runtime::{KeyPlace, KeyedPlan, PlanSource};
use lighthouse_settings::{Layer, TenantSettings};
use lighthouse_vault::Vault;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type Connections = Arc<BTreeMap<String, Connection>>;

/// Resolves providers and connectors of one tenant per user.
pub struct TenantConfig {
    settings: TenantSettings,
    vault: Vault,
    instance: Option<Vault>,
    connections: Mutex<HashMap<Option<Uuid>, (i64, Connections)>>,
}

impl TenantConfig {
    /// Creates the resolver. `vault` is the tenant's; `instance` is the
    /// instance vault if the tenant may use the instance's provider keys.
    pub fn new(settings: TenantSettings, vault: Vault, instance: Option<Vault>) -> Self {
        Self {
            settings,
            vault,
            instance,
            connections: Mutex::default(),
        }
    }

    /// The settings it reads.
    pub fn settings(&self) -> &TenantSettings {
        &self.settings
    }

    fn vaults(&self, layers: &[Layer], user: Option<Uuid>) -> Vec<Vault> {
        layers
            .iter()
            .filter_map(|layer| match layer {
                Layer::Personal => user.map(|user| self.vault.personal(user)),
                Layer::Tenant => Some(self.vault.clone()),
                Layer::Instance => self.instance.clone(),
            })
            .collect()
    }

    fn resolve_connections(&self, user: Option<Uuid>) -> Connections {
        let effective = self.settings.effective(user);
        Arc::new(
            effective
                .connectors
                .iter()
                .filter(|(_, connector)| connector.enabled)
                .map(|(name, connector)| {
                    let connection = Connection {
                        config: connector.config.clone(),
                        vaults: self.vaults(connector.token_layers(), user),
                        guarded: connector.guarded(),
                    };
                    (name.clone(), connection)
                })
                .collect(),
        )
    }
}

impl ConnectorSource for TenantConfig {
    fn connections(&self) -> Connections {
        let user = acting_user::current();
        let revision = self.settings.snapshot().revision;
        let mut cache = self
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, connections)) = cache.get(&user)
            && *cached == revision
        {
            return connections.clone();
        }
        let connections = self.resolve_connections(user);
        cache.retain(|_, (cached, _)| *cached == revision);
        cache.insert(user, (revision, connections.clone()));
        connections
    }
}

impl PlanSource for TenantConfig {
    fn plan(&self) -> KeyedPlan {
        let user = acting_user::current();
        let effective = self.settings.effective(user);
        let keys = effective
            .providers
            .iter()
            .map(|(name, provider)| {
                let place = KeyPlace {
                    entry: provider.key_name(name),
                    vaults: self.vaults(provider.key_layers(), user),
                };
                (name.clone(), place)
            })
            .collect();
        KeyedPlan {
            plan: effective.router_plan(),
            keys,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use lighthouse_core::TenantId;
    use lighthouse_settings::{ConnectorInput, Defaults, Options, ProviderInput};
    use lighthouse_store::testing::fresh_store;
    use lighthouse_vault::MasterKey;

    #[tokio::test]
    async fn keys_and_connectors_resolve_for_the_acting_user() {
        let store = fresh_store().await;
        let user = Uuid::now_v7();
        lighthouse_store::sqlx::query(
            "INSERT INTO users (id, email, name) VALUES ($1, 'sam@example.com', 'Sam')",
        )
        .bind(user)
        .execute(store.pool())
        .await
        .unwrap();
        let key = base64::engine::general_purpose::STANDARD.encode([3u8; 32]);
        let vault = Vault::new(
            store.clone(),
            TenantId::DEFAULT,
            &MasterKey::from_base64(&key).unwrap(),
        );
        let settings = TenantSettings::open(
            store.clone(),
            TenantId::DEFAULT,
            Arc::new(Defaults::default()),
            Options {
                allow_private_endpoints: true,
                ..Options::default()
            },
        )
        .await
        .unwrap();
        let provider = ProviderInput {
            kind: "openai".to_owned(),
            base_url: None,
        };
        settings
            .set_provider(None, None, "shared", &provider)
            .await
            .unwrap();
        settings
            .set_provider(Some(user), Some(user), "mine", &provider)
            .await
            .unwrap();
        let connector = ConnectorInput {
            url: "http://10.0.0.1/mcp".to_owned(),
            auth: "bearer".to_owned(),
            client_id: None,
            scopes: Vec::new(),
            read_tools: Vec::new(),
            draft_tools: Vec::new(),
            enabled: true,
        };
        settings
            .set_connector(Some(user), Some(user), "mail", &connector)
            .await
            .unwrap();
        let config = TenantConfig::new(settings, vault, None);
        let tenant = config.plan();
        assert_eq!(tenant.keys["shared"].entry, "provider:shared");
        assert_eq!(tenant.keys["shared"].vaults.len(), 1);
        assert!(!tenant.keys.contains_key("mine"));
        assert!(config.connections().is_empty());
        let (plan, connections) =
            acting_user::scope(Some(user), async { (config.plan(), config.connections()) }).await;
        let shared = &plan.keys["shared"].vaults;
        assert_eq!((shared.len(), shared[0].user()), (2, Some(user)));
        assert_eq!(plan.keys["mine"].vaults.len(), 1);
        assert_eq!(plan.keys["mine"].vaults[0].user(), Some(user));
        let mail = &connections["mail"];
        assert_eq!(mail.vaults.len(), 1);
        assert_eq!(mail.vaults[0].user(), Some(user));
        assert!(!mail.guarded);
        let again = acting_user::scope(Some(user), async { config.connections() }).await;
        assert!(
            Arc::ptr_eq(&connections, &again),
            "cached until the next change"
        );
        assert_eq!(config.settings().snapshot().revision, 3);
    }
}
