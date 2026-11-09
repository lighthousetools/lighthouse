//! Wiring of the Lighthouse server.

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use lighthouse_api::hooks::{
    Action, AllowAll, EnglishMails, Entitlements, Lifecycle, MailTemplates, NoHooks,
};
use lighthouse_api::{
    AgentChangeError, AgentInfo, AgentTool, ApiState, AskAnswer, Assistant, Catalog, ConnectorInfo,
    Extensions, Instance, ModelRoute, RunTrigger, SignupMode, SkillInfo, TriggerError,
};
use lighthouse_audit::{AuditError, AuditLog, RunEnd, RunStatus};
use lighthouse_auth::Accounts;
use lighthouse_brain::{Brain, BrainToolFactory, Embedder, MediaError, MediaReader, SearchHit};
use lighthouse_config::{Config, ConnectorAuth, ConnectorTransport};
use lighthouse_core::agent::{AgentDefinition, Delivery, ToolLevel, Trigger, parse_cron, slugify};
use lighthouse_core::{RunId, TenantId};
use lighthouse_desk::Desk;
use lighthouse_mcp::{McpConnector, OAuthFlows, ToolHostFactory, ToolPolicy};
use lighthouse_notify::{ChannelNotifier, EventBus, Mailer, SmtpMailer};
use lighthouse_router::media::OpenAiMedia;
use lighthouse_runtime::{
    AgentRegistry, AutomationRegistry, DeskToolFactory, LiveRouterSource, LoadedAgent,
    RegistryError, Runtime,
};
use lighthouse_scheduler::{AgentRunner, ScheduleHandle, Scheduler};
use lighthouse_sessions::Sessions;
use lighthouse_settings::{Defaults, Effective, FileKind, Layer, TenantSettings};
use lighthouse_skills::SkillLibrary;
use lighthouse_store::Store;
use lighthouse_sync::SourceList;
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

pub mod assistant;
pub mod automations;
pub mod connected;
pub mod outcomes;
pub mod publishing;
pub mod tenant_config;
pub mod weather;

use automations::{AutomationEngine, ServerAutomations};
use connected::ConnectedService;
use lighthouse_core::automation::RUN_PREFIX;
use outcomes::{OutcomeParts, Outcomes, Pushes};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
/// How often a workspace looks for settings changed by another server process.
const SETTINGS_INTERVAL: Duration = Duration::from_secs(15);

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
    settings: TenantSettings,
    vault: Vault,
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
    /// Builds the assistant for the user the task acts for, if they connected anything.
    async fn build_assistant(&self, message: &str) -> Result<Option<AgentDefinition>, String> {
        let effective = self
            .settings
            .effective(lighthouse_audit::acting_user::current());
        let connectors: Vec<String> = connector_infos(&effective, &self.vault)
            .await?
            .into_iter()
            .filter(|connector| connector.connected)
            .map(|connector| connector.name)
            .collect();
        Ok(assistant::assistant(
            self.runtime.tools.as_ref(),
            &self.runtime.policy,
            &connectors,
            message,
        )
        .await)
    }

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
        let user = audit.run_user(run).await?;
        let known = if checkpoint.agent_id == assistant::ASSISTANT_ID {
            // The saved state holds the conversation, so the message is not needed again.
            let built = self
                .runtime
                .as_user(user, self.build_assistant(""))
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            built.map(Arc::new)
        } else {
            self.agents
                .get_for(user, &checkpoint.agent_id)
                .map(|known| known.agent)
        };
        let Some(agent) = known else {
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
        if !known.enabled {
            return Err(TriggerError::Refused(format!(
                "the agent `{agent_id}` is switched off"
            )));
        }
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
        if !known.enabled {
            return Err(TriggerError::Refused(format!(
                "the agent `{agent_id}` is switched off"
            )));
        }
        self.allowed(agent_id).await?;
        let run = self
            .spawn_with(known.agent, "task", Some(context.to_owned()))
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(run.as_uuid())
    }

    async fn trigger_assistant(
        &self,
        message: &str,
        earlier: &str,
    ) -> Result<Option<Uuid>, TriggerError> {
        let Some(agent) = self
            .build_assistant(message)
            .await
            .map_err(TriggerError::Failed)?
        else {
            return Ok(None);
        };
        self.allowed(assistant::ASSISTANT_ID).await?;
        let context = (!earlier.is_empty()).then(|| format!("Conversation so far:\n{earlier}"));
        let run = self
            .spawn_with(Arc::new(agent), "task", context)
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(Some(run.as_uuid()))
    }

    async fn resume(&self, run: Uuid) -> Result<(), TriggerError> {
        self.resume_run(run)
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))
    }
}

#[async_trait]
impl AgentRunner for AgentPool {
    async fn start(&self, job: &str, trigger: &str) {
        // A personal job names its person, and works for them.
        let (id, user) = split_job(job);
        if id.starts_with(RUN_PREFIX) {
            self.engine.start_scheduled(id, user).await;
            return;
        }
        let Some(known) = self.agents.get_for(user, id).filter(|known| known.enabled) else {
            return;
        };
        if let Err(error) = self.allowed(id).await {
            tracing::warn!(agent = %id, %error, "run not allowed");
            return;
        }
        let started = self
            .runtime
            .as_user(user, self.spawn(known.agent, trigger))
            .await;
        if let Err(error) = started {
            tracing::error!(agent = %id, %error, "cannot start run");
        }
    }
}

