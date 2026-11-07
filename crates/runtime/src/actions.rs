//! Actions the owner asks for in the apps, such as a reply or a comment.
//!
//! An action is a run with the agent ID `action:<source>` and one tool call.
//! A `read` or `draft` tool runs at once. Every other tool waits for the
//! owner's approval through the same flow as the proposals of agents.

use crate::{Runtime, step};
use lighthouse_audit::{AuditError, NewApproval, RunEnd, RunStatus};
use lighthouse_core::RunId;
use lighthouse_core::agent::ToolLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Prefix of the run agent ID of an action.
pub const ACTION_PREFIX: &str = "action:";

/// What became of an action.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionOutcome {
    /// It waits for the owner's approval.
    Proposed {
        /// The run.
        run: Uuid,
        /// The approval to decide.
        approval: Uuid,
    },
    /// It ran at once.
    Done {
        /// The run.
        run: Uuid,
        /// Whether the tool succeeded.
        ok: bool,
        /// Tool output.
        output: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingAction {
    tool: String,
    approval: Uuid,
}

impl Runtime {
    /// Carries out an action, or proposes it when the tool has external effect.
    pub async fn act(
        &self,
        agent_id: &str,
        tool: &str,
        input: Value,
        reason: &str,
    ) -> Result<ActionOutcome, AuditError> {
        let run = RunId::new();
        self.audit.start_run(run, agent_id, "owner").await?;
        if self.policy.level(tool) == ToolLevel::Write {
            let mut proposed = step("tool", tool, "proposed");
            proposed.detail = Some(input.to_string());
            self.audit.record_step(run, &proposed).await?;
            let approval = NewApproval {
                id: Uuid::now_v7(),
                tool: tool.to_owned(),
                input,
                quotes_external: false,
                reason: Some(reason.to_owned()),
                consulted: Vec::new(),
            };
            let pending = PendingAction {
                tool: tool.to_owned(),
                approval: approval.id,
            };
            let state = serde_json::to_value(&pending).unwrap_or(Value::Null);
            self.audit
                .pause_run(run, agent_id, &state, &[approval])
                .await?;
            return Ok(ActionOutcome::Proposed {
                run: run.as_uuid(),
                approval: pending.approval,
            });
        }
        let (ok, output) = self.automation_tool(run, tool, input).await?;
        self.end_action(run, ok, &output).await?;
        Ok(ActionOutcome::Done {
            run: run.as_uuid(),
            ok,
            output,
        })
    }

    async fn end_action(&self, run: RunId, ok: bool, output: &str) -> Result<(), AuditError> {
        let reason = format!("the tool failed: {output}");
        self.audit
            .finish_run(
                run,
                RunEnd {
                    status: if ok {
                        RunStatus::Succeeded
                    } else {
                        RunStatus::Failed
                    },
                    reason: (!ok).then_some(reason.as_str()),
                    report: Some(output),
                },
            )
            .await
    }

    /// Continues an action after the decision on it.
    pub async fn resume_action(&self, run: RunId, state: Value) -> Result<(), AuditError> {
        let Ok(pending) = serde_json::from_value::<PendingAction>(state) else {
            return self
                .end_action(run, false, "the saved action cannot be read")
                .await;
        };
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
                self.end_action(run, ok, &output).await
            }
            None => {
                self.audit
                    .record_step(run, &step("approval", &pending.tool, "denied"))
                    .await?;
                let report = format!(
                    "The owner did not approve {}. It did not happen.",
                    pending.tool
                );
                self.audit
                    .finish_run(
                        run,
                        RunEnd {
                            status: RunStatus::Succeeded,
                            reason: None,
                            report: Some(&report),
                        },
                    )
                    .await
            }
        }
    }
}
