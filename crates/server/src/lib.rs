//! Wiring of the Lighthouse server.

use anyhow::Context;
use async_trait::async_trait;
use lighthouse_api::{ApiState, RunTrigger, TriggerError};
use lighthouse_audit::{AuditError, AuditLog, RunEnd, RunStatus};
use lighthouse_auth::Auth;
use lighthouse_config::{Config, OwnerConfig};
use lighthouse_core::agent::AgentDefinition;
use lighthouse_core::{RunId, TenantId};
use lighthouse_mcp::{McpConnector, OAuthFlows, ToolPolicy};
use lighthouse_notify::ChannelNotifier;
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
    let runtime = Arc::new(Runtime {
        router: Arc::new(VaultRouterSource::new(config.clone(), vault.clone())),
        tools: Arc::new(McpConnector::new(config.connectors.clone(), vault.clone())),
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
    let router = lighthouse_api::router(ApiState {
        audit,
        trigger: pool.clone(),
        oauth: Arc::new(oauth),
        auth: Auth::new(auth_store, tenant),
        vault,
        admin_token: config.server.admin_token.clone(),
    });
    Ok(App {
        router,
        scheduler,
        pool,
    })
}