/// Splits a scheduled job into what it fires and the person it is for.
fn split_job(job: &str) -> (&str, Option<Uuid>) {
    match job.rsplit_once('@').map(|(id, user)| (id, user.parse())) {
        Some((id, Ok(user))) => (id, Some(user)),
        _ => (job, None),
    }
}

/// Keeps the scheduler in line with the agents and automations of a tenant and
/// the time zones of the people they run for.
struct ScheduleSync {
    settings: TenantSettings,
    resolver: Arc<tenant_config::TenantConfig>,
    agents: Arc<AgentRegistry>,
    automations: Arc<AutomationRegistry>,
    schedules: ScheduleHandle,
}

impl ScheduleSync {
    fn cron_of(trigger: &Trigger) -> Option<&String> {
        match trigger {
            Trigger::Cron(expression) => Some(expression),
            Trigger::Manual => None,
        }
    }

    fn automation_cron(
        trigger: &lighthouse_core::automation::AutomationTrigger,
    ) -> Option<&String> {
        match trigger {
            lighthouse_core::automation::AutomationTrigger::Cron(cron) => Some(cron),
            _ => None,
        }
    }

    /// The jobs of the tenant, which work for its first owner, and of each
    /// member's own agents and automations. What the first owner replaced or
    /// switched off for themselves does not run a second time for the tenant.
    fn jobs(&self) -> Vec<lighthouse_scheduler::Job> {
        let snapshot = self.settings.snapshot();
        let primary = snapshot.primary().map(|member| member.id);
        let mut jobs = Vec::new();
        let zone = self.resolver.zone_of(primary);
        let as_primary_agents = self.agents.for_user(primary);
        for known in self.agents.for_user(None).values() {
            let theirs = as_primary_agents.get(&known.agent.id);
            let replaced =
                theirs.is_some_and(|theirs| theirs.layer == lighthouse_settings::Layer::Personal);
            let off = !known.enabled || theirs.is_some_and(|theirs| !theirs.enabled);
            if let (Some(cron), false, false) = (Self::cron_of(&known.agent.trigger), replaced, off)
            {
                jobs.push(lighthouse_scheduler::Job {
                    id: known.agent.id.clone(),
                    cron: cron.clone(),
                    zone,
                });
            }
        }
        let as_primary_automations = self.automations.for_user(primary);
        for known in self.automations.for_user(None).values() {
            let theirs = as_primary_automations.get(&known.automation.id);
            let replaced =
                theirs.is_some_and(|theirs| theirs.layer == lighthouse_settings::Layer::Personal);
            let off = !known.enabled || theirs.is_some_and(|theirs| !theirs.enabled);
            if let (Some(cron), false, false) = (
                Self::automation_cron(&known.automation.trigger),
                replaced,
                off,
            ) {
                jobs.push(lighthouse_scheduler::Job {
                    id: format!("{RUN_PREFIX}{}", known.automation.id),
                    cron: cron.clone(),
                    zone,
                });
            }
        }
        for user in snapshot.members.iter().map(|member| member.id) {
            if !snapshot.personal.contains_key(&user) {
                continue;
            }
            let zone = self.resolver.zone_of(Some(user));
            for known in self.agents.for_user(Some(user)).values() {
                if known.layer != lighthouse_settings::Layer::Personal || !known.enabled {
                    continue;
                }
                if let Some(cron) = Self::cron_of(&known.agent.trigger) {
                    jobs.push(lighthouse_scheduler::Job {
                        id: format!("{}@{user}", known.agent.id),
                        cron: cron.clone(),
                        zone,
                    });
                }
            }
            for known in self.automations.for_user(Some(user)).values() {
                if known.layer != lighthouse_settings::Layer::Personal || !known.enabled {
                    continue;
                }
                if let Some(cron) = Self::automation_cron(&known.automation.trigger) {
                    jobs.push(lighthouse_scheduler::Job {
                        id: format!("{RUN_PREFIX}{}@{user}", known.automation.id),
                        cron: cron.clone(),
                        zone,
                    });
                }
            }
        }
        jobs
    }

    fn refresh(&self) {
        self.schedules.replace(self.jobs());
    }
}

/// Answers what the apps ask about agents and connections.
struct ServerCatalog {
    settings: TenantSettings,
    vault: Vault,
    runtime: Arc<Runtime>,
    agents: Arc<AgentRegistry>,
    schedule: Arc<ScheduleSync>,
    resolver: Arc<tenant_config::TenantConfig>,
}

