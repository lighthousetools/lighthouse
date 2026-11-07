//! Runs automations step by step.
//!
//! An automation run is a run in the log, with the agent ID `automation:<id>`.
//! Each step becomes a step of that run. A tool with external effect pauses the
//! run with an approval, like an agent does, and the run continues after the
//! owner's decision.

use crate::{Runtime, step};
use async_trait::async_trait;
use lighthouse_audit::{AuditError, NewApproval, RunEnd, RunStatus};
use lighthouse_core::RunId;
use lighthouse_core::agent::{Delivery, ToolLevel};
use lighthouse_core::automation::{
    Automation, AutomationStep, Condition, Op, program, render, render_value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

const SUCCEEDED: &str = "succeeded";
const FAILED: &str = "failed";

/// The end of an agent run that an automation started.
#[derive(Clone, Debug)]
pub struct AgentOutcome {
    /// The run.
    pub run_id: Uuid,
    /// Its status, such as `succeeded`, `failed` or `waiting`.
    pub status: String,
    /// The report, if it has one.
    pub report: Option<String>,
    /// Why it did not succeed.
    pub reason: Option<String>,
}

/// Runs agents for automations.
#[async_trait]
pub trait AgentSteps: Send + Sync {
    /// Runs an agent to its end, or to its first pause, with extra context.
    async fn run_agent(
        &self,
        agent_id: &str,
        trigger: &str,
        context: &str,
    ) -> Result<AgentOutcome, String>;
}

/// A tool call that waits for the owner.
#[derive(Debug, Serialize, Deserialize)]
struct PendingTool {
    approval: Uuid,
    tool: String,
}

/// Progress of an automation run, saved while it waits.
#[derive(Debug, Serialize, Deserialize)]
struct AutomationState {
    next: usize,
    event: Value,
    output: String,
    status: String,
    pending: Option<PendingTool>,
    /// An agent run of this automation that waits for a decision.
    #[serde(default)]
    waiting_run: Option<Uuid>,
}

impl AutomationState {
    fn values<'a>(&self, automation: &'a Automation) -> BTreeMap<&'a str, String> {
        BTreeMap::from([
            ("output", self.output.clone()),
            ("status", self.status.clone()),
            ("automation", automation.name.clone()),
        ])
    }

    fn set(&mut self, ok: bool, output: String) {
        self.status = if ok { SUCCEEDED } else { FAILED }.to_owned();
        self.output = output;
    }
}

fn holds(condition: &Condition, state: &AutomationState) -> bool {
    let output = state.output.to_lowercase();
    match condition {
        Condition::Contains(text) => output.contains(&text.to_lowercase()),
        Condition::NotContains(text) => !output.contains(&text.to_lowercase()),
        Condition::Status(status) => &state.status == status,
    }
}

fn condition_name(condition: &Condition) -> String {
    match condition {
        Condition::Contains(text) => format!("contains {text}"),
        Condition::NotContains(text) => format!("not contains {text}"),
        Condition::Status(status) => format!("status {status}"),
    }
}

impl Runtime {
    /// Runs a registered automation run from its first step.
    pub async fn run_automation(
        &self,
        run: RunId,
        automation: &Automation,
        event: Value,
        agents: &dyn AgentSteps,
    ) -> Result<(), AuditError> {
        let state = AutomationState {
            next: 0,
            event,
            output: String::new(),
            status: SUCCEEDED.to_owned(),
            pending: None,
            waiting_run: None,
        };
        self.drive_automation(run, automation, state, agents).await
    }

