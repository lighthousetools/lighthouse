//! Wiring of the Lighthouse server.

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use lighthouse_api::hooks::{Action, AllowAll, Entitlements, Lifecycle, NoHooks};
use lighthouse_api::{
    AgentChangeError, AgentInfo, AgentTool, ApiState, AskAnswer, Assistant, Catalog, ConnectorInfo,
    Extensions, Instance, ModelRoute, RunTrigger, SignupMode, SkillInfo, TriggerError,
};
use lighthouse_audit::{AuditError, AuditLog, RunEnd, RunStatus};
use lighthouse_auth::Accounts;
use lighthouse_brain::{Brain, BrainToolFactory, Embedder, MediaError, MediaReader, SearchHit};
use lighthouse_config::{Config, ConnectorAuth, ConnectorTransport};
use lighthouse_core::agent::{AgentDefinition, Delivery, ToolLevel, Trigger, parse_cron};
use lighthouse_core::{RunId, TenantId};
use lighthouse_desk::Desk;
use lighthouse_mcp::{McpConnector, OAuthFlows, ToolHostFactory, ToolPolicy};
use lighthouse_notify::{ChannelNotifier, EventBus};
use lighthouse_router::media::OpenAiMedia;
use lighthouse_runtime::{
    AgentRegistry, AutomationRegistry, DeskToolFactory, LoadedAgent, RegistryError, Runtime,
    VaultRouterSource,
};
use lighthouse_scheduler::{AgentRunner, ScheduleHandle, Scheduler};
use lighthouse_sessions::Sessions;
use lighthouse_skills::SkillLibrary;
use lighthouse_store::Store;
use lighthouse_vault::{MasterKey, Vault};
use secrecy::ExposeSecret;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

pub mod automations;
pub mod connected;
pub mod outcomes;
pub mod publishing;
pub mod weather;

use automations::{AutomationEngine, ServerAutomations};
use connected::ConnectedService;
use lighthouse_core::automation::RUN_PREFIX;
use outcomes::{OutcomeParts, Outcomes, Pushes};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);

/// Reads the master key from the configuration.
pub fn master_key(config: &Config) -> anyhow::Result<MasterKey> {
    if let Some(key) = &config.vault.master_key {
        return Ok(MasterKey::from_base64(key.expose_secret())?);
    }
    let path = config
        .vault
        .master_key_file
        .as_ref()
        .context("configure vault.master_key_file or vault.master_key")?;
    MasterKey::from_file(path)
        .with_context(|| format!("cannot use master key file {}", path.display()))
}

/// Starts runs of the loaded agents in the background.
pub struct AgentPool {
    tenant: TenantId,
    entitlements: Arc<dyn Entitlements>,
    desk: Desk,
    runtime: Arc<Runtime>,
    agents: Arc<AgentRegistry>,
    outcomes: Arc<Outcomes>,
    engine: Arc<AutomationEngine>,
    connected: Arc<ConnectedService>,
}

async fn after_run(outcomes: &Outcomes, engine: &Arc<AutomationEngine>, run: RunId) {
    match outcomes.after_run(run.as_uuid()).await {
        Ok(Some(record)) => engine.after_agent_run(&record).await,
        Ok(None) => {}
        Err(error) => {
            tracing::error!(run = %run.as_uuid(), %error, "cannot handle the end of a run");
        }
    }
}

impl AgentPool {
    /// Asks the entitlements whether the agent may start a run now.
    async fn allowed(&self, agent_id: &str) -> Result<(), TriggerError> {
        let action = Action::RunAgent {
            agent_id: agent_id.to_owned(),
        };
        self.entitlements
            .check(self.tenant, &action)
            .await
            .map_err(TriggerError::Refused)
    }

    async fn spawn(&self, agent: Arc<AgentDefinition>, trigger: &str) -> Result<RunId, AuditError> {
        self.spawn_with(agent, trigger, None).await
    }

    async fn spawn_with(
        &self,
        agent: Arc<AgentDefinition>,
        trigger: &str,
        context: Option<String>,
    ) -> Result<RunId, AuditError> {
        let run = RunId::new();
        self.runtime.register(run, &agent, trigger).await?;
        let runtime = self.runtime.clone();
        let outcomes = self.outcomes.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime
                .execute_with_context(run, &agent, context.as_deref())
                .await
            {
                tracing::error!(agent = %agent.id, %error, "run failed");
            }
            after_run(&outcomes, &engine, run).await;
        });
        Ok(run)
    }
}