/// The provider and model of a task, from the effective routing and providers.
fn model_route(effective: &Effective, task: &str) -> Option<ModelRoute> {
    let route = &effective.routing.get(task)?.route;
    let model = |provider: &str| {
        effective
            .model(provider, task)
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

/// The enabled connectors for the user the task acts for, and whether they have
/// what they need to sign in.
async fn connector_infos(
    effective: &Effective,
    vault: &Vault,
) -> Result<Vec<ConnectorInfo>, String> {
    let mut stored = vault.list().await.map_err(|error| error.to_string())?;
    if let Some(user) = lighthouse_audit::acting_user::current() {
        stored.extend(
            vault
                .personal(user)
                .list()
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(effective
        .enabled_connectors()
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

impl ServerCatalog {
    /// What applies to the user the request acts for.
    fn effective(&self) -> Effective {
        self.settings
            .effective(lighthouse_audit::acting_user::current())
    }

    fn next_run(&self, expression: &str) -> Option<DateTime<Utc>> {
        let zone = self
            .resolver
            .zone_of(lighthouse_audit::acting_user::current());
        let now = Utc::now().with_timezone(&zone);
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
            model: model_route(&self.effective(), "text"),
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
            layer: known.layer.as_str().to_owned(),
            enabled: known.enabled,
            disabled_by: known.disabled_by.map(|layer| layer.as_str().to_owned()),
        }
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
        for (name, connector) in &self.effective().enabled_connectors() {
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

pub(crate) fn settings_failure(error: lighthouse_settings::SettingsError) -> AgentChangeError {
    use lighthouse_settings::SettingsError;
    match error {
        SettingsError::Database(_) => AgentChangeError::Failed(error.to_string()),
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

    async fn save_agent(
        &self,
        source: &str,
        owner: Option<Uuid>,
    ) -> Result<AgentInfo, AgentChangeError> {
        let agent = AgentDefinition::parse(source)
            .map_err(|error| AgentChangeError::Rejected(error.to_string()))?;
        if slugify(&agent.id) != agent.id || agent.id.is_empty() {
            return Err(AgentChangeError::Rejected(
                RegistryError::BadId(agent.id).to_string(),
            ));
        }
        if owner.is_some()
            && agent
                .deliver
                .iter()
                .any(|target| matches!(target, Delivery::Mail(_)))
        {
            return Err(AgentChangeError::Rejected(
                "your own agents deliver to `owner` or a Slack alias, not to other addresses"
                    .to_owned(),
            ));
        }
        self.settings
            .set_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Agent,
                &agent.id,
                source,
            )
            .await
            .map_err(settings_failure)?;
        self.schedule.refresh();
        self.agents
            .get_for(owner, &agent.id)
            .map(|known| self.info(&known))
            .ok_or_else(|| AgentChangeError::Failed("the agent did not load".to_owned()))
    }

    async fn delete_agent(&self, id: &str, owner: Option<Uuid>) -> Result<bool, AgentChangeError> {
        let Some(known) = self.agents.get_for(owner, id) else {
            return Ok(false);
        };
        let wanted = if owner.is_some() {
            Layer::Personal
        } else {
            Layer::Tenant
        };
        if known.layer != wanted {
            return Err(AgentChangeError::Rejected(
                RegistryError::ReadOnly(id.to_owned()).to_string(),
            ));
        }
        self.settings
            .delete_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Agent,
                id,
            )
            .await
            .map_err(settings_failure)?;
        self.schedule.refresh();
        Ok(true)
    }

    async fn switch(
        &self,
        kind: &str,
        id: &str,
        owner: Option<Uuid>,
        enabled: bool,
    ) -> Result<bool, AgentChangeError> {
        let actor = lighthouse_audit::acting_user::current();
        let (kind, exists) = match kind {
            "agent" => (FileKind::Agent, self.agents.get_for(owner, id).is_some()),
            "automation" => (
                FileKind::Automation,
                self.schedule.automations.get_for(owner, id).is_some(),
            ),
            _ => (FileKind::Skill, self.runtime.skills.exists(id)),
        };
        if !exists {
            return Ok(false);
        }
        self.settings
            .switch_file(owner, actor, kind, id, enabled)
            .await
            .map_err(settings_failure)?;
        self.schedule.refresh();
        Ok(true)
    }

    async fn save_skill(
        &self,
        name: &str,
        source: &str,
        owner: Option<Uuid>,
    ) -> Result<(), AgentChangeError> {
        let skill = lighthouse_skills::parse(source, name)
            .map_err(|error| AgentChangeError::Rejected(error.to_string()))?;
        if skill.name != name {
            return Err(AgentChangeError::Rejected(format!(
                "the file names the skill `{}`, not `{name}`",
                skill.name
            )));
        }
        self.settings
            .set_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Skill,
                name,
                source,
            )
            .await
            .map_err(settings_failure)
    }

    async fn delete_skill(
        &self,
        name: &str,
        owner: Option<Uuid>,
    ) -> Result<bool, AgentChangeError> {
        match self
            .settings
            .delete_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Skill,
                name,
            )
            .await
        {
            Ok(()) => Ok(true),
            Err(lighthouse_settings::SettingsError::NotFound(_)) => Ok(false),
            Err(error) => Err(settings_failure(error)),
        }
    }

    async fn connectors(&self) -> Result<Vec<ConnectorInfo>, String> {
        connector_infos(&self.effective(), &self.vault).await
    }

    fn models(&self) -> Vec<ModelRoute> {
        let effective = self.effective();
        effective
            .routing
            .keys()
            .filter_map(|task| model_route(&effective, task))
            .collect()
    }

    fn skills(&self) -> Vec<SkillInfo> {
        self.runtime
            .skills
            .all()
            .into_iter()
            .map(|known| SkillInfo {
                name: known.skill.name,
                description: known.skill.description,
                layer: known.layer.as_str().to_owned(),
                enabled: known.enabled,
                disabled_by: known.disabled_by.map(|layer| layer.as_str().to_owned()),
                source: known.source,
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

/// Reaches the provider of the Brain's models as they apply now, with its key
/// from the vaults on every call. Embeddings belong to the shared Brain, so they
/// follow the tenant; images and audio follow the person who adds them.
struct ProviderMedia {
    settings: TenantSettings,
    resolver: Arc<tenant_config::TenantConfig>,
}

#[derive(serde::Deserialize, Default)]
struct BrainModels {
    provider: Option<String>,
    embedding_model: Option<String>,
    vision_model: Option<String>,
    transcription_model: Option<String>,
}

impl ProviderMedia {
    /// The models for a user: the tenant's, with what they set for themselves.
    fn models(&self, user: Option<Uuid>) -> BrainModels {
        let parse = |value: &serde_json::Value| serde_json::from_value(value.clone()).ok();
        let mut models: BrainModels = self
            .settings
            .effective(None)
            .values
            .get(lighthouse_settings::BRAIN)
            .and_then(|(value, _)| parse(value))
            .unwrap_or_default();
        let own: Option<BrainModels> = user.and_then(|user| {
            parse(
                self.settings
                    .layer(Some(user))
                    .values
                    .get(lighthouse_settings::BRAIN)?,
            )
        });
        if let Some(own) = own {
            models.provider = own.provider.or(models.provider);
            models.vision_model = own.vision_model.or(models.vision_model);
            models.transcription_model = own.transcription_model.or(models.transcription_model);
        }
        models
    }

    async fn client(&self, user: Option<Uuid>) -> Result<OpenAiMedia, MediaError> {
        let effective = self.settings.effective(user);
        let not_configured = MediaError::NotConfigured("this provider, its key is missing");
        let name = self
            .models(user)
            .provider
            .ok_or(MediaError::NotConfigured("a provider"))?;
        let provider = effective
            .providers
            .get(&name)
            .ok_or(MediaError::NotConfigured("this provider"))?;
        let entry = provider.key_name(&name);
        for vault in self.resolver.vaults(provider.key_layers(), user) {
            if let Some(key) = vault.get(&entry).await.map_err(media_failed)? {
                let client = OpenAiMedia::new(key, provider.base_url.as_deref());
                return Ok(if provider.guarded() {
                    client.with_http(lighthouse_router::net::guarded_client())
                } else {
                    client
                });
            }
        }
        Err(not_configured)
    }
}

fn media_failed(error: impl std::fmt::Display) -> MediaError {
    MediaError::Failed(error.to_string())
}

#[async_trait]
impl Embedder for ProviderMedia {
    fn model(&self) -> String {
        self.models(None).embedding_model.unwrap_or_default()
    }

    fn active(&self) -> bool {
        let models = self.models(None);
        models.provider.is_some() && models.embedding_model.is_some()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MediaError> {
        self.client(None)
            .await?
            .embed(&self.model(), texts)
            .await
            .map_err(media_failed)
    }
}

#[async_trait]
impl MediaReader for ProviderMedia {
    async fn describe_image(&self, content_type: &str, bytes: &[u8]) -> Result<String, MediaError> {
        let user = lighthouse_audit::acting_user::current();
        let model = self
            .models(user)
            .vision_model
            .ok_or(MediaError::NotConfigured("images"))?;
        self.client(user)
            .await?
            .describe_image(&model, content_type, bytes)
            .await
            .map_err(media_failed)
    }

    async fn transcribe(
        &self,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, MediaError> {
        let user = lighthouse_audit::acting_user::current();
        let model = self
            .models(user)
            .transcription_model
            .ok_or(MediaError::NotConfigured("audio"))?;
        self.client(user)
            .await?
            .transcribe(&model, name, content_type, bytes)
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
    /// Sends every mail of the instance, if mail is configured.
    mailer: Option<Arc<dyn Mailer>>,
    /// The instance layer of the settings.
    defaults: Arc<Defaults>,
    /// What tenants and people may set.
    options: lighthouse_settings::Options,
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
    settings: &TenantSettings,
    resolver: &Arc<tenant_config::TenantConfig>,
) -> anyhow::Result<Brain> {
    let media = Arc::new(ProviderMedia {
        settings: settings.clone(),
        resolver: resolver.clone(),
    });
    Ok(Brain::new(shared.store.clone(), tenant, path)?
        .with_media(media.clone())
        .with_embedder(media))
}

/// One open tenant: the state the API serves and its background work.
pub struct Workspace {
    tenant: TenantId,
    state: ApiState,
    pool: Arc<AgentPool>,
    settings: TenantSettings,
    schedule: Arc<ScheduleSync>,
    brain: Brain,
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

    /// The settings of the tenant.
    pub fn settings(&self) -> &TenantSettings {
        &self.settings
    }

    /// Starts the scheduler and the maintenance of this tenant, once.
    fn start(&self) {
        let Some(scheduler) = self.scheduler.lock().expect("scheduler lock").take() else {
            return;
        };
        let mut tasks = self.tasks.lock().expect("tasks lock");
        tasks.push(tokio::spawn(scheduler.run()));
        tasks.push(tokio::spawn(self.pool.clone().run_maintenance()));
        tasks.push(tokio::spawn(follow_settings(
            self.settings.clone(),
            self.schedule.clone(),
            self.brain.clone(),
        )));
    }

    fn stop(&self) {
        for task in self.tasks.lock().expect("tasks lock").drain(..) {
            task.abort();
        }
    }
}

/// Picks up settings that another server process changed, and brings the
/// schedules in line after every change.
async fn follow_settings(settings: TenantSettings, schedule: Arc<ScheduleSync>, brain: Brain) {
    let mut ticker = tokio::time::interval(SETTINGS_INTERVAL);
    let mut changes = settings.subscribe();
    let brain_models = |settings: &TenantSettings| {
        settings
            .effective(None)
            .values
            .get(lighthouse_settings::BRAIN)
            .map(|(value, _)| (value["provider"].clone(), value["embedding_model"].clone()))
    };
    let mut embedding = brain_models(&settings);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = settings.refresh().await {
                    tracing::warn!(%error, "cannot refresh the settings");
                }
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                schedule.refresh();
                // A new embedding model rebuilds the vectors in the background.
                // Search compares only vectors of the current model, so until
                // then it finds less, never wrong.
                let now = brain_models(&settings);
                if now != embedding {
                    embedding = now;
                    let brain = brain.clone();
                    tokio::spawn(async move {
                        match brain.embed_missing().await {
                            Ok(done) => tracing::info!(done, "embedded the Brain for a new model"),
                            Err(error) => tracing::warn!(%error, "embedding failed, search stays lexical for now"),
                        }
                    });
                }
            }
        }
    }
}

/// Takes over agents or automations created as files before they moved to the
/// database, into the tenant layer, and sets the directory aside. What is
/// already in the database stays. Failing to move the directory changes nothing:
/// the next start finds the same files and keeps what is there.
async fn import_created(
    settings: &TenantSettings,
    directory: &std::path::Path,
    kind: FileKind,
) -> anyhow::Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        let id = match kind {
            FileKind::Agent => AgentDefinition::parse(&source).map(|agent| agent.id).ok(),
            _ => lighthouse_core::automation::Automation::parse(&source)
                .map(|automation| automation.id)
                .ok(),
        };
        let Some(id) = id else {
            tracing::warn!(path = %path.display(), "leaving an invalid file behind");
            continue;
        };
        if settings.import_file(kind, &id, &source).await? {
            tracing::info!(%id, kind = kind.as_str(), "imported a created file");
        }
    }
    let mut moved = directory.as_os_str().to_owned();
    moved.push(".imported");
    if let Err(error) = std::fs::rename(directory, &moved) {
        tracing::warn!(%error, path = %directory.display(), "cannot set the imported directory aside");
    }
    Ok(())
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
    // Mail goes through the instance's mailer, so SMTP is not set up again here.
    let delivery = lighthouse_config::DeliveryConfig {
        smtp: None,
        slack_channels: config.delivery.slack_channels.clone(),
    };
    let settings = TenantSettings::open(
        store.clone(),
        tenant,
        shared.defaults.clone(),
        shared.options,
    )
    .await
    .context("cannot read the settings of the tenant")?;
    let resolver = Arc::new(
        tenant_config::TenantConfig::new(
            settings.clone(),
            vault.clone(),
            shared.key_fallback(tenant),
        )
        .with_person_defaults(
            (tenant == TenantId::DEFAULT).then(|| tenant_config::InstanceOwner {
                name: owner.name.clone(),
                email: owner.email.clone(),
                accounts: owner.accounts.clone(),
            }),
            zone,
        ),
    );
    let brain = open_brain(shared, tenant, paths.brain, &settings, &resolver)?;
    let report = brain.sync().await.context("cannot read the Brain")?;
    tracing::info!(?report, "Brain ready");
    let embedding = brain.clone();
    tokio::spawn(async move {
        if let Err(error) = embedding.embed_missing().await {
            tracing::warn!(%error, "embedding failed, search stays lexical for now");
        }
    });
    let mcp: Arc<dyn ToolHostFactory> = Arc::new(McpConnector::live(resolver.clone()));
    let sync_tools = mcp.clone();
    let runtime = Arc::new(Runtime {
        router: Arc::new(LiveRouterSource::new(resolver.clone())),
        tools: Arc::new(BrainToolFactory::new(
            brain.clone(),
            Arc::new(DeskToolFactory::new(desk.clone(), mcp)),
        )),
        policy: ToolPolicy::live(resolver.clone()),
        skills: lighthouse_runtime::SkillRegistry::from(SkillLibrary::load(&config.paths.skills)?)
            .layered(settings.clone()),
        audit: audit.clone(),
        notifier: Arc::new({
            let notifier = ChannelNotifier::with_vaults(
                delivery,
                vault.clone(),
                shared.vault(TenantId::DEFAULT),
            )?
            .with_slack(resolver.clone());
            match &shared.mailer {
                Some(mailer) => notifier.with_mailer(mailer.clone()),
                None => notifier,
            }
        }),
        owner_context: owner_context(&owner),
        owner_email: owner.email.clone(),
        zone,
        desk: desk.clone(),
        people: Some(resolver.clone()),
    });
    import_created(&settings, &paths.created_agents, FileKind::Agent).await?;
    import_created(&settings, &paths.created_automations, FileKind::Automation).await?;
    let agents = Arc::new(AgentRegistry::layered(
        config.paths.agents.clone(),
        settings.clone(),
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
        settings: Some(settings.clone()),
        people: Some(resolver.clone()),
    }));
    let automation_files = Arc::new(AutomationRegistry::layered(
        config.paths.automations.clone(),
        settings.clone(),
    )?);
    tracing::info!(count = automation_files.list().len(), "automations loaded");
    let engine = Arc::new(
        AutomationEngine::new(
            runtime.clone(),
            agents.clone(),
            automation_files.clone(),
            outcomes.clone(),
            desk.clone(),
        )
        .with_entitlements(tenant, shared.entitlements.clone())
        .with_sessions(lighthouse_sessions::Sessions::new(store.clone(), tenant)),
    );
    tracing::info!(count = resolver.sources().len(), "connected sources loaded");
    let sync_store = lighthouse_sync::SyncStore::new(store.clone(), tenant);
    let syncer = Arc::new(lighthouse_sync::Syncer::live(
        sync_store.clone(),
        sync_tools,
        resolver.clone(),
    ));
    let connected = Arc::new(
        ConnectedService::new(
            syncer,
            runtime.clone(),
            engine.clone(),
            outcomes.clone(),
            events.clone(),
        )
        .with_resolver(resolver.clone()),
    );
    let publisher = Arc::new(publishing::ServerPublisher::new(
        runtime.clone(),
        outcomes.clone(),
        settings.clone(),
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
        settings: settings.clone(),
        vault: vault.clone(),
    });
    let scheduler = Scheduler::new(
        store.clone(),
        tenant,
        &config.server.timezone,
        &definitions,
        pool.clone(),
    )?;
    let schedule = Arc::new(ScheduleSync {
        settings: settings.clone(),
        resolver: resolver.clone(),
        agents: agents.clone(),
        automations: automation_files,
        schedules: scheduler.handle(),
    });
    schedule.refresh();
    let automations = Arc::new(ServerAutomations::new(
        engine,
        schedule.clone(),
        resolver.clone(),
    ));
    let catalog = Arc::new(ServerCatalog {
        settings: settings.clone(),
        vault: vault.clone(),
        runtime: pool.runtime.clone(),
        agents,
        schedule: schedule.clone(),
        resolver: resolver.clone(),
    });
    let assistant = Arc::new(BrainAssistant {
        brain: brain.clone(),
        runtime: pool.runtime.clone(),
    });
    let pool_brain = brain.clone();
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
        tenant_zone: zone,
        automations,
        sync: sync_store,
        connected,
        publisher,
        talk_agent: config.talk.agent.clone(),
        writer: Arc::new(publishing::ServerWriter::new(pool.runtime.clone())),
        settings: settings.clone(),
        instance_vault: shared.key_fallback(tenant),
    };
    Ok(Workspace {
        tenant,
        state,
        pool,
        settings,
        schedule,
        brain: pool_brain,
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
    /// Accounts, tenants and mail of the instance, the same handle
    /// [`Lifecycle::on_start`] gets.
    pub instance: Instance,
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
    /// Sends every mail of the server: verification, invitations and reports
    /// to `mail:` targets. Without one, `[delivery.smtp]` is used if set.
    pub mailer: Option<Arc<dyn Mailer>>,
    /// Texts of the verification and invitation mails.
    pub mail_templates: Arc<dyn MailTemplates>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            lifecycle: Arc::new(NoHooks),
            entitlements: Arc::new(AllowAll),
            routes: Extensions::default(),
            mailer: None,
            mail_templates: Arc::new(EnglishMails),
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
    if config.tenancy.require_verified_email
        && hooks.mailer.is_none()
        && config.delivery.smtp.is_none()
    {
        anyhow::bail!(
            "tenancy.require_verified_email needs mail: configure [delivery.smtp] or pass a mailer"
        );
    }
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
    let key = master_key(&config)?;
    let mailer: Option<Arc<dyn Mailer>> = match (hooks.mailer, &config.delivery.smtp) {
        (Some(mailer), _) => Some(mailer),
        (None, Some(smtp)) => Some(Arc::new(SmtpMailer::from_config(
            smtp.clone(),
            Vault::new(store.clone(), TenantId::DEFAULT, &key),
        )?)),
        (None, None) => None,
    };
    let shared = Arc::new(Shared {
        key,
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
        mailer: mailer.clone(),
        defaults: Arc::new(Defaults::from_config(&config)),
        options: lighthouse_settings::Options::from_config(&config),
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
    let instance = Instance {
        accounts,
        tenants: workspaces.clone(),
        oauth: shared.oauth.clone(),
        admin_token: config.server.admin_token.clone(),
        signup: signup_mode(config.tenancy.signup),
        require_verified_email: config.tenancy.require_verified_email,
        mailer,
        mail_templates: hooks.mail_templates,
        public_url: config.server.public_url.trim_end_matches('/').to_owned(),
        trust_forwarded_for: config.server.trust_forwarded_for,
        lifecycle: hooks.lifecycle,
        entitlements: hooks.entitlements,
        extension_config: Arc::new(config.extensions.clone()),
    };
    instance.lifecycle.on_start(&instance).await;
    let router = lighthouse_api::router_with(instance.clone(), hooks.routes);
    let router = match &config.paths.web {
        Some(directory) => lighthouse_api::with_web_app(router, directory),
        None => router,
    };
    Ok(App {
        router,
        workspaces,
        instance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confirmed_addresses_need_mail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lighthouse.toml");
        let toml = "[server]\nbind = \"127.0.0.1:0\"\npublic_url = \"https://lighthouse.test\"\n\
                    [database]\nurl = \"postgres://nowhere.invalid/none\"\n[vault]\n\
                    [owner]\nname = \"Alex\"\nemail = \"alex@example.com\"\n\
                    [providers]\n[routing]\n[tenancy]\nrequire_verified_email = true\n";
        std::fs::write(&path, toml).unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.delivery.smtp.is_none());
        let error = build_with(config, Hooks::default()).await.err().unwrap();
        assert!(error.to_string().contains("needs mail"), "{error}");
    }

    async fn store_with_people() -> (Store, Uuid, Uuid) {
        let store = lighthouse_store::testing::fresh_store().await;
        let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
        for (id, email, role) in [
            (first, "a@example.com", "owner"),
            (second, "b@example.com", "member"),
        ] {
            lighthouse_store::sqlx::query(
                "INSERT INTO users (id, email, name) VALUES ($1, $2, 'x')",
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
        (store, first, second)
    }

    fn scheduled(name: &str, cron: &str) -> String {
        format!("---\nname: {name}\nscope: org\ntrigger:\n  cron: \"{cron}\"\n---\nGo.\n")
    }

    #[tokio::test]
    async fn schedules_run_for_their_person_in_their_time_zone() {
        let (store, first, second) = store_with_people().await;
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("agents");
        std::fs::create_dir(&managed).unwrap();
        std::fs::write(
            managed.join("built-in.md"),
            scheduled("Built in", "0 7 * * *"),
        )
        .unwrap();
        std::fs::write(
            managed.join("skipped.md"),
            scheduled("Skipped", "0 7 * * *"),
        )
        .unwrap();
        let settings = TenantSettings::open(
            store.clone(),
            TenantId::DEFAULT,
            Arc::default(),
            lighthouse_settings::Options::default(),
        )
        .await
        .unwrap();
        let key = MasterKey::from_base64("QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE=").unwrap();
        let resolver = Arc::new(
            tenant_config::TenantConfig::new(
                settings.clone(),
                Vault::new(store.clone(), TenantId::DEFAULT, &key),
                None,
            )
            .with_person_defaults(None, chrono_tz::Europe::Berlin),
        );
        let agents = Arc::new(AgentRegistry::layered(managed, settings.clone()).unwrap());
        let automations = Arc::new(
            AutomationRegistry::layered(dir.path().join("automations"), settings.clone()).unwrap(),
        );
        struct Runner;
        #[async_trait]
        impl AgentRunner for Runner {
            async fn start(&self, _: &str, _: &str) {}
        }
        let scheduler =
            Scheduler::new(store, TenantId::DEFAULT, "UTC", &[], Arc::new(Runner)).unwrap();
        let sync = ScheduleSync {
            settings: settings.clone(),
            resolver,
            agents,
            automations,
            schedules: scheduler.handle(),
        };
        let tokyo = serde_json::json!("Asia/Tokyo");
        settings
            .set_value(
                Some(second),
                Some(second),
                lighthouse_settings::TIMEZONE,
                &tokyo,
            )
            .await
            .unwrap();
        settings
            .set_file(
                Some(second),
                Some(second),
                FileKind::Agent,
                "own",
                &scheduled("Own", "0 8 * * *"),
            )
            .await
            .unwrap();
        settings
            .set_file(
                Some(first),
                Some(first),
                FileKind::Agent,
                "built-in",
                &scheduled("Built in", "0 9 * * *"),
            )
            .await
            .unwrap();
        settings
            .switch_file(None, None, FileKind::Agent, "skipped", false)
            .await
            .unwrap();
        let automation =
            "---\nname: Nightly\ntrigger:\n  cron: \"0 1 * * *\"\nsteps:\n  - agent: own\n---\nx\n";
        settings
            .set_file(
                Some(second),
                Some(second),
                FileKind::Automation,
                "nightly",
                automation,
            )
            .await
            .unwrap();
        let jobs = sync.jobs();
        let job = |id: String| jobs.iter().find(|job| job.id == id).cloned();
        let own = job(format!("own@{second}")).unwrap();
        assert_eq!(
            (own.cron.as_str(), own.zone),
            ("0 8 * * *", chrono_tz::Asia::Tokyo)
        );
        assert_eq!(
            job(format!("{RUN_PREFIX}nightly@{second}")).unwrap().zone,
            chrono_tz::Asia::Tokyo
        );
        let replaced = job(format!("built-in@{first}")).unwrap();
        assert_eq!(
            (replaced.cron.as_str(), replaced.zone),
            ("0 9 * * *", chrono_tz::Europe::Berlin)
        );
        assert!(
            job("built-in".to_owned()).is_none(),
            "not a second time for the tenant"
        );
        assert!(
            job("skipped".to_owned()).is_none(),
            "the tenant switched it off"
        );
        assert_eq!(split_job(&format!("own@{second}")), ("own", Some(second)));
        assert_eq!(split_job("brief"), ("brief", None));
        sync.refresh();
    }

    #[tokio::test]
    async fn the_brain_follows_its_settings_and_rebuilds_vectors_for_a_new_model() {
        use axum::routing::post;
        let (store, _first, second) = store_with_people().await;
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let recorded = seen.clone();
        let router = axum::Router::new().route(
            "/v1/embeddings",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let recorded = recorded.clone();
                async move {
                    recorded
                        .lock()
                        .unwrap()
                        .push(body["model"].as_str().unwrap().to_owned());
                    let count = body["input"].as_array().unwrap().len();
                    let data: Vec<_> = (0..count)
                        .map(|index| serde_json::json!({"index": index, "embedding": [1.0, 0.0]}))
                        .collect();
                    axum::Json(serde_json::json!({"data": data}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let settings = TenantSettings::open(
            store.clone(),
            TenantId::DEFAULT,
            Arc::default(),
            lighthouse_settings::Options {
                allow_private_endpoints: true,
                ..lighthouse_settings::Options::default()
            },
        )
        .await
        .unwrap();
        let key = MasterKey::from_base64("QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE=").unwrap();
        let vault = Vault::new(store.clone(), TenantId::DEFAULT, &key);
        let resolver = Arc::new(tenant_config::TenantConfig::new(
            settings.clone(),
            vault.clone(),
            None,
        ));
        let media = Arc::new(ProviderMedia {
            settings: settings.clone(),
            resolver: resolver.clone(),
        });
        assert!(!media.active(), "nothing set");
        assert!(matches!(
            media.embed(&["x".to_owned()]).await,
            Err(MediaError::NotConfigured(_))
        ));
        settings
            .set_provider(
                None,
                None,
                "local",
                &lighthouse_settings::ProviderInput {
                    kind: "openai".to_owned(),
                    base_url: Some(url),
                },
            )
            .await
            .unwrap();
        vault
            .put(
                "provider:local",
                &secrecy::SecretString::from("key".to_owned()),
            )
            .await
            .unwrap();
        let models = |model: &str| serde_json::json!({"provider": "local", "embedding_model": model, "vision_model": "tenant-eyes"});
        settings
            .set_value(None, None, lighthouse_settings::BRAIN, &models("first"))
            .await
            .unwrap();
        // A person's own vision model does not touch the shared embeddings.
        settings
            .set_value(
                Some(second),
                Some(second),
                lighthouse_settings::BRAIN,
                &serde_json::json!({"vision_model": "my-eyes"}),
            )
            .await
            .unwrap();
        assert_eq!(
            media.models(Some(second)).vision_model.as_deref(),
            Some("my-eyes")
        );
        assert_eq!(
            media.models(Some(second)).provider.as_deref(),
            Some("local")
        );
        assert_eq!(
            media.models(None).vision_model.as_deref(),
            Some("tenant-eyes")
        );
        assert_eq!(media.model(), "first");
        let dir = tempfile::tempdir().unwrap();
        let brain = Brain::new(store.clone(), TenantId::DEFAULT, dir.path().to_path_buf())
            .unwrap()
            .with_media(media.clone())
            .with_embedder(media.clone());
        brain
            .remember(lighthouse_brain::NewEntry {
                kind: "note".to_owned(),
                title: "A note".to_owned(),
                body: "Something to find".to_owned(),
                sources: vec![lighthouse_brain::Source {
                    kind: "app".to_owned(),
                    reference: "test".to_owned(),
                }],
                links: Vec::new(),
            })
            .await
            .unwrap();
        let vector_models = || async {
            let rows: Vec<(Option<String>,)> =
                lighthouse_store::sqlx::query_as("SELECT embedding_model FROM brain_chunks")
                    .fetch_all(store.pool())
                    .await
                    .unwrap();
            rows.into_iter().map(|(model,)| model).collect::<Vec<_>>()
        };
        brain.embed_missing().await.unwrap();
        assert!(
            vector_models()
                .await
                .iter()
                .all(|model| model.as_deref() == Some("first"))
        );
        // A changed model rebuilds the vectors in the background.
        let scheduler = Scheduler::new(
            store.clone(),
            TenantId::DEFAULT,
            "UTC",
            &[],
            Arc::new(NoRunner),
        )
        .unwrap();
        let schedule = Arc::new(ScheduleSync {
            settings: settings.clone(),
            resolver,
            agents: Arc::new(
                AgentRegistry::layered(dir.path().join("a"), settings.clone()).unwrap(),
            ),
            automations: Arc::new(
                AutomationRegistry::layered(dir.path().join("b"), settings.clone()).unwrap(),
            ),
            schedules: scheduler.handle(),
        });
        let following = tokio::spawn(follow_settings(settings.clone(), schedule, brain.clone()));
        tokio::task::yield_now().await;
        settings
            .set_value(None, None, lighthouse_settings::BRAIN, &models("second"))
            .await
            .unwrap();
        for _ in 0..100 {
            if vector_models()
                .await
                .iter()
                .all(|model| model.as_deref() == Some("second"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        following.abort();
        assert!(
            vector_models()
                .await
                .iter()
                .all(|model| model.as_deref() == Some("second"))
        );
        assert!(seen.lock().unwrap().contains(&"second".to_owned()));
        assert!(!brain.search("Something", 3).await.unwrap().is_empty());
    }

    struct NoRunner;

    #[async_trait]
    impl AgentRunner for NoRunner {
        async fn start(&self, _: &str, _: &str) {}
    }

    #[tokio::test]
    async fn files_created_before_are_taken_over_once() {
        let (store, _, _) = store_with_people().await;
        let settings = TenantSettings::open(
            store,
            TenantId::DEFAULT,
            Arc::default(),
            lighthouse_settings::Options::default(),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let created = dir.path().join("created");
        std::fs::create_dir(&created).unwrap();
        std::fs::write(created.join("old.md"), scheduled("Old", "0 7 * * *")).unwrap();
        std::fs::write(created.join("bad.md"), "nonsense").unwrap();
        import_created(&settings, &created, FileKind::Agent)
            .await
            .unwrap();
        let layer = settings.layer(None);
        assert_eq!(layer.files.len(), 1);
        assert!(
            layer
                .files
                .contains_key(&(FileKind::Agent, "old".to_owned()))
        );
        assert!(!created.exists(), "set aside");
        assert!(dir.path().join("created.imported/old.md").exists());
        import_created(&settings, &created, FileKind::Agent)
            .await
            .unwrap();
        assert_eq!(settings.layer(None).files.len(), 1);
    }

    #[test]
    fn model_routes_come_from_routing_and_providers() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../demo/lighthouse.demo.toml");
        let mut config = Config::load(&path).unwrap();
        let effective =
            |config: &Config| Effective::resolve(&Defaults::from_config(config), None, None, false);
        let route = model_route(&effective(&config), "text").unwrap();
        assert_eq!(
            (route.provider.as_str(), route.model.as_deref()),
            ("demo", Some("demo"))
        );
        assert_eq!((route.fallback, route.fallback_model), (None, None));
        config.routing.get_mut("text").unwrap().fallback = Some("demo".to_owned());
        let route = model_route(&effective(&config), "text").unwrap();
        assert_eq!(route.fallback_model.as_deref(), Some("demo"));
        assert!(model_route(&effective(&config), "vision").is_none());
    }
}
