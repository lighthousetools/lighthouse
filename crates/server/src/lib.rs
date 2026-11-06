//! Wiring of the Lighthouse server.

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use lighthouse_api::{
    AgentChangeError, AgentInfo, AgentTool, ApiState, AskAnswer, Assistant, Catalog, ConnectorInfo,
    RunTrigger, SkillInfo, TriggerError,
};
use lighthouse_audit::{AuditError, AuditLog, RunEnd, RunStatus};
use lighthouse_auth::Auth;
use lighthouse_brain::{Brain, BrainToolFactory, Embedder, MediaError, MediaReader, SearchHit};
use lighthouse_config::{Config, ConnectorAuth, ConnectorTransport, OwnerConfig};
use lighthouse_core::agent::{AgentDefinition, Delivery, ToolLevel, Trigger, parse_cron};
use lighthouse_core::{RunId, TenantId};
use lighthouse_mcp::{McpConnector, OAuthFlows, ToolHostFactory, ToolPolicy};
use lighthouse_notify::ChannelNotifier;
use lighthouse_router::media::OpenAiMedia;
use lighthouse_runtime::{AgentRegistry, LoadedAgent, RegistryError, Runtime, VaultRouterSource};
use lighthouse_scheduler::{AgentRunner, ScheduleHandle, Scheduler};
use lighthouse_sessions::Sessions;
use lighthouse_skills::SkillLibrary;
use lighthouse_store::Store;
use lighthouse_vault::{MasterKey, Vault};
use secrecy::ExposeSecret;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

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

fn owner_context(owner: &OwnerConfig) -> String {
    let mut context = format!("You work for {} ({}).", owner.name, owner.email);
    for (service, account) in &owner.accounts {
        let _ = write!(context, " Their {service} account is {account}.");
    }
    context
}

/// Starts runs of the loaded agents in the background.
pub struct AgentPool {
    runtime: Arc<Runtime>,
    agents: Arc<AgentRegistry>,
}

impl AgentPool {
    async fn spawn(&self, agent: Arc<AgentDefinition>, trigger: &str) -> Result<RunId, AuditError> {
        let run = RunId::new();
        self.runtime.register(run, &agent, trigger).await?;
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.execute(run, &agent).await {
                tracing::error!(agent = %agent.id, %error, "run failed");
            }
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
            return Ok(());
        };
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.resume(id, &agent, checkpoint.state).await {
                tracing::error!(agent = %agent.id, %error, "resumed run failed");
            }
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
        Ok(())
    }

    /// Runs the maintenance until the process ends.
    pub async fn run_maintenance(self: Arc<Self>) {
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
        let run = self
            .spawn(known.agent, "manual")
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
        if let Some(known) = self.agents.get(agent_id)
            && let Err(error) = self.spawn(known.agent, trigger).await
        {
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
        let schedule = match &agent.trigger {
            Trigger::Cron(expression) => Some(expression.clone()),
            Trigger::Manual => None,
        };
        AgentInfo {
            id: agent.id.clone(),
            name: agent.name.clone(),
            next_run: schedule.as_deref().and_then(|cron| self.next_run(cron)),
            schedule,
            tools: agent
                .tools
                .iter()
                .map(|grant| AgentTool {
                    name: grant.name.clone(),
                    level: level_name(grant.level).to_owned(),
                })
                .collect(),
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
    provider: String,
}

impl ProviderMedia {
    async fn client(&self) -> Result<OpenAiMedia, MediaError> {
        let key = self
            .vault
            .get(&self.provider)
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

fn open_brain(config: &Arc<Config>, store: Store, vault: &Vault) -> anyhow::Result<Brain> {
    let mut brain = Brain::new(store, TenantId::SINGLE, config.brain.path.clone())?;
    if let Some(provider) = &config.brain.provider {
        let media = Arc::new(ProviderMedia {
            config: config.clone(),
            vault: vault.clone(),
            provider: provider.clone(),
        });
        brain = brain.with_media(media.clone());
        if config.brain.embedding_model.is_some() {
            brain = brain.with_embedder(media);
        }
    }
    Ok(brain)
}

/// A fully wired server.
pub struct App {
    /// The HTTP router.
    pub router: lighthouse_api::Router,
    /// The scheduler, ready to run.
    pub scheduler: Scheduler,
    /// The agents, for background maintenance.
    pub pool: Arc<AgentPool>,
}

/// Connects everything described by the configuration.
pub async fn build(config: Config) -> anyhow::Result<App> {
    let config = Arc::new(config);
    let store = Store::connect(config.database.url.expose_secret())
        .await
        .context("cannot connect to Postgres")?;
    store
        .migrate()
        .await
        .context("cannot migrate the database")?;
    let tenant = TenantId::SINGLE;
    let vault = Vault::new(store.clone(), tenant, &master_key(&config)?);
    let audit = AuditLog::new(store.clone(), tenant);
    let interrupted = audit.fail_interrupted_runs().await?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "marked interrupted runs as failed");
    }
    let auth_store = store.clone();
    let session_store = store.clone();
    let delivery = lighthouse_config::DeliveryConfig {
        smtp: config.delivery.smtp.clone(),
        slack_channels: config.delivery.slack_channels.clone(),
    };
    let zone: Tz = config
        .server
        .timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown time zone"))?;
    let brain = open_brain(&config, store.clone(), &vault)?;
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
    let runtime = Arc::new(Runtime {
        router: Arc::new(VaultRouterSource::new(config.clone(), vault.clone())),
        tools: Arc::new(BrainToolFactory::new(brain.clone(), mcp)),
        policy: ToolPolicy::new(&config.connectors),
        skills: SkillLibrary::load(&config.paths.skills)?,
        audit: audit.clone(),
        notifier: Arc::new(ChannelNotifier::from_config(delivery, vault.clone())?),
        owner_context: owner_context(&config.owner),
        owner_email: config.owner.email.clone(),
        zone,
    });
    let agents = Arc::new(AgentRegistry::load(
        config.paths.agents.clone(),
        config.paths.created_agents.clone(),
    )?);
    let definitions: Vec<AgentDefinition> = agents
        .list()
        .iter()
        .map(|known| (*known.agent).clone())
        .collect();
    tracing::info!(count = definitions.len(), "agents loaded");
    let pool = Arc::new(AgentPool {
        runtime,
        agents: agents.clone(),
    });
    let scheduler = Scheduler::new(
        store,
        tenant,
        &config.server.timezone,
        &definitions,
        pool.clone(),
    )?;
    let catalog = Arc::new(ServerCatalog {
        config: config.clone(),
        vault: vault.clone(),
        runtime: pool.runtime.clone(),
        agents,
        schedules: scheduler.handle(),
        zone,
    });
    let oauth = OAuthFlows::new(
        config.connectors.clone(),
        vault.clone(),
        &config.server.public_url,
    );
    let assistant = Arc::new(BrainAssistant {
        brain: brain.clone(),
        runtime: pool.runtime.clone(),
    });
    let router = lighthouse_api::router(ApiState {
        audit,
        trigger: pool.clone(),
        oauth: Arc::new(oauth),
        auth: Auth::new(auth_store, tenant),
        vault,
        brain: brain.clone(),
        assistant,
        catalog,
        owner_name: config.owner.name.clone(),
        sessions: Sessions::new(session_store, tenant),
        admin_token: config.server.admin_token.clone(),
    });
    let router = match &config.paths.web {
        Some(directory) => lighthouse_api::with_web_app(router, directory),
        None => router,
    };
    Ok(App {
        router,
        scheduler,
        pool,
    })
}
