//! The providers and connectors of a tenant, resolved for the user the
//! current task acts for.
//!
//! Requests act for their caller, and runs for the user they work for (see
//! `Runtime::as_run_user`). Background work without a user, such as source
//! refreshes, sees the tenant layer. Each call answers from one snapshot of
//! the settings, so a run that asked once keeps a consistent set.
//!
//! The same resolver names the person a run works for and lists the sources of
//! a tenant, shared and personal, each resolved for the user it belongs to.

use async_trait::async_trait;
use chrono_tz::Tz;
use lighthouse_audit::acting_user;
use lighthouse_mcp::{Connection, ConnectorSource};
use lighthouse_runtime::{KeyPlace, KeyedPlan, People, Person, PlanSource};
use lighthouse_settings::{
    ACCOUNTS, ADDRESSES, EffectiveSource, Layer, SLACK_CHANNELS, Snapshot, TIMEZONE,
    TenantSettings, source_problem,
};
use lighthouse_sync::{Source, SourceList};
use lighthouse_vault::Vault;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type Connections = Arc<BTreeMap<String, Connection>>;
type CachedSources = (Arc<Snapshot>, Arc<Vec<Source>>);

/// Resolves providers and connectors of one tenant per user.
pub struct TenantConfig {
    settings: TenantSettings,
    vault: Vault,
    instance: Option<Vault>,
    connections: Mutex<HashMap<Option<Uuid>, (i64, Connections)>>,
    /// The person of `[owner]`, in the default tenant.
    instance_owner: Option<InstanceOwner>,
    zone: Tz,
    sources: Mutex<Option<CachedSources>>,
}

/// What `[owner]` says about the first owner of the default tenant.
#[derive(Clone, Debug, Default)]
pub struct InstanceOwner {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
    /// Account names by connector.
    pub accounts: BTreeMap<String, String>,
}

