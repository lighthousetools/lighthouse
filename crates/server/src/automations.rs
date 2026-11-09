//! Starts automations on schedules, events and webhooks, and serves them to the apps.

use crate::outcomes::Outcomes;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use lighthouse_api::hooks::{Action, Entitlements};
use lighthouse_api::{
    AgentChangeError, AutomationDryRun, AutomationInfo, AutomationStepInfo, AutomationTriggerInfo,
    Automations, DryRunRequest, PlannedStep, TriggerError,
};
use lighthouse_audit::{AuditError, RunRecord};
use lighthouse_core::agent::{ToolLevel, parse_cron, slugify};
use lighthouse_core::automation::{
    Automation, AutomationStep, AutomationTrigger, Condition, RUN_PREFIX, matches,
};
use lighthouse_core::{RunId, TenantId};
use lighthouse_desk::Desk;
use lighthouse_runtime::{
    AgentOutcome, AgentRegistry, AgentSteps, AutomationRegistry, LoadedAutomation, RegistryError,
    Runtime,
};
use lighthouse_settings::FileKind;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Runs automations and the agent steps inside them.
pub struct AutomationEngine {
    runtime: Arc<Runtime>,
    agents: Arc<AgentRegistry>,
    automations: Arc<AutomationRegistry>,
    outcomes: Arc<Outcomes>,
    desk: Desk,
    limits: Option<(TenantId, Arc<dyn Entitlements>)>,
}

impl std::fmt::Debug for AutomationEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationEngine").finish_non_exhaustive()
    }
}

fn event_of(record: &RunRecord) -> Option<&'static str> {
    match record.status.as_str() {
        "succeeded" => Some("run.succeeded"),
        "waiting" => Some("run.waiting"),
        "failed" | "limit_exceeded" | "rejected" => Some("run.failed"),
        _ => None,
    }
}

impl AutomationEngine {
    /// Creates the engine.
    pub fn new(
        runtime: Arc<Runtime>,
        agents: Arc<AgentRegistry>,
        automations: Arc<AutomationRegistry>,
        outcomes: Arc<Outcomes>,
        desk: Desk,
    ) -> Self {
        Self {
            runtime,
            agents,
            automations,
            outcomes,
            desk,
            limits: None,
        }
    }

    /// Asks the entitlements of the tenant before every agent step.
    #[must_use]
    pub fn with_entitlements(
        mut self,
        tenant: TenantId,
        entitlements: Arc<dyn Entitlements>,
    ) -> Self {
        self.limits = Some((tenant, entitlements));
        self
    }

    async fn switches(&self) -> HashMap<String, bool> {
        match self.desk.automation_switches().await {
            Ok(switches) => switches,
            Err(error) => {
                tracing::error!(%error, "cannot read the automation switches");
                HashMap::new()
            }
        }
    }

    /// Whether an automation may run: no layer switched it off, and for a
    /// shared one the tenant's switch is on, or its file says so.
    fn enabled(switches: &HashMap<String, bool>, known: &LoadedAutomation) -> bool {
        if !known.enabled {
            return false;
        }
        if known.layer == lighthouse_settings::Layer::Personal {
            return known.automation.enabled;
        }
        switches
            .get(&known.automation.id)
            .copied()
            .unwrap_or(known.automation.enabled)
    }

    /// Registers a run of an automation and runs it in the background.
    pub async fn start(
        self: &Arc<Self>,
        automation: Arc<Automation>,
        trigger: &str,
        payload: Value,
    ) -> Result<RunId, AuditError> {
        let run = RunId::new();
        self.runtime
            .audit
            .start_run(run, &automation.run_agent_id(), trigger)
            .await?;
        let engine = self.clone();
        tokio::spawn(async move {
            let result = engine
                .runtime
                .run_automation(run, &automation, payload, &*engine)
                .await;
            if let Err(error) = result {
                tracing::error!(automation = %automation.id, %error, "automation failed");
            }
            engine.finished(run).await;
        });
        Ok(run)
    }

    async fn finished(&self, run: RunId) {
        if let Err(error) = self.outcomes.after_run(run.as_uuid()).await {
            tracing::error!(run = %run.as_uuid(), %error, "cannot handle the end of a run");
        }
    }