impl AgentPool {
    async fn resume_run(&self, run: Uuid) -> anyhow::Result<()> {
        let audit = &self.runtime.audit;
        let Some(checkpoint) = audit.take_checkpoint(run).await? else {
            return Ok(());
        };
        let id = RunId::from_uuid(run);
        if checkpoint
            .agent_id
            .starts_with(lighthouse_runtime::ACTION_PREFIX)
        {
            let runtime = self.runtime.clone();
            let outcomes = self.outcomes.clone();
            let connected = self.connected.clone();
            let desk = self.desk.clone();
            tokio::spawn(async move {
                if let Err(error) = runtime.resume_action(id, checkpoint.state).await {
                    tracing::error!(%error, "resumed action failed");
                }
                if publishing::is_publishing(&checkpoint.agent_id)
                    && let Err(error) = publishing::finish_publishing(
                        &runtime.audit,
                        &desk,
                        &checkpoint.agent_id,
                        id.as_uuid(),
                    )
                    .await
                {
                    tracing::error!(%error, "cannot record the publishing outcome");
                }
                if let Err(error) = outcomes.after_run(id.as_uuid()).await {
                    tracing::error!(%error, "cannot handle the end of an action");
                }
                connected.refresh_after(&checkpoint.agent_id);
            });
            return Ok(());
        }
        if checkpoint.agent_id.starts_with(RUN_PREFIX) {
            self.engine
                .resume(id, &checkpoint.agent_id, checkpoint.state)
                .await;
            return Ok(());
        }
        let Some(agent) = self
            .agents
            .get(&checkpoint.agent_id)
            .map(|known| known.agent)
        else {
            let reason = format!("the agent `{}` no longer exists", checkpoint.agent_id);
            let end = RunEnd {
                status: RunStatus::Failed,
                reason: Some(&reason),
                report: None,
            };
            audit.finish_run(id, end).await?;
            after_run(&self.outcomes, &self.engine, id).await;
            return Ok(());
        };
        let runtime = self.runtime.clone();
        let outcomes = self.outcomes.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.resume(id, &agent, checkpoint.state).await {
                tracing::error!(agent = %agent.id, %error, "resumed run failed");
            }
            after_run(&outcomes, &engine, id).await;
        });
        Ok(())
    }

    /// Expires overdue approvals and resumes runs that wait for nothing anymore.
    ///
    /// Resuming from here also recovers runs when the server stopped right
    /// after a decision.
    pub async fn maintain(&self) -> anyhow::Result<()> {
        let audit = &self.runtime.audit;
        audit.expire_approvals().await?;
        for run in audit.resumable_runs().await? {
            self.resume_run(run).await?;
        }
        self.outcomes.check_nodes().await?;
        Ok(())
    }

    /// Runs the maintenance until the process ends.
    pub async fn run_maintenance(self: Arc<Self>) {
        tokio::spawn(self.connected.clone().run());
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = self.maintain().await {
                tracing::error!(%error, "maintenance failed");
            }
        }
    }
}

#[async_trait]
impl RunTrigger for AgentPool {
    async fn trigger(&self, agent_id: &str) -> Result<Uuid, TriggerError> {
        let known = self
            .agents
            .get(agent_id)
            .ok_or_else(|| TriggerError::UnknownAgent(agent_id.to_owned()))?;
        self.allowed(agent_id).await?;
        let run = self
            .spawn(known.agent, "manual")
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(run.as_uuid())
    }

    async fn trigger_with_context(
        &self,
        agent_id: &str,
        context: &str,
    ) -> Result<Uuid, TriggerError> {
        let known = self
            .agents
            .get(agent_id)
            .ok_or_else(|| TriggerError::UnknownAgent(agent_id.to_owned()))?;
        self.allowed(agent_id).await?;
        let run = self
            .spawn_with(known.agent, "task", Some(context.to_owned()))
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(run.as_uuid())
    }

    async fn resume(&self, run: Uuid) -> Result<(), TriggerError> {
        self.resume_run(run)
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))
    }
}

#[async_trait]
impl AgentRunner for AgentPool {
    async fn start(&self, agent_id: &str, trigger: &str) {
        if agent_id.starts_with(RUN_PREFIX) {
            self.engine.start_scheduled(agent_id).await;
            return;
        }
        let Some(known) = self.agents.get(agent_id) else {
            return;
        };
        if let Err(error) = self.allowed(agent_id).await {
            tracing::warn!(agent = %agent_id, %error, "run not allowed");
            return;
        }
        if let Err(error) = self.spawn(known.agent, trigger).await {
            tracing::error!(agent = %agent_id, %error, "cannot start run");
        }
    }
}

/// Answers what the apps ask about agents and connections.
struct ServerCatalog {
    config: Arc<Config>,
    vault: Vault,
    runtime: Arc<Runtime>,
    agents: Arc<AgentRegistry>,
    schedules: ScheduleHandle,
    zone: Tz,
}

/// The provider and model of a task, from `[routing]` and `[providers]`.
fn model_route(config: &Config, task: &str) -> Option<ModelRoute> {
    let route = config.routing.get(task)?;
    let model = |provider: &str| {
        config
            .providers
            .get(provider)
            .and_then(|provider| provider.tasks.get(task))
            .map(|model| model.model.clone())
    };
    Some(ModelRoute {
        task: task.to_owned(),
        model: model(&route.provider),
        provider: route.provider.clone(),
        fallback_model: route.fallback.as_deref().and_then(model),
        fallback: route.fallback.clone(),
    })
}

