//! Wiring of the Lighthouse server.

use anyhow::Context;
use async_trait::async_trait;
use lighthouse_api::{ApiState, AskAnswer, Assistant, RunTrigger, TriggerError};
use lighthouse_audit::{AuditError, AuditLog, RunEnd, RunStatus};
use lighthouse_auth::Auth;
use lighthouse_brain::{Brain, BrainToolFactory, Embedder, MediaError, MediaReader};
use lighthouse_config::{Config, OwnerConfig};
use lighthouse_core::agent::AgentDefinition;
use lighthouse_core::{RunId, TenantId};
use lighthouse_mcp::{McpConnector, OAuthFlows, ToolHostFactory, ToolPolicy};
use lighthouse_notify::ChannelNotifier;
use lighthouse_router::media::OpenAiMedia;
use lighthouse_runtime::{Runtime, VaultRouterSource, load_agents};
use lighthouse_scheduler::{AgentRunner, Scheduler};
use lighthouse_skills::SkillLibrary;
use lighthouse_store::Store;
use lighthouse_vault::{MasterKey, Vault};
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
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
    agents: HashMap<String, Arc<AgentDefinition>>,
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
        let Some(agent) = self.agents.get(&checkpoint.agent_id).cloned() else {
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
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| TriggerError::UnknownAgent(agent_id.to_owned()))?;
        let run = self
            .spawn(agent.clone(), "manual")
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
        if let Some(agent) = self.agents.get(agent_id)
            && let Err(error) = self.spawn(agent.clone(), trigger).await
        {
            tracing::error!(agent = %agent_id, %error, "cannot start run");
        }
    }
}

/// Number of entries a question is answered from.
const ASK_ENTRIES: usize = 6;

/// Answers questions from the Brain.
struct BrainAssistant {
    brain: Brain,
    runtime: Arc<Runtime>,
}

#[async_trait]
impl Assistant for BrainAssistant {
    async fn ask(&self, question: &str) -> Result<AskAnswer, String> {
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
        let (run, answer) = self.runtime.ask(question, &context).await?;
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
    let auth_store = store.clone();
    let delivery = lighthouse_config::DeliveryConfig {
        smtp: config.delivery.smtp.clone(),
        slack_channels: config.delivery.slack_channels.clone(),
    };
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
        zone: config
            .server
            .timezone
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown time zone"))?,
    });
    let mut agents = Vec::new();
    for file in load_agents(&config.paths.agents)? {
        match file.agent {
            Ok(agent) => agents.push(agent),
            Err(error) => {
                tracing::error!(path = %file.path.display(), %error, "skipping invalid agent")
            }
        }
    }
    tracing::info!(count = agents.len(), "agents loaded");
    let pool = Arc::new(AgentPool {
        runtime,
        agents: agents
            .iter()
            .map(|agent| (agent.id.clone(), Arc::new(agent.clone())))
            .collect(),
    });
    let scheduler = Scheduler::new(
        store,
        tenant,
        &config.server.timezone,
        &agents,
        pool.clone(),
    )?;
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
        admin_token: config.server.admin_token.clone(),
    });
    Ok(App {
        router,
        scheduler,
        pool,
    })
}