    /// Continues a paused automation run.
    pub async fn resume(self: &Arc<Self>, run: RunId, agent_id: &str, state: Value) {
        let id = agent_id.strip_prefix(RUN_PREFIX).unwrap_or(agent_id);
        let user = self
            .runtime
            .audit
            .run_user(run.as_uuid())
            .await
            .ok()
            .flatten();
        let Some(known) = self.automations.get_for(user, id) else {
            let reason = format!("the automation `{id}` no longer exists");
            let end = lighthouse_audit::RunEnd {
                status: lighthouse_audit::RunStatus::Failed,
                reason: Some(&reason),
                report: None,
            };
            if let Err(error) = self.runtime.audit.finish_run(run, end).await {
                tracing::error!(%error, "cannot end the run");
            }
            self.finished(run).await;
            return;
        };
        let engine = self.clone();
        tokio::spawn(async move {
            let automation = known.automation;
            let result = engine
                .runtime
                .resume_automation(run, &automation, state, &*engine)
                .await;
            if let Err(error) = result {
                tracing::error!(automation = %automation.id, %error, "resumed automation failed");
            }
            engine.finished(run).await;
        });
    }

    /// Resumes the automations that waited for an agent run, right after it ended.
    async fn resume_waiting(self: &Arc<Self>, child: Uuid) {
        let audit = &self.runtime.audit;
        let waiting = match audit.runs_waiting_on(child).await {
            Ok(waiting) => waiting,
            Err(error) => {
                tracing::error!(%error, "cannot find waiting automations");
                return;
            }
        };
        for run in waiting {
            match audit.take_checkpoint(run).await {
                Ok(Some(checkpoint)) => {
                    self.resume(
                        RunId::from_uuid(run),
                        &checkpoint.agent_id,
                        checkpoint.state,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(error) => tracing::error!(%error, "cannot take the checkpoint"),
            }
        }
    }

    /// Starts a scheduled automation, given as `automation:<id>`.
    pub async fn start_scheduled(self: &Arc<Self>, scheduled: &str, user: Option<Uuid>) {
        let id = scheduled.strip_prefix(RUN_PREFIX).unwrap_or(scheduled);
        let Some(known) = self.automations.get_for(user, id) else {
            return;
        };
        if !Self::enabled(&self.switches().await, &known) {
            return;
        }
        let started = self
            .runtime
            .as_user(user, self.start(known.automation, "cron", Value::Null))
            .await;
        if let Err(error) = started {
            tracing::error!(automation = %id, %error, "cannot start the automation");
        }
    }

    /// Starts the automations a person made for themselves that wait for an
    /// event of theirs, such as one from their own source. Shared automations
    /// never hear of it.
    pub async fn emit_personal(
        self: &Arc<Self>,
        user: Uuid,
        name: &str,
        payload: Value,
    ) -> Vec<Uuid> {
        let known: Vec<LoadedAutomation> = self
            .automations
            .for_user(Some(user))
            .values()
            .filter(|known| known.layer == lighthouse_settings::Layer::Personal)
            .cloned()
            .collect();
        self.runtime
            .as_user(Some(user), self.start_matching(known, name, payload))
            .await
    }

    /// Starts every enabled automation that waits for this event.
    pub async fn emit(self: &Arc<Self>, name: &str, payload: Value) -> Vec<Uuid> {
        let known = self.automations.list();
        self.start_matching(known, name, payload).await
    }

    async fn start_matching(
        self: &Arc<Self>,
        known: Vec<LoadedAutomation>,
        name: &str,
        payload: Value,
    ) -> Vec<Uuid> {
        let switches = self.switches().await;
        let mut runs = Vec::new();
        for known in known {
            let enabled = Self::enabled(&switches, &known);
            let automation = known.automation;
            let AutomationTrigger::Event {
                name: wanted,
                filter,
            } = &automation.trigger
            else {
                continue;
            };
            if wanted != name || !enabled || !matches(filter, &payload) {
                continue;
            }
            let trigger = format!("event:{name}");
            match self
                .start(automation.clone(), &trigger, payload.clone())
                .await
            {
                Ok(run) => runs.push(run.as_uuid()),
                Err(error) => {
                    tracing::error!(automation = %automation.id, %error, "cannot start the automation");
                }
            }
        }
        runs
    }

    /// Turns the end of an agent run into a `run.*` event for automations.
    /// Runs that automations started never trigger automations, so they cannot
    /// loop.
    pub async fn after_agent_run(self: &Arc<Self>, record: &RunRecord) {
        if record.trigger.starts_with(RUN_PREFIX)
            && !matches!(record.status.as_str(), "running" | "waiting")
        {
            self.resume_waiting(record.id).await;
        }
        if record.agent_id.starts_with(RUN_PREFIX) || record.trigger.starts_with(RUN_PREFIX) {
            return;
        }
        let Some(event) = event_of(record) else {
            return;
        };
        let payload = json!({
            "run_id": record.id,
            "agent_id": record.agent_id,
            "status": record.status,
            "reason": record.reason,
            "trigger": record.trigger,
        });
        // A person's own agent concerns only them; other runs concern the
        // tenant, and the person they worked for hears of them too.
        let user = self.runtime.audit.run_user(record.id).await.ok().flatten();
        let own = user.is_some_and(|user| {
            self.agents
                .get_for(Some(user), &record.agent_id)
                .is_some_and(|known| known.layer == lighthouse_settings::Layer::Personal)
        });
        if !own {
            self.emit(event, payload.clone()).await;
        }
        if let Some(user) = user {
            self.emit_personal(user, event, payload).await;
        }
    }
}

#[async_trait]
impl AgentSteps for AutomationEngine {
    async fn run_agent(
        &self,
        agent_id: &str,
        trigger: &str,
        context: &str,
    ) -> Result<AgentOutcome, String> {
        let known = self
            .agents
            .get(agent_id)
            .ok_or_else(|| format!("unknown agent `{agent_id}`"))?;
        if let Some((tenant, entitlements)) = &self.limits {
            let action = Action::RunAgent {
                agent_id: agent_id.to_owned(),
            };
            entitlements.check(*tenant, &action).await?;
        }
        let run = RunId::new();
        let failed = |error: AuditError| error.to_string();
        self.runtime
            .register(run, &known.agent, trigger)
            .await
            .map_err(failed)?;
        self.runtime
            .execute_with_context(run, &known.agent, Some(context))
            .await
            .map_err(failed)?;
        let record = self
            .outcomes
            .after_run(run.as_uuid())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the run disappeared".to_owned())?;
        Ok(AgentOutcome {
            run_id: record.id,
            status: record.status,
            report: record.report,
            reason: record.reason,
        })
    }
}

/// Placeholder output of steps a dry run does not run.
const DRY_OUTPUT: &str = "(output of the previous step)";

fn plan_steps(
    steps: &[AutomationStep],
    prefix: &str,
    event: &Value,
    policy: &lighthouse_mcp::ToolPolicy,
    planned: &mut Vec<PlannedStep>,
) {
    let values = std::collections::BTreeMap::from([
        ("output", DRY_OUTPUT.to_owned()),
        ("status", "succeeded".to_owned()),
        ("automation", String::new()),
    ]);
    for (index, step) in steps.iter().enumerate() {
        let path = format!("{prefix}{}", index + 1);
        let mut entry = PlannedStep {
            path: path.clone(),
            kind: step.kind().to_owned(),
            summary: String::new(),
            input: None,
            message: None,
            needs_approval: false,
        };
        match step {
            AutomationStep::Agent(agent) => entry.summary = format!("run the agent {agent}"),
            AutomationStep::Tool { name, input } => {
                entry.summary = format!("call {name}");
                entry.input = Some(lighthouse_core::automation::render_value(
                    input, &values, event,
                ));
                entry.needs_approval = policy.level(name) == ToolLevel::Write;
            }
            AutomationStep::Notify { to, message, .. } => {
                entry.summary = format!("notify {}", crate::delivery_name(to));
                entry.message = Some(lighthouse_core::automation::render(message, &values, event));
            }
            AutomationStep::If(condition) => entry.summary = condition_summary(condition),
            AutomationStep::Branch {
                condition,
                then,
                otherwise,
            } => {
                entry.summary = format!("branch: {}", condition_summary(condition));
                planned.push(entry);
                plan_steps(then, &format!("{path}.then."), event, policy, planned);
                plan_steps(otherwise, &format!("{path}.else."), event, policy, planned);
                continue;
            }
        }
        planned.push(entry);
    }
}

/// Serves the automations to the apps.
pub struct ServerAutomations {
    engine: Arc<AutomationEngine>,
    schedule: Arc<crate::ScheduleSync>,
    resolver: Arc<crate::tenant_config::TenantConfig>,
}

/// Whether a step of an automation mails an address directly.
fn notifies_mail(steps: &[AutomationStep]) -> bool {
    steps.iter().any(|step| match step {
        AutomationStep::Notify { to, .. } => {
            matches!(to, lighthouse_core::agent::Delivery::Mail(_))
        }
        AutomationStep::Branch {
            then, otherwise, ..
        } => notifies_mail(then) || notifies_mail(otherwise),
        _ => false,
    })
}

fn registry_error(error: RegistryError) -> AgentChangeError {
    match error {
        RegistryError::Io(_) => AgentChangeError::Failed(error.to_string()),
        other => AgentChangeError::Rejected(other.to_string()),
    }
}

fn condition_summary(condition: &Condition) -> String {
    match condition {
        Condition::Contains(text) => format!("continue if the output contains \"{text}\""),
        Condition::NotContains(text) => {
            format!("continue unless the output contains \"{text}\"")
        }
        Condition::Status(status) => format!("continue if the previous step {status}"),
    }
}

impl ServerAutomations {
    /// Serves the automations of an engine.
    pub(crate) fn new(
        engine: Arc<AutomationEngine>,
        schedule: Arc<crate::ScheduleSync>,
        resolver: Arc<crate::tenant_config::TenantConfig>,
    ) -> Self {
        Self {
            engine,
            schedule,
            resolver,
        }
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

    fn info(&self, known: &LoadedAutomation, switches: &HashMap<String, bool>) -> AutomationInfo {
        let automation = &known.automation;
        let policy = &self.engine.runtime.policy;
        let (kind, cron, event, filter) = match &automation.trigger {
            AutomationTrigger::Cron(cron) => ("cron", Some(cron.clone()), None, Default::default()),
            AutomationTrigger::Event { name, filter } => {
                ("event", None, Some(name.clone()), filter.clone())
            }
            AutomationTrigger::Webhook { filter } => ("webhook", None, None, filter.clone()),
            AutomationTrigger::Manual => ("manual", None, None, Default::default()),
        };
        let enabled = AutomationEngine::enabled(switches, known);
        AutomationInfo {
            layer: known.layer.as_str().to_owned(),
            disabled_by: known.disabled_by.map(|layer| layer.as_str().to_owned()),
            id: automation.id.clone(),
            name: automation.name.clone(),
            description: automation.description.clone(),
            enabled,
            next_run: cron
                .as_deref()
                .filter(|_| enabled)
                .and_then(|cron| self.next_run(cron)),
            trigger: AutomationTriggerInfo {
                kind: kind.to_owned(),
                cron,
                event,
                filter,
            },
            steps: automation
                .steps
                .iter()
                .map(|step| {
                    let (summary, needs_approval) = match step {
                        AutomationStep::Agent(agent) => (format!("run the agent {agent}"), false),
                        AutomationStep::Tool { name, .. } => (
                            format!("call {name}"),
                            policy.level(name) == ToolLevel::Write,
                        ),
                        AutomationStep::Notify { to, .. } => {
                            (format!("notify {}", crate::delivery_name(to)), false)
                        }
                        AutomationStep::If(condition) => (condition_summary(condition), false),
                        AutomationStep::Branch { condition, .. } => {
                            (format!("branch: {}", condition_summary(condition)), false)
                        }
                    };
                    AutomationStepInfo {
                        kind: step.kind().to_owned(),
                        summary,
                        needs_approval,
                    }
                })
                .collect(),
            editable: known.editable,
            source: known.source.clone(),
        }
    }

    fn settings(&self) -> Result<&lighthouse_settings::TenantSettings, AgentChangeError> {
        self.engine
            .automations
            .settings()
            .ok_or_else(|| AgentChangeError::Failed("no settings".to_owned()))
    }

    fn known(&self, id: &str) -> Result<LoadedAutomation, TriggerError> {
        self.engine
            .automations
            .get(id)
            .ok_or_else(|| TriggerError::UnknownAgent(id.to_owned()))
    }
}

#[async_trait]
impl Automations for ServerAutomations {
    async fn list(&self) -> Result<Vec<AutomationInfo>, String> {
        let switches = self.engine.switches().await;
        Ok(self
            .engine
            .automations
            .list()
            .iter()
            .map(|known| self.info(known, &switches))
            .collect())
    }

    async fn get(&self, id: &str) -> Result<Option<AutomationInfo>, String> {
        let switches = self.engine.switches().await;
        Ok(self
            .engine
            .automations
            .get(id)
            .map(|known| self.info(&known, &switches)))
    }

    async fn save(
        &self,
        source: &str,
        owner: Option<Uuid>,
    ) -> Result<AutomationInfo, AgentChangeError> {
        let automation = Automation::parse(source)
            .map_err(|error| AgentChangeError::Rejected(error.to_string()))?;
        if slugify(&automation.id) != automation.id {
            return Err(registry_error(RegistryError::BadId(automation.id)));
        }
        if owner.is_some() && notifies_mail(&automation.steps) {
            return Err(AgentChangeError::Rejected(
                "your own automations notify `owner` or a Slack alias, not other addresses"
                    .to_owned(),
            ));
        }
        self.settings()?
            .set_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Automation,
                &automation.id,
                source,
            )
            .await
            .map_err(crate::settings_failure)?;
        self.schedule.refresh();
        let switches = self.engine.switches().await;
        self.engine
            .automations
            .get_for(owner, &automation.id)
            .map(|known| self.info(&known, &switches))
            .ok_or_else(|| AgentChangeError::Failed("the automation did not load".to_owned()))
    }

    async fn delete(&self, id: &str, owner: Option<Uuid>) -> Result<bool, AgentChangeError> {
        let Some(known) = self.engine.automations.get_for(owner, id) else {
            return Ok(false);
        };
        let wanted = if owner.is_some() {
            lighthouse_settings::Layer::Personal
        } else {
            lighthouse_settings::Layer::Tenant
        };
        if known.layer != wanted {
            return Err(registry_error(RegistryError::ReadOnly(id.to_owned())));
        }
        self.settings()?
            .delete_file(
                owner,
                lighthouse_audit::acting_user::current(),
                FileKind::Automation,
                id,
            )
            .await
            .map_err(crate::settings_failure)?;
        self.schedule.refresh();
        Ok(true)
    }

    async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<AutomationInfo>, AgentChangeError> {
        let Some(known) = self.engine.automations.get(id) else {
            return Ok(None);
        };
        if known.layer == lighthouse_settings::Layer::Personal {
            self.settings()?
                .switch_file(
                    lighthouse_audit::acting_user::current(),
                    lighthouse_audit::acting_user::current(),
                    FileKind::Automation,
                    id,
                    enabled,
                )
                .await
                .map_err(crate::settings_failure)?;
            self.schedule.refresh();
            return self.get(id).await.map_err(AgentChangeError::Failed);
        }
        self.engine
            .desk
            .set_automation_enabled(id, enabled)
            .await
            .map_err(|error| AgentChangeError::Failed(error.to_string()))?;
        self.get(id).await.map_err(AgentChangeError::Failed)
    }

    async fn run(&self, id: &str, payload: Value) -> Result<Uuid, TriggerError> {
        let known = self.known(id)?;
        let run = self
            .engine
            .start(known.automation, "manual", payload)
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(run.as_uuid())
    }

    async fn webhook(&self, id: &str, payload: Value) -> Result<Option<Uuid>, TriggerError> {
        let known = self.known(id)?;
        let AutomationTrigger::Webhook { filter } = &known.automation.trigger else {
            return Err(TriggerError::Refused(format!(
                "the automation `{id}` has no webhook trigger"
            )));
        };
        if !AutomationEngine::enabled(&self.engine.switches().await, &known) {
            return Err(TriggerError::Refused(format!(
                "the automation `{id}` is switched off"
            )));
        }
        if !matches(filter, &payload) {
            return Ok(None);
        }
        let run = self
            .engine
            .start(known.automation.clone(), "webhook", payload)
            .await
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        Ok(Some(run.as_uuid()))
    }

    async fn emit(&self, name: &str, payload: Value) -> Result<Vec<Uuid>, String> {
        Ok(self.engine.emit(name, payload).await)
    }

    async fn dry_run(&self, request: DryRunRequest) -> Result<AutomationDryRun, AgentChangeError> {
        let automation = match (&request.source, &request.id) {
            (Some(source), _) => Arc::new(
                Automation::parse(source)
                    .map_err(|error| AgentChangeError::Rejected(error.to_string()))?,
            ),
            (None, Some(id)) => {
                self.engine
                    .automations
                    .get(id)
                    .ok_or_else(|| {
                        AgentChangeError::Rejected(format!("unknown automation `{id}`"))
                    })?
                    .automation
            }
            (None, None) => {
                return Err(AgentChangeError::Rejected(
                    "name an `id` or send a `source`".to_owned(),
                ));
            }
        };
        let trigger_matches = match &automation.trigger {
            AutomationTrigger::Event { filter, .. } | AutomationTrigger::Webhook { filter } => {
                Some(matches(filter, &request.payload))
            }
            _ => None,
        };
        let mut steps = Vec::new();
        plan_steps(
            &automation.steps,
            "",
            &request.payload,
            &self.engine.runtime.policy,
            &mut steps,
        );
        Ok(AutomationDryRun {
            id: automation.id.clone(),
            trigger_matches,
            steps,
        })
    }

    async fn draft(&self, description: &str) -> Result<String, String> {
        let agents: Vec<String> = self
            .engine
            .agents
            .list()
            .iter()
            .map(|known| format!("{}: {}", known.agent.id, known.agent.name))
            .collect();
        let tools: Vec<String> = self
            .engine
            .agents
            .list()
            .iter()
            .flat_map(|known| {
                known
                    .agent
                    .tools
                    .iter()
                    .map(|grant| grant.name.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        self.engine
            .runtime
            .draft_automation(description, &agents, &tools)
            .await
    }
}