fn level_name(level: ToolLevel) -> &'static str {
    match level {
        ToolLevel::Read => "read",
        ToolLevel::Draft => "draft",
        ToolLevel::Write => "write",
    }
}

fn delivery_name(delivery: &Delivery) -> String {
    match delivery {
        Delivery::Owner => "owner".to_owned(),
        Delivery::Mail(address) => address.clone(),
        Delivery::Slack(alias) => format!("slack:{alias}"),
    }
}

impl ServerCatalog {
    fn next_run(&self, expression: &str) -> Option<DateTime<Utc>> {
        let now = Utc::now().with_timezone(&self.zone);
        let next = parse_cron(expression)
            .ok()?
            .find_next_occurrence(&now, false)
            .ok()?;
        Some(next.with_timezone(&Utc))
    }

    fn info(&self, known: &LoadedAgent) -> AgentInfo {
        let agent = &known.agent;
        let tools: Vec<AgentTool> = agent
            .tools
            .iter()
            .map(|grant| AgentTool {
                name: grant.name.clone(),
                level: level_name(grant.level).to_owned(),
            })
            .collect();
        let schedule = match &agent.trigger {
            Trigger::Cron(expression) => Some(expression.clone()),
            Trigger::Manual => None,
        };
        AgentInfo {
            id: agent.id.clone(),
            name: agent.name.clone(),
            next_run: schedule.as_deref().and_then(|cron| self.next_run(cron)),
            schedule,
            autonomy: lighthouse_api::autonomy(&tools).to_owned(),
            tools,
            model: model_route(&self.config, "text"),
            max_cost_eur: agent.limits.max_cost_eur,
            corrections: 0,
            skills: agent.skills.clone(),
            deliver: agent.deliver.iter().map(delivery_name).collect(),
            blocked: self
                .runtime
                .policy
                .blocked(agent)
                .into_iter()
                .map(|tool| format!("{}: {}", tool.tool, tool.reason))
                .collect(),
            editable: known.editable,
            source: known.source.clone(),
        }
    }

    fn refresh_schedules(&self) -> Result<(), AgentChangeError> {
        let agents: Vec<AgentDefinition> = self
            .agents
            .list()
            .iter()
            .map(|known| (*known.agent).clone())
            .collect();
        self.schedules
            .update(&agents)
            .map_err(|error| AgentChangeError::Failed(error.to_string()))
    }

    fn known_tools(&self) -> Vec<String> {
        let mut tools = vec![
            "brain.search".to_owned(),
            "brain.get".to_owned(),
            "brain.remember: draft".to_owned(),
            "brain.link: draft".to_owned(),
            "tasks.propose: draft".to_owned(),
            "memory.propose: draft".to_owned(),
            "posts.propose: draft".to_owned(),
        ];
        for (name, connector) in &self.config.connectors {
            tools.extend(
                connector
                    .read_tools
                    .iter()
                    .map(|tool| format!("{name}.{tool}")),
            );
            tools.extend(
                connector
                    .draft_tools
                    .iter()
                    .map(|tool| format!("{name}.{tool}: draft")),
            );
        }
        tools
    }
}

fn registry_error(error: RegistryError) -> AgentChangeError {
    match error {
        RegistryError::Io(_) => AgentChangeError::Failed(error.to_string()),
        other => AgentChangeError::Rejected(other.to_string()),
    }
}

#[async_trait]
impl Catalog for ServerCatalog {
    fn agents(&self) -> Vec<AgentInfo> {
        self.agents
            .list()
            .iter()
            .map(|known| self.info(known))
            .collect()
    }

    fn agent(&self, id: &str) -> Option<AgentInfo> {
        self.agents.get(id).map(|known| self.info(&known))
    }

    async fn draft_agent(&self, description: &str) -> Result<String, String> {
        self.runtime
            .draft_agent(description, &self.known_tools())
            .await
    }

    async fn save_agent(&self, source: &str) -> Result<AgentInfo, AgentChangeError> {
        let agent = self.agents.save(source).map_err(registry_error)?;
        self.refresh_schedules()?;
        self.agents
            .get(&agent.id)
            .map(|known| self.info(&known))
            .ok_or_else(|| AgentChangeError::Failed("the agent did not load".to_owned()))
    }

    async fn delete_agent(&self, id: &str) -> Result<bool, AgentChangeError> {
        let deleted = self.agents.delete(id).map_err(registry_error)?;
        self.refresh_schedules()?;
        Ok(deleted)
    }

