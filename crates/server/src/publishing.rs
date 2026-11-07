//! Publishes posts through a connector tool, always after the owner's approval.

use crate::outcomes::Outcomes;
use async_trait::async_trait;
use lighthouse_api::{ActionError, ActionResult, Publisher};
use lighthouse_audit::AuditLog;
use lighthouse_config::PublishingConfig;
use lighthouse_core::agent::ToolLevel;
use lighthouse_desk::{Desk, Post};
use lighthouse_runtime::{ACTION_PREFIX, ActionOutcome, Runtime};
use lighthouse_sync::mapping::render_value;
use lighthouse_sync::prune_empty;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Prefix of the run agent ID of a publishing action, followed by the post ID.
pub const POST_PREFIX: &str = "action:post:";

/// Publishes posts.
pub struct ServerPublisher {
    runtime: Arc<Runtime>,
    outcomes: Arc<Outcomes>,
    config: Option<PublishingConfig>,
}

impl std::fmt::Debug for ServerPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerPublisher").finish_non_exhaustive()
    }
}

impl ServerPublisher {
    /// Creates the publisher.
    pub fn new(
        runtime: Arc<Runtime>,
        outcomes: Arc<Outcomes>,
        config: Option<PublishingConfig>,
    ) -> Self {
        Self {
            runtime,
            outcomes,
            config,
        }
    }
}

fn label(post: &Post) -> String {
    post.title
        .clone()
        .or_else(|| {
            post.body
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect()
}

#[async_trait]
impl Publisher for ServerPublisher {
    async fn publish(&self, post: &Post) -> Result<ActionResult, ActionError> {
        let config = self.config.as_ref().ok_or_else(|| {
            ActionError::Rejected(
                "publishing is not set up: add [publishing] to the configuration".to_owned(),
            )
        })?;
        let tool = format!("{}.{}", config.connector, config.tool);
        if self.runtime.policy.level(&tool) != ToolLevel::Write {
            return Err(ActionError::Rejected(format!(
                "`{tool}` is a read or draft tool. Publishing needs a write tool, so it waits for approval"
            )));
        }
        let template = config
            .input
            .clone()
            .unwrap_or_else(|| json!({"text": "{{post.body}}"}));
        let context = json!({"post": post});
        let input = prune_empty(render_value(&template, &context));
        let reason = format!("You asked to publish the post \"{}\"", label(post));
        let agent = format!("{POST_PREFIX}{}", post.id);
        let outcome = self
            .runtime
            .act(&agent, &tool, input, &reason)
            .await
            .map_err(|error| ActionError::Failed(error.to_string()))?;
        let ActionOutcome::Proposed { run, approval } = outcome else {
            return Err(ActionError::Failed(
                "the publishing tool ran without approval".to_owned(),
            ));
        };
        if let Err(error) = self.outcomes.after_run(run).await {
            tracing::warn!(%error, "cannot announce the publishing request");
        }
        Ok(ActionResult {
            status: "proposed".to_owned(),
            run_id: run,
            approval_id: Some(approval),
            tool,
            output: None,
        })
    }
}

/// Records the end of a publishing run on its post: published if the owner
/// approved and the tool succeeded, back to the plan otherwise.
pub async fn finish_publishing(
    audit: &AuditLog,
    desk: &Desk,
    agent_id: &str,
    run: Uuid,
) -> anyhow::Result<()> {
    let Some(post) = agent_id
        .strip_prefix(POST_PREFIX)
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return Ok(());
    };
    let Some(detail) = audit.run_detail(run).await? else {
        return Ok(());
    };
    let approved = audit
        .run_approvals(run)
        .await?
        .iter()
        .any(|approval| approval.status == "approved");
    let published = approved && detail.run.status == "succeeded";
    let external_id = detail
        .run
        .report
        .as_deref()
        .and_then(|report| serde_json::from_str::<Value>(report).ok())
        .and_then(|value| {
            ["id", "urn", "url"]
                .iter()
                .find_map(|key| value[*key].as_str().map(str::to_owned))
        });
    desk.post_published(post, published, external_id.as_deref())
        .await?;
    Ok(())
}

/// Whether a run agent ID belongs to publishing.
pub fn is_publishing(agent_id: &str) -> bool {
    agent_id.starts_with(POST_PREFIX) && agent_id.len() > ACTION_PREFIX.len()
}