/// One person with what identifies them in services.
struct Identity {
    name: String,
    email: String,
    accounts: BTreeMap<String, String>,
    addresses: Vec<String>,
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
            instance_owner: None,
            zone: chrono_tz::UTC,
            sources: Mutex::default(),
        }
    }

    /// Names the person of `[owner]` and the default time zone.
    #[must_use]
    pub fn with_person_defaults(mut self, owner: Option<InstanceOwner>, zone: Tz) -> Self {
        self.instance_owner = owner;
        self.zone = zone;
        self
    }

    /// Who a user is, or the person the tenant works for without one.
    fn identity(&self, snapshot: &Snapshot, user: Option<Uuid>) -> Option<Identity> {
        let member = match user {
            Some(user) => snapshot.member(user),
            None => snapshot.primary(),
        };
        let primary = snapshot.primary().map(|primary| primary.id);
        let owner = self
            .instance_owner
            .as_ref()
            .filter(|_| member.is_none_or(|member| Some(member.id) == primary));
        let mut identity = match (member, owner) {
            (Some(member), Some(owner)) => Identity {
                name: member.name.clone(),
                email: member.email.clone(),
                accounts: owner.accounts.clone(),
                addresses: Vec::new(),
            },
            (Some(member), None) => Identity {
                name: member.name.clone(),
                email: member.email.clone(),
                accounts: BTreeMap::new(),
                addresses: Vec::new(),
            },
            (None, Some(owner)) => Identity {
                name: owner.name.clone(),
                email: owner.email.clone(),
                accounts: owner.accounts.clone(),
                addresses: Vec::new(),
            },
            (None, None) => return None,
        };
        let values = member.and_then(|member| snapshot.personal.get(&member.id));
        if let Some(values) = values.map(|layer| &layer.values) {
            if let Some(accounts) = values.get(ACCOUNTS).and_then(|value| value.as_object()) {
                for (service, account) in accounts {
                    if let Some(account) = account.as_str() {
                        identity
                            .accounts
                            .insert(service.clone(), account.to_owned());
                    }
                }
            }
            if let Some(addresses) = values.get(ADDRESSES).and_then(|value| value.as_array()) {
                identity.addresses = addresses
                    .iter()
                    .filter_map(|address| address.as_str().map(str::to_owned))
                    .collect();
            }
        }
        Some(identity)
    }

    /// Whether a personal source would read through the tenant's shared account.
    /// A shared mailbox is its first owner's: a member's own source must use
    /// their own account on a shared connector, or its items would show them
    /// what the shared account sees.
    pub async fn needs_own_token(
        &self,
        source: &Source,
    ) -> Result<bool, lighthouse_vault::VaultError> {
        let Some(owner) = source.owner else {
            return Ok(false);
        };
        let effective = self.settings.effective(Some(owner));
        let Some(connector) = effective.connectors.get(&source.connector) else {
            return Ok(false);
        };
        let lighthouse_config::ConnectorTransport::Http { auth, .. } = &connector.config.transport
        else {
            return Ok(false);
        };
        let entry = match auth {
            lighthouse_config::ConnectorAuth::None => return Ok(false),
            lighthouse_config::ConnectorAuth::Bearer => format!("mcp:{}", source.connector),
            lighthouse_config::ConnectorAuth::Oauth => format!("oauth:{}", source.connector),
        };
        Ok(self.vault.personal(owner).get(&entry).await?.is_none())
    }

    /// The time zone of a user, or the tenant's for `None`.
    pub fn zone_of(&self, user: Option<Uuid>) -> Tz {
        self.settings
            .effective(user)
            .values
            .get(TIMEZONE)
            .and_then(|(zone, _)| zone.as_str()?.parse().ok())
            .unwrap_or(self.zone)
    }

    fn resolve_source(&self, snapshot: &Snapshot, source: &EffectiveSource) -> Option<Source> {
        let identity = self.identity(snapshot, source.owner);
        let mut accounts = Vec::new();
        if let Some(identity) = identity {
            accounts.extend(identity.accounts.get(&source.config.connector).cloned());
            if !identity.email.is_empty() {
                accounts.push(identity.email);
            }
            accounts.extend(identity.addresses);
        }
        let mut resolved =
            match lighthouse_sync::sources::resolve(&source.key, &source.config, accounts) {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::warn!(source = %source.key, %error, "cannot use a source");
                    return None;
                }
            };
        resolved.label.clone_from(&source.name);
        resolved.owner = source.owner;
        resolved.problem = source_problem(&resolved, &self.settings.effective(source.owner));
        Some(resolved)
    }

    /// The settings it reads.
    pub fn settings(&self) -> &TenantSettings {
        &self.settings
    }

    pub(crate) fn vaults(&self, layers: &[Layer], user: Option<Uuid>) -> Vec<Vault> {
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

impl lighthouse_notify::SlackSource for TenantConfig {
    fn aliases(&self) -> Vec<String> {
        self.settings
            .effective(acting_user::current())
            .values
            .get(SLACK_CHANNELS)
            .and_then(|(value, _)| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    fn vaults(&self) -> Vec<Vault> {
        self.vaults(&[Layer::Personal, Layer::Tenant], acting_user::current())
    }

    fn guarded(&self) -> bool {
        !self.settings.options().allow_private_endpoints
    }
}

impl SourceList for TenantConfig {
    fn sources(&self) -> Arc<Vec<Source>> {
        let snapshot = self.settings.snapshot();
        let mut cache = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, sources)) = cache.as_ref()
            && Arc::ptr_eq(cached, &snapshot)
        {
            return sources.clone();
        }
        let sources: Arc<Vec<Source>> = Arc::new(
            self.settings
                .all_sources()
                .iter()
                .filter_map(|source| self.resolve_source(&snapshot, source))
                .collect(),
        );
        *cache = Some((snapshot, sources.clone()));
        sources
    }
}

#[async_trait]
impl People for TenantConfig {
    async fn person(&self, user: Option<Uuid>) -> Option<Person> {
        // Someone who just joined may not be in the snapshot yet.
        if user.is_some_and(|user| self.settings.snapshot().member(user).is_none())
            && let Err(error) = self.settings.reload().await
        {
            tracing::warn!(%error, "cannot reload the settings");
        }
        let snapshot = self.settings.snapshot();
        let identity = self.identity(&snapshot, user)?;
        let mut context = format!("You work for {} ({}).", identity.name, identity.email);
        for (service, account) in &identity.accounts {
            context.push_str(&format!(" Their {service} account is {account}."));
        }
        Some(Person {
            context,
            email: identity.email,
            zone: self.zone_of(user),
        })
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
            api: None,
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

    #[tokio::test]
    async fn people_and_their_sources_resolve_from_their_own_settings() {
        let store = fresh_store().await;
        for (id, email, role) in [
            (Uuid::from_u128(1), "alex@example.com", "owner"),
            (Uuid::from_u128(2), "sam@example.com", "member"),
        ] {
            lighthouse_store::sqlx::query(
                "INSERT INTO users (id, email, name) VALUES ($1, $2, 'Person')",
            )
            .bind(id)
            .bind(email)
            .execute(store.pool())
            .await
            .unwrap();
            lighthouse_store::sqlx::query(
                "INSERT INTO memberships (tenant_id, user_id, role) VALUES ($1, $2, $3)",
            )
            .bind(TenantId::DEFAULT.as_uuid())
            .bind(id)
            .bind(role)
            .execute(store.pool())
            .await
            .unwrap();
        }
        let (alex, sam) = (Uuid::from_u128(1), Uuid::from_u128(2));
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
        settings
            .set_value(
                Some(sam),
                Some(sam),
                ACCOUNTS,
                &serde_json::json!({"mail": "sam-99"}),
            )
            .await
            .unwrap();
        settings
            .set_value(
                Some(sam),
                Some(sam),
                ADDRESSES,
                &serde_json::json!(["sam@home.example.com"]),
            )
            .await
            .unwrap();
        settings
            .set_value(
                Some(sam),
                Some(sam),
                TIMEZONE,
                &serde_json::json!("Asia/Tokyo"),
            )
            .await
            .unwrap();
        let connector = ConnectorInput {
            url: "http://10.0.0.1/mcp".to_owned(),
            auth: "none".to_owned(),
            client_id: None,
            scopes: Vec::new(),
            read_tools: vec!["list".to_owned()],
            draft_tools: Vec::new(),
            enabled: true,
            api: None,
        };
        settings
            .set_connector(Some(sam), Some(sam), "mail", &connector)
            .await
            .unwrap();
        let definition = serde_json::json!({"connector": "mail", "domain": "mail", "collections": {
            "inbox": {"tool": "list", "kind": "thread", "fields": {"external_id": "id"}}}});
        settings
            .set_source(
                Some(sam),
                Some(sam),
                "inbox",
                &lighthouse_settings::SourceInput {
                    definition: definition.clone(),
                    enabled: true,
                },
            )
            .await
            .unwrap();
        let owner = InstanceOwner {
            name: "Configured".to_owned(),
            email: "configured@example.com".to_owned(),
            accounts: BTreeMap::from([("linear".to_owned(), "alex-account".to_owned())]),
        };
        let config = TenantConfig::new(settings.clone(), vault, None)
            .with_person_defaults(Some(owner), chrono_tz::Europe::Berlin);
        let first = config.person(None).await.unwrap();
        assert_eq!(first.email, "alex@example.com");
        assert!(
            first.context.contains("linear account is alex-account"),
            "{}",
            first.context
        );
        assert_eq!(first.zone, chrono_tz::Europe::Berlin);
        let them = config.person(Some(sam)).await.unwrap();
        assert_eq!(them.email, "sam@example.com");
        assert!(
            them.context.contains("mail account is sam-99"),
            "{}",
            them.context
        );
        assert!(!them.context.contains("alex-account"), "{}", them.context);
        assert_eq!(them.zone, chrono_tz::Asia::Tokyo);
        assert!(config.person(Some(alex)).await.is_some());
        let sources = config.sources();
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!((source.label.as_str(), source.owner), ("inbox", Some(sam)));
        assert_eq!(source.name, format!("inbox@{sam}"));
        assert!(source.problem.is_none(), "{:?}", source.problem);
        assert_eq!(
            source.accounts,
            ["sam-99", "sam@example.com", "sam@home.example.com"]
        );
        assert!(
            Arc::ptr_eq(&sources, &config.sources()),
            "cached until a change"
        );
        settings
            .switch_connector(Some(sam), Some(sam), "mail", false)
            .await
            .unwrap();
        assert!(
            config.sources()[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("switched off")
        );
    }
}