    async fn connectors(&self) -> Result<Vec<ConnectorInfo>, String> {
        let stored = self.vault.list().await.map_err(|error| error.to_string())?;
        Ok(self
            .config
            .connectors
            .iter()
            .map(|(name, connector)| {
                let has = |key: String| stored.contains(&key);
                let (auth, connected) = match &connector.transport {
                    ConnectorTransport::Stdio { .. } => ("process", true),
                    ConnectorTransport::Http { auth, .. } => match auth {
                        ConnectorAuth::None => ("none", true),
                        ConnectorAuth::Bearer => ("bearer", has(format!("mcp:{name}"))),
                        ConnectorAuth::Oauth => ("oauth", has(format!("oauth:{name}"))),
                    },
                };
                ConnectorInfo {
                    name: name.clone(),
                    auth: auth.to_owned(),
                    connected,
                    read_tools: connector.read_tools.clone(),
                    draft_tools: connector.draft_tools.clone(),
                }
            })
            .collect())
    }

    fn models(&self) -> Vec<ModelRoute> {
        self.config
            .routing
            .keys()
            .filter_map(|task| model_route(&self.config, task))
            .collect()
    }

    fn skills(&self) -> Vec<SkillInfo> {
        let library = &self.runtime.skills;
        library
            .names()
            .filter_map(|name| library.get(name))
            .map(|skill| SkillInfo {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }
}

/// Number of entries a question is answered from.
const ASK_ENTRIES: usize = 6;

/// Answers questions from the Brain.
struct BrainAssistant {
    brain: Brain,
    runtime: Arc<Runtime>,
}

impl BrainAssistant {
    async fn context(&self, question: &str) -> Result<(Vec<SearchHit>, String), String> {
        let hits = self
            .brain
            .search(question, ASK_ENTRIES)
            .await
            .map_err(|error| error.to_string())?;
        let mut context = String::new();
        for hit in &hits {
            if let Ok(Some(entry)) = self.brain.get(hit.entry.id).await {
                let _ = writeln!(context, "## {}\n{}\n", entry.title, entry.body);
            }
        }
        Ok((hits, context))
    }
}

#[async_trait]
impl Assistant for BrainAssistant {
    async fn ask(&self, question: &str) -> Result<AskAnswer, String> {
        let (hits, context) = self.context(question).await?;
        let (run, answer) = self.runtime.ask(question, &context).await?;
        Ok(AskAnswer {
            run_id: run.as_uuid(),
            answer,
            sources: hits.into_iter().map(|hit| hit.entry).collect(),
        })
    }

    async fn ask_stream(
        &self,
        question: &str,
        deltas: UnboundedSender<String>,
    ) -> Result<AskAnswer, String> {
        let (hits, context) = self.context(question).await?;
        let sink = move |text: String| {
            let _ = deltas.send(text);
        };
        let (run, answer) = self
            .runtime
            .ask_streaming(question, &context, &sink)
            .await?;
        Ok(AskAnswer {
            run_id: run.as_uuid(),
            answer,
            sources: hits.into_iter().map(|hit| hit.entry).collect(),
        })
    }
}

/// Reaches the configured provider, with the key from the vault on every call.
struct ProviderMedia {
    config: Arc<Config>,
    vault: Vault,
    fallback: Option<Vault>,
    provider: String,
}

impl ProviderMedia {
    async fn client(&self) -> Result<OpenAiMedia, MediaError> {
        let key =
            lighthouse_runtime::provider_key(&self.vault, self.fallback.as_ref(), &self.provider)
                .await
                .map_err(|error| MediaError::Failed(error.to_string()))?
                .ok_or(MediaError::NotConfigured(
                    "this provider, its key is missing",
                ))?;
        let base_url = self
            .config
            .providers
            .get(&self.provider)
            .and_then(|provider| provider.base_url.as_deref());
        Ok(OpenAiMedia::new(key, base_url))
    }
}

fn media_failed(error: impl std::fmt::Display) -> MediaError {
    MediaError::Failed(error.to_string())
}

#[async_trait]
impl Embedder for ProviderMedia {
    fn model(&self) -> String {
        self.config
            .brain
            .embedding_model
            .clone()
            .unwrap_or_default()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MediaError> {
        self.client()
            .await?
            .embed(&self.model(), texts)
            .await
            .map_err(media_failed)
    }
}

#[async_trait]
impl MediaReader for ProviderMedia {
    async fn describe_image(&self, content_type: &str, bytes: &[u8]) -> Result<String, MediaError> {
        let model = self
            .config
            .brain
            .vision_model
            .as_deref()
            .ok_or(MediaError::NotConfigured("images"))?;
        self.client()
            .await?
            .describe_image(model, content_type, bytes)
            .await
            .map_err(media_failed)
    }

    async fn transcribe(
        &self,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, MediaError> {
        let model = self
            .config
            .brain
            .transcription_model
            .as_deref()
            .ok_or(MediaError::NotConfigured("audio"))?;
        self.client()
            .await?
            .transcribe(model, name, content_type, bytes)
            .await
            .map_err(media_failed)
    }
}

/// The person agents work for in a tenant.
struct TenantOwner {
    name: String,
    email: String,
    accounts: BTreeMap<String, String>,
}

/// Where a tenant keeps its files.
struct TenantPaths {
    brain: PathBuf,
    created_agents: PathBuf,
    created_automations: PathBuf,
}

/// Everything the tenants of an instance share.
struct Shared {
    config: Arc<Config>,
    store: Store,
    key: MasterKey,
    zone: Tz,
    accounts: Accounts,
    policy: ToolPolicy,
    /// One set of OAuth flows serves every tenant, so the callback finds each flow.
    oauth: Arc<OAuthFlows>,
    entitlements: Arc<dyn Entitlements>,
}

impl Shared {
    fn vault(&self, tenant: TenantId) -> Vault {
        Vault::new(self.store.clone(), tenant, &self.key)
    }

    /// The instance vault, for provider keys of tenants without their own,
    /// if the configuration allows it.
    fn key_fallback(&self, tenant: TenantId) -> Option<Vault> {
        (self.config.tenancy.allow_instance_keys && tenant != TenantId::DEFAULT)
            .then(|| self.vault(TenantId::DEFAULT))
    }

    fn paths(&self, tenant: TenantId) -> TenantPaths {
        if tenant == TenantId::DEFAULT {
            return TenantPaths {
                brain: self.config.brain.path.clone(),
                created_agents: self.config.paths.created_agents.clone(),
                created_automations: self.config.paths.created_automations.clone(),
            };
        }
        let root = self.tenant_dir(tenant);
        TenantPaths {
            brain: root.join("brain"),
            created_agents: root.join("agents-created"),
            created_automations: root.join("automations-created"),
        }
    }

    fn tenant_dir(&self, tenant: TenantId) -> PathBuf {
        self.config
            .tenancy
            .data_dir
            .join(tenant.as_uuid().to_string())
    }

    async fn owner(&self, tenant: TenantId) -> anyhow::Result<TenantOwner> {
        if tenant == TenantId::DEFAULT {
            let owner = &self.config.owner;
            return Ok(TenantOwner {
                name: owner.name.clone(),
                email: owner.email.clone(),
                accounts: owner.accounts.clone(),
            });
        }
        let user = self.accounts.primary_user(tenant).await?;
        Ok(user.map_or_else(
            || TenantOwner {
                name: String::new(),
                email: String::new(),
                accounts: BTreeMap::new(),
            },
            |user| TenantOwner {
                name: user.name,
                email: user.email,
                accounts: BTreeMap::new(),
            },
        ))
    }
}

fn owner_context(owner: &TenantOwner) -> String {
    let mut context = format!("You work for {} ({}).", owner.name, owner.email);
    for (service, account) in &owner.accounts {
        let _ = write!(context, " Their {service} account is {account}.");
    }
    context
}

/// Resolves the configured sources for an owner and checks that they only read.
fn resolve_sources(
    config: &Config,
    policy: &ToolPolicy,
    owner: &TenantOwner,
) -> anyhow::Result<Vec<lighthouse_sync::Source>> {
    let mut sources = Vec::new();
    for (name, source) in &config.sources {
        let mut accounts: Vec<String> = owner
            .accounts
            .get(&source.connector)
            .cloned()
            .into_iter()
            .collect();
        if !owner.email.is_empty() {
            accounts.push(owner.email.clone());
        }
        let resolved = lighthouse_sync::resolve(name, source, accounts)?;
        for (collection, settings) in &resolved.collections {
            let tool = resolved.tool(&settings.tool);
            if policy.level(&tool) != ToolLevel::Read {
                anyhow::bail!(
                    "source `{name}`: collection `{collection}` calls `{tool}`, which is not in the \
                     connector's read_tools. Refreshes only read."
                );
            }
        }
        sources.push(resolved);
    }
    Ok(sources)
}

fn open_brain(
    shared: &Shared,
    tenant: TenantId,
    path: PathBuf,
    vault: &Vault,
) -> anyhow::Result<Brain> {
    let config = &shared.config;
    let mut brain = Brain::new(shared.store.clone(), tenant, path)?;
    if let Some(provider) = &config.brain.provider {
        let media = Arc::new(ProviderMedia {
            config: config.clone(),
            vault: vault.clone(),
            fallback: shared.key_fallback(tenant),
            provider: provider.clone(),
        });
        brain = brain.with_media(media.clone());
        if config.brain.embedding_model.is_some() {
            brain = brain.with_embedder(media);
        }
    }
    Ok(brain)
}

/// One open tenant: the state the API serves and its background work.
pub struct Workspace {
    tenant: TenantId,
    state: ApiState,
    pool: Arc<AgentPool>,
    scheduler: std::sync::Mutex<Option<Scheduler>>,
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Workspace {
    /// The tenant.
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// The agents of the tenant.
    pub fn pool(&self) -> &Arc<AgentPool> {
        &self.pool
    }

    /// Starts the scheduler and the maintenance of this tenant, once.
    fn start(&self) {
        let Some(scheduler) = self.scheduler.lock().expect("scheduler lock").take() else {
            return;
        };
        let mut tasks = self.tasks.lock().expect("tasks lock");
        tasks.push(tokio::spawn(scheduler.run()));
        tasks.push(tokio::spawn(self.pool.clone().run_maintenance()));
    }

    fn stop(&self) {
        for task in self.tasks.lock().expect("tasks lock").drain(..) {
            task.abort();
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Opens tenants on first use and runs their background work.
///
/// Each tenant has its own vault, Brain, agents, scheduler, sync and
/// automations, so nothing of one tenant reaches another. Background work of
/// each tenant runs in its own tasks: one failing tenant does not stop others.
pub struct Workspaces {
    shared: Arc<Shared>,
    /// One cell per tenant, so opening one tenant does not hold up requests
    /// to the others. A cell stays empty while opening fails.
    open: std::sync::Mutex<HashMap<TenantId, Arc<tokio::sync::OnceCell<Arc<Workspace>>>>>,
    background: AtomicBool,
}

impl Workspaces {
    /// Returns an open tenant, or opens it. `None` if the tenant does not exist.
    pub async fn open(&self, tenant: TenantId) -> anyhow::Result<Option<Arc<Workspace>>> {
        let cell = self
            .open
            .lock()
            .expect("workspaces lock")
            .entry(tenant)
            .or_default()
            .clone();
        if let Some(workspace) = cell.get() {
            return Ok(Some(workspace.clone()));
        }
        if self.shared.accounts.tenant(tenant).await?.is_none() {
            self.open.lock().expect("workspaces lock").remove(&tenant);
            return Ok(None);
        }
        let workspace = cell
            .get_or_try_init(|| async {
                let workspace = Arc::new(open_tenant(&self.shared, tenant).await?);
                if self.background.load(Ordering::SeqCst) {
                    workspace.start();
                }
                tracing::info!(tenant = %tenant.as_uuid(), "tenant open");
                Ok::<_, anyhow::Error>(workspace)
            })
            .await?;
        Ok(Some(workspace.clone()))
    }

    fn opened_workspaces(&self) -> Vec<Arc<Workspace>> {
        self.open
            .lock()
            .expect("workspaces lock")
            .values()
            .filter_map(|cell| cell.get().cloned())
            .collect()
    }

    /// Opens every tenant of the instance. A tenant that fails to open is
    /// logged and tried again on the next round.
    pub async fn open_all(&self) -> anyhow::Result<()> {
        for tenant in self.shared.accounts.all_tenants().await? {
            if let Err(error) = self.open(tenant).await {
                tracing::error!(tenant = %tenant.as_uuid(), %error, "cannot open the tenant");
            }
        }
        Ok(())
    }

    /// Runs the background work of every tenant until the process ends, and
    /// picks up new tenants once a minute.
    pub async fn run(self: Arc<Self>) {
        self.background.store(true, Ordering::SeqCst);
        for workspace in self.opened_workspaces() {
            workspace.start();
        }
        let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = self.open_all().await {
                tracing::error!(%error, "cannot list the tenants");
            }
        }
    }

    /// The tenants that are open now.
    pub async fn opened(&self) -> Vec<TenantId> {
        let mut tenants: Vec<TenantId> = self
            .opened_workspaces()
            .iter()
            .map(|workspace| workspace.tenant)
            .collect();
        tenants.sort();
        tenants
    }
}

#[async_trait]
impl lighthouse_api::Tenants for Workspaces {
    async fn state(&self, tenant: TenantId) -> Result<Option<ApiState>, String> {
        self.open(tenant)
            .await
            .map(|workspace| workspace.map(|workspace| workspace.state.clone()))
            .map_err(|error| error.to_string())
    }

    async fn forget(&self, tenant: TenantId) {
        let cell = self.open.lock().expect("workspaces lock").remove(&tenant);
        if let Some(workspace) = cell.as_ref().and_then(|cell| cell.get()) {
            workspace.stop();
        }
        if tenant != TenantId::DEFAULT {
            let directory = self.shared.tenant_dir(tenant);
            match tokio::fs::remove_dir_all(&directory).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(%error, "cannot remove the files of a deleted tenant"),
            }
        }
        tracing::info!(tenant = %tenant.as_uuid(), "tenant deleted");
    }
}

/// Wires everything one tenant needs.
#[allow(clippy::too_many_lines)]
async fn open_tenant(shared: &Arc<Shared>, tenant: TenantId) -> anyhow::Result<Workspace> {
    let config = &shared.config;
    let store = shared.store.clone();
    let zone = shared.zone;
    let owner = shared.owner(tenant).await?;
    let paths = shared.paths(tenant);
    if tenant != TenantId::DEFAULT {
        for directory in [
            &paths.brain,
            &paths.created_agents,
            &paths.created_automations,
        ] {
            tokio::fs::create_dir_all(directory)
                .await
                .with_context(|| format!("cannot create {}", directory.display()))?;
        }
    }
    let vault = shared.vault(tenant);
    let audit = AuditLog::new(store.clone(), tenant);
    let interrupted = audit.fail_interrupted_runs().await?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "marked interrupted runs as failed");
    }
    let desk = Desk::new(store.clone(), tenant);
    let events = EventBus::new();
    let delivery = lighthouse_config::DeliveryConfig {
        smtp: config.delivery.smtp.clone(),
        slack_channels: config.delivery.slack_channels.clone(),
    };
    let brain = open_brain(shared, tenant, paths.brain, &vault)?;
    let report = brain.sync().await.context("cannot read the Brain")?;
    tracing::info!(?report, "Brain ready");
    let embedding = brain.clone();
    tokio::spawn(async move {
        if let Err(error) = embedding.embed_missing().await {
            tracing::warn!(%error, "embedding failed, search stays lexical for now");
        }
    });
    let mcp: Arc<dyn ToolHostFactory> =
        Arc::new(McpConnector::new(config.connectors.clone(), vault.clone()));
    let sync_tools = mcp.clone();
    let runtime = Arc::new(Runtime {
        router: Arc::new(
            VaultRouterSource::new(config.clone(), vault.clone())
                .with_fallback(shared.key_fallback(tenant)),
        ),
        tools: Arc::new(BrainToolFactory::new(
            brain.clone(),
            Arc::new(DeskToolFactory::new(desk.clone(), mcp)),
        )),
        policy: ToolPolicy::new(&config.connectors),
        skills: SkillLibrary::load(&config.paths.skills)?,
        audit: audit.clone(),
        notifier: Arc::new(ChannelNotifier::with_vaults(
            delivery,
            vault.clone(),
            shared.vault(TenantId::DEFAULT),
        )?),
        owner_context: owner_context(&owner),
        owner_email: owner.email.clone(),
        zone,
        desk: desk.clone(),
    });
    let agents = Arc::new(AgentRegistry::load(
        config.paths.agents.clone(),
        paths.created_agents,
    )?);
    let definitions: Vec<AgentDefinition> = agents
        .list()
        .iter()
        .map(|known| (*known.agent).clone())
        .collect();
    tracing::info!(count = definitions.len(), "agents loaded");
    let notifier = runtime.notifier.clone();
    let outcomes = Arc::new(Outcomes::new(OutcomeParts {
        audit: audit.clone(),
        desk: desk.clone(),
        sessions: Sessions::new(store.clone(), tenant),
        events: events.clone(),
        notifier,
        pushes: Pushes::from_config(&config.notifications)?,
        agents: agents.clone(),
        zone,
        weather: config.weather.clone(),
    }));
    let automation_files = Arc::new(AutomationRegistry::load(
        config.paths.automations.clone(),
        paths.created_automations,
    )?);
    tracing::info!(count = automation_files.list().len(), "automations loaded");
    let engine = Arc::new(AutomationEngine::new(
        runtime.clone(),
        agents.clone(),
        automation_files,
        outcomes.clone(),
        desk.clone(),
    ));
    let sources = resolve_sources(config, &shared.policy, &owner)?;
    tracing::info!(count = sources.len(), "connected sources loaded");
    let sync_store = lighthouse_sync::SyncStore::new(store.clone(), tenant);
    let syncer = Arc::new(lighthouse_sync::Syncer::new(
        sync_store.clone(),
        sync_tools,
        sources,
    ));
    let connected = Arc::new(ConnectedService::new(
        syncer,
        runtime.clone(),
        engine.clone(),
        outcomes.clone(),
        events.clone(),
    ));
    let publisher = Arc::new(publishing::ServerPublisher::new(
        runtime.clone(),
        outcomes.clone(),
        config.publishing.clone(),
    ));
    let pool = Arc::new(AgentPool {
        tenant,
        entitlements: shared.entitlements.clone(),
        desk: desk.clone(),
        runtime,
        agents: agents.clone(),
        outcomes,
        engine: engine.clone(),
        connected: connected.clone(),
    });
    let scheduler = Scheduler::new(
        store.clone(),
        tenant,
        &config.server.timezone,
        &definitions,
        pool.clone(),
    )?;
    let automations = Arc::new(ServerAutomations::new(engine, scheduler.handle(), zone)?);
    let catalog = Arc::new(ServerCatalog {
        config: config.clone(),
        vault: vault.clone(),
        runtime: pool.runtime.clone(),
        agents,
        schedules: scheduler.handle(),
        zone,
    });
    let assistant = Arc::new(BrainAssistant {
        brain: brain.clone(),
        runtime: pool.runtime.clone(),
    });
    let state = ApiState {
        audit,
        trigger: pool.clone(),
        oauth: shared.oauth.clone(),
        vault,
        brain,
        assistant,
        catalog,
        sessions: Sessions::new(store, tenant),
        events,
        desk,
        zone,
        automations,
        sync: sync_store,
        connected,
        publisher,
        talk_agent: config.talk.agent.clone(),
        writer: Arc::new(publishing::ServerWriter::new(pool.runtime.clone())),
    };
    Ok(Workspace {
        tenant,
        state,
        pool,
        scheduler: std::sync::Mutex::new(Some(scheduler)),
        tasks: std::sync::Mutex::default(),
    })
}

/// A fully wired server.
pub struct App {
    /// The HTTP router.
    pub router: lighthouse_api::Router,
    /// The tenants, for background work.
    pub workspaces: Arc<Workspaces>,
}

fn signup_mode(mode: lighthouse_config::SignupMode) -> SignupMode {
    match mode {
        lighthouse_config::SignupMode::Closed => SignupMode::Closed,
        lighthouse_config::SignupMode::Invite => SignupMode::Invite,
        lighthouse_config::SignupMode::Open => SignupMode::Open,
    }
}

/// What a program built on this crate adds to the server.
pub struct Hooks {
    /// Hooks around sign-ups and tenants.
    pub lifecycle: Arc<dyn Lifecycle>,
    /// Limits of tenants, consulted before tenants are created, members
    /// invited and agents run.
    pub entitlements: Arc<dyn Entitlements>,
    /// Extra routes, public and behind the sign-in.
    pub routes: Extensions,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            lifecycle: Arc::new(NoHooks),
            entitlements: Arc::new(AllowAll),
            routes: Extensions::default(),
        }
    }
}

/// Connects everything described by the configuration.
pub async fn build(config: Config) -> anyhow::Result<App> {
    build_with(config, Hooks::default()).await
}

/// Connects everything described by the configuration, with the hooks and
/// routes of another program.
pub async fn build_with(config: Config, hooks: Hooks) -> anyhow::Result<App> {
    let config = Arc::new(config);
    let store = Store::connect(config.database.url.expose_secret())
        .await
        .context("cannot connect to Postgres")?;
    store
        .migrate()
        .await
        .context("cannot migrate the database")?;
    let zone: Tz = config
        .server
        .timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown time zone"))?;
    let accounts = Accounts::new(store.clone());
    accounts
        .bootstrap_owner(&config.owner.email, &config.owner.name)
        .await
        .context("cannot create the first user from the configured owner")?;
    let policy = ToolPolicy::new(&config.connectors);
    let shared = Arc::new(Shared {
        key: master_key(&config)?,
        config: config.clone(),
        store,
        zone,
        accounts: accounts.clone(),
        policy,
        oauth: Arc::new(OAuthFlows::new(
            config.connectors.clone(),
            &config.server.public_url,
        )),
        entitlements: hooks.entitlements.clone(),
    });
    let configured_owner = shared.owner(TenantId::DEFAULT).await?;
    resolve_sources(&config, &shared.policy, &configured_owner)?;
    let workspaces = Arc::new(Workspaces {
        shared: shared.clone(),
        open: std::sync::Mutex::default(),
        background: AtomicBool::new(false),
    });
    workspaces
        .open(TenantId::DEFAULT)
        .await
        .context("cannot open the default tenant")?;
    let mailer: Option<Arc<dyn lighthouse_notify::Notifier>> = match &config.delivery.smtp {
        Some(smtp) => Some(Arc::new(ChannelNotifier::from_config(
            lighthouse_config::DeliveryConfig {
                smtp: Some(smtp.clone()),
                slack_channels: Vec::new(),
            },
            shared.vault(TenantId::DEFAULT),
        )?)),
        None => None,
    };
    let router = lighthouse_api::router_with(
        Instance {
            accounts,
            tenants: workspaces.clone(),
            oauth: shared.oauth.clone(),
            admin_token: config.server.admin_token.clone(),
            signup: signup_mode(config.tenancy.signup),
            mailer,
            public_url: config.server.public_url.trim_end_matches('/').to_owned(),
            trust_forwarded_for: config.server.trust_forwarded_for,
            lifecycle: hooks.lifecycle,
            entitlements: hooks.entitlements,
        },
        hooks.routes,
    );
    let router = match &config.paths.web {
        Some(directory) => lighthouse_api::with_web_app(router, directory),
        None => router,
    };
    Ok(App { router, workspaces })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_routes_come_from_routing_and_providers() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../demo/lighthouse.demo.toml");
        let mut config = Config::load(&path).unwrap();
        let route = model_route(&config, "text").unwrap();
        assert_eq!(
            (route.provider.as_str(), route.model.as_deref()),
            ("demo", Some("demo"))
        );
        assert_eq!((route.fallback, route.fallback_model), (None, None));
        config.routing.get_mut("text").unwrap().fallback = Some("demo".to_owned());
        let route = model_route(&config, "text").unwrap();
        assert_eq!(route.fallback_model.as_deref(), Some("demo"));
        assert!(model_route(&config, "vision").is_none());
    }
}