    /// Continues an automation run after the decision on its tool call.
    pub async fn resume_automation(
        &self,
        run: RunId,
        automation: &Automation,
        saved: Value,
        agents: &dyn AgentSteps,
    ) -> Result<(), AuditError> {
        let mut state: AutomationState = match serde_json::from_value(saved) {
            Ok(state) => state,
            Err(error) => {
                let reason = format!("cannot read the saved state: {error}");
                return self
                    .end_automation(run, RunStatus::Failed, Some(&reason), None)
                    .await;
            }
        };
        if let Some(child) = state.waiting_run {
            let detail = self.audit.run_detail(child).await?;
            match detail.map(|detail| detail.run) {
                Some(record) if matches!(record.status.as_str(), "running" | "waiting") => {
                    let trigger = automation.run_agent_id();
                    let saved = serde_json::to_value(&state).unwrap_or(Value::Null);
                    return self.audit.pause_run(run, &trigger, &saved, &[]).await;
                }
                Some(record) => {
                    let ok = record.status == SUCCEEDED;
                    let mut finished =
                        step("agent", &record.agent_id, if ok { "ok" } else { "error" });
                    finished.detail = Some(format!("run={child} status={}", record.status));
                    self.audit.record_step(run, &finished).await?;
                    state.set(ok, record.report.or(record.reason).unwrap_or_default());
                }
                None => state.set(false, format!("the agent run {child} is gone")),
            }
            state.waiting_run = None;
            state.next += 1;
        }
        if let Some(pending) = state.pending.take() {
            let approval = self
                .audit
                .run_approvals(run.as_uuid())
                .await?
                .into_iter()
                .find(|approval| approval.id == pending.approval);
            match approval.filter(|approval| approval.status == "approved") {
                Some(approval) => {
                    let (ok, output) = self
                        .automation_tool(run, &pending.tool, approval.input)
                        .await?;
                    state.set(ok, output);
                    state.next += 1;
                }
                None => {
                    let status = "denied";
                    self.audit
                        .record_step(run, &step("approval", &pending.tool, status))
                        .await?;
                    let report = format!(
                        "Stopped: the owner did not approve {}. It did not happen.",
                        pending.tool
                    );
                    return self
                        .end_automation(run, RunStatus::Succeeded, None, Some(&report))
                        .await;
                }
            }
        }
        self.drive_automation(run, automation, state, agents).await
    }

    async fn end_automation(
        &self,
        run: RunId,
        status: RunStatus,
        reason: Option<&str>,
        report: Option<&str>,
    ) -> Result<(), AuditError> {
        self.audit
            .finish_run(
                run,
                RunEnd {
                    status,
                    reason,
                    report,
                },
            )
            .await
    }

    pub(crate) async fn automation_tool(
        &self,
        run: RunId,
        tool: &str,
        input: Value,
    ) -> Result<(bool, String), AuditError> {
        let mut record = step("tool", tool, "ok");
        record.detail = Some(input.to_string());
        let names = [tool.to_owned()];
        let result = match self.tools.open(&names, run).await {
            Ok(host) => host.call_tool(tool, input).await,
            Err(error) => Err(error),
        };
        let (ok, output) = match result {
            Ok(output) => (!output.is_error, output.text),
            Err(error) => (false, error.to_string()),
        };
        if !ok {
            record.status = "error".to_owned();
        }
        self.audit.record_step(run, &record).await?;
        Ok((ok, output))
    }

