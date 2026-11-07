//! Starts automations on schedules, events and webhooks, and serves them to the apps.

use crate::outcomes::Outcomes;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use lighthouse_api::{
    AgentChangeError, AutomationInfo, AutomationStepInfo, AutomationTriggerInfo, Automations,
    TriggerError,
};
use lighthouse_audit::{AuditError, RunRecord};
use lighthouse_core::RunId;
use lighthouse_core::agent::{ToolLevel, parse_cron};
use lighthouse_core::automation::{
    Automation, AutomationStep, AutomationTrigger, Condition, RUN_PREFIX, matches,
};
use lighthouse_desk::Desk;
use lighthouse_runtime::{
    AgentOutcome, AgentRegistry, AgentSteps, AutomationRegistry, LoadedAutomation, RegistryError,
    Runtime,
};
use lighthouse_scheduler::ScheduleHandle;
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
        }
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

    fn enabled(switches: &HashMap<String, bool>, automation: &Automation) -> bool {
        switches
            .get(&automation.id)
            .copied()
            .unwrap_or(automation.enabled)
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
        let Some(known) = self.automations.get(id) else {
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

    /// Starts a scheduled automation, given as `automation:<id>`.
    pub async fn start_scheduled(self: &Arc<Self>, scheduled: &str) {
        let id = scheduled.strip_prefix(RUN_PREFIX).unwrap_or(scheduled);
        let Some(known) = self.automations.get(id) else {
            return;
        };
        if !Self::enabled(&self.switches().await, &known.automation) {
            return;
        }
        if let Err(error) = self.start(known.automation, "cron", Value::Null).await {
            tracing::error!(automation = %id, %error, "cannot start the automation");
        }
    }

    /// Starts every enabled automation that waits for this event.
    pub async fn emit(self: &Arc<Self>, name: &str, payload: Value) -> Vec<Uuid> {
        let switches = self.switches().await;
        let mut runs = Vec::new();
        for known in self.automations.list() {
            let automation = known.automation;
            let AutomationTrigger::Event {
                name: wanted,
                filter,
            } = &automation.trigger
            else {
                continue;
            };
            if wanted != name
                || !Self::enabled(&switches, &automation)
                || !matches(filter, &payload)
            {
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
        self.emit(event, payload).await;
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

/// Serves the automations to the apps.
pub struct ServerAutomations {
    engine: Arc<AutomationEngine>,
    schedules: ScheduleHandle,
    zone: Tz,
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
    /// Serves the automations of an engine and keeps their schedules current.
    pub fn new(
        engine: Arc<AutomationEngine>,
        schedules: ScheduleHandle,
        zone: Tz,
    ) -> anyhow::Result<Self> {
        let automations = Self {
            engine,
            schedules,
            zone,
        };
        automations
            .refresh_schedules()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(automations)
    }

    fn refresh_schedules(&self) -> Result<(), AgentChangeError> {
        let entries: Vec<(String, String)> = self
            .engine
            .automations
            .list()
            .into_iter()
            .filter_map(|known| match &known.automation.trigger {
                AutomationTrigger::Cron(cron) => Some((known.automation.id.clone(), cron.clone())),
                _ => None,
            })
            .collect();
        self.schedules
            .update_automations(&entries)
            .map_err(|error| AgentChangeError::Failed(error.to_string()))
    }

    fn next_run(&self, expression: &str) -> Option<DateTime<Utc>> {
        let now = Utc::now().with_timezone(&self.zone);
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
        let enabled = AutomationEngine::enabled(switches, automation);
        AutomationInfo {
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

    async fn save(&self, source: &str) -> Result<AutomationInfo, AgentChangeError> {
        let automation = self
            .engine
            .automations
            .save(source)
            .map_err(registry_error)?;
        self.refresh_schedules()?;
        self.get(&automation.id)
            .await
            .map_err(AgentChangeError::Failed)?
            .ok_or_else(|| AgentChangeError::Failed("the automation did not load".to_owned()))
    }

    async fn delete(&self, id: &str) -> Result<bool, AgentChangeError> {
        let deleted = self.engine.automations.delete(id).map_err(registry_error)?;
        self.refresh_schedules()?;
        Ok(deleted)
    }

    async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<AutomationInfo>, AgentChangeError> {
        if self.engine.automations.get(id).is_none() {
            return Ok(None);
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
        if !AutomationEngine::enabled(&self.engine.switches().await, &known.automation) {
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
}