    async fn drive_automation(
        &self,
        run: RunId,
        automation: &Automation,
        mut state: AutomationState,
        agents: &dyn AgentSteps,
    ) -> Result<(), AuditError> {
        let trigger = automation.run_agent_id();
        let ops = program(&automation.steps);
        while let Some(op) = ops.get(state.next) {
            let current = match op {
                Op::Jump(target) => {
                    state.next = *target;
                    continue;
                }
                Op::JumpUnless(condition, target) => {
                    let name = condition_name(condition);
                    let holds = holds(condition, &state);
                    let status = if holds { "ok" } else { "else" };
                    self.audit
                        .record_step(run, &step("condition", &name, status))
                        .await?;
                    state.next = if holds { state.next + 1 } else { *target };
                    continue;
                }
                Op::Step(step) => *step,
            };
            let values = state.values(automation);
            match current {
                AutomationStep::Agent(agent) => {
                    let context = format!(
                        "This run is a step of the automation \"{}\".\nEvent: {}\nOutput of the previous step:\n{}",
                        automation.name, state.event, state.output
                    );
                    let mut record = step("agent", agent, "ok");
                    match agents.run_agent(agent, &trigger, &context).await {
                        Ok(outcome) if outcome.status == "waiting" => {
                            record.status = "waiting".to_owned();
                            record.detail = Some(format!("run={} status=waiting", outcome.run_id));
                            self.audit.record_step(run, &record).await?;
                            state.waiting_run = Some(outcome.run_id);
                            let saved = serde_json::to_value(&state).unwrap_or(Value::Null);
                            return self.audit.pause_run(run, &trigger, &saved, &[]).await;
                        }
                        Ok(outcome) => {
                            let ok = outcome.status == SUCCEEDED;
                            record.detail =
                                Some(format!("run={} status={}", outcome.run_id, outcome.status));
                            if !ok {
                                record.status = "error".to_owned();
                            }
                            let output = outcome.report.or(outcome.reason).unwrap_or_default();
                            state.set(ok, output);
                        }
                        Err(error) => {
                            record.status = "error".to_owned();
                            record.detail = Some(error.clone());
                            state.set(false, error);
                        }
                    }
                    self.audit.record_step(run, &record).await?;
                }
                AutomationStep::Tool { name, input } => {
                    let input = render_value(input, &values, &state.event);
                    if self.policy.level(name) == ToolLevel::Write {
                        let mut proposed = step("tool", name, "proposed");
                        proposed.detail = Some(input.to_string());
                        self.audit.record_step(run, &proposed).await?;
                        let approval = NewApproval {
                            id: Uuid::now_v7(),
                            tool: name.clone(),
                            input,
                            quotes_external: true,
                            reason: Some(format!(
                                "Step {} of the automation \"{}\"",
                                state.next + 1,
                                automation.name
                            )),
                            consulted: Vec::new(),
                        };
                        state.pending = Some(PendingTool {
                            approval: approval.id,
                            tool: name.clone(),
                        });
                        let saved = serde_json::to_value(&state).unwrap_or(Value::Null);
                        return self
                            .audit
                            .pause_run(run, &trigger, &saved, &[approval])
                            .await;
                    }
                    let (ok, output) = self.automation_tool(run, name, input).await?;
                    state.set(ok, output);
                }
                AutomationStep::Notify {
                    to,
                    subject,
                    message,
                } => {
                    let target = match to {
                        Delivery::Owner => Delivery::Mail(self.owner_email.clone()),
                        other => other.clone(),
                    };
                    let subject = subject.as_deref().map_or_else(
                        || automation.name.clone(),
                        |text| render(text, &values, &state.event),
                    );
                    let text = render(message, &values, &state.event);
                    let name = match &target {
                        Delivery::Mail(address) => format!("mail:{address}"),
                        Delivery::Slack(alias) => format!("slack:{alias}"),
                        Delivery::Owner => "owner".to_owned(),
                    };
                    let mut record = step("delivery", &name, "ok");
                    match self.notifier.deliver(&target, &subject, &text).await {
                        Ok(()) => state.status = SUCCEEDED.to_owned(),
                        Err(error) => {
                            record.status = "error".to_owned();
                            record.detail = Some(error.to_string());
                            state.status = FAILED.to_owned();
                        }
                    }
                    self.audit.record_step(run, &record).await?;
                }
                AutomationStep::Branch { .. } => {}
                AutomationStep::If(condition) => {
                    let name = condition_name(condition);
                    if !holds(condition, &state) {
                        self.audit
                            .record_step(run, &step("condition", &name, "skipped"))
                            .await?;
                        let report = format!(
                            "Stopped at step {}: the condition `{name}` did not hold.",
                            state.next + 1
                        );
                        return self
                            .end_automation(run, RunStatus::Succeeded, None, Some(&report))
                            .await;
                    }
                    self.audit
                        .record_step(run, &step("condition", &name, "ok"))
                        .await?;
                }
            }
            state.next += 1;
        }
        let report = if state.output.is_empty() {
            "Done.".to_owned()
        } else {
            state.output.clone()
        };
        if state.status == FAILED {
            let reason = format!("the last step failed: {report}");
            return self
                .end_automation(run, RunStatus::Failed, Some(&reason), Some(&report))
                .await;
        }
        self.end_automation(run, RunStatus::Succeeded, None, Some(&report))
            .await
    }
}
