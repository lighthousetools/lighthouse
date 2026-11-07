//! Refreshes connected sources, passes their events on and carries out actions.

use crate::automations::AutomationEngine;
use crate::outcomes::Outcomes;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use lighthouse_api::{
    ActionError, ActionResult, ActionTool, CollectionTool, Connected, RefreshResult, SourceInfo,
};
use lighthouse_desk::{NewAlert, Severity, SourceLink};
use lighthouse_notify::EventBus;
use lighthouse_runtime::{ACTION_PREFIX, ActionOutcome, Runtime};
use lighthouse_sync::{SyncReport, SyncedItem, Syncer};
use serde_json::{Value, json};
use std::sync::Arc;

const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// The connected sources of the server.
pub struct ConnectedService {
    syncer: Arc<Syncer>,
    runtime: Arc<Runtime>,
    engine: Arc<AutomationEngine>,
    outcomes: Arc<Outcomes>,
    events: EventBus,
}

impl std::fmt::Debug for ConnectedService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedService").finish_non_exhaustive()
    }
}

impl ConnectedService {
    /// Creates the service.
    pub fn new(
        syncer: Arc<Syncer>,
        runtime: Arc<Runtime>,
        engine: Arc<AutomationEngine>,
        outcomes: Arc<Outcomes>,
        events: EventBus,
    ) -> Self {
        Self {
            syncer,
            runtime,
            engine,
            outcomes,
            events,
        }
    }

    async fn handle(&self, report: &SyncReport) -> anyhow::Result<()> {
        for event in &report.events {
            self.engine.emit(&event.name, event.payload.clone()).await;
        }
        for check in &report.checks {
            if check.failed && check.on_my_pull_request {
                let pull_request = check.pull_request.clone().unwrap_or_default();
                self.outcomes
                    .raise_alert(NewAlert {
                        kind: "check_failed".to_owned(),
                        key: check.key.clone(),
                        severity: Severity::Warning,
                        title: format!(
                            "The check {} failed on your pull request {pull_request}",
                            check.title
                        ),
                        detail: None,
                        source: check.url.clone().map(|url| SourceLink {
                            kind: "url".to_owned(),
                            reference: check.key.clone(),
                            label: Some(check.title.clone()),
                            url: Some(url),
                        }),
                    })
                    .await?;
            } else if !check.failed {
                self.outcomes.resolve_alerts(&check.key).await?;
            }
        }
        self.events
            .emit("connected.changed", json!({"source": report.source}));
        Ok(())
    }

    /// Refreshes a source and passes on what changed.
    pub async fn refresh_source(&self, name: &str) -> anyhow::Result<SyncReport> {
        let report = self.syncer.sync(name, Utc::now()).await?;
        for error in &report.errors {
            tracing::warn!(source = %name, %error, "refresh failed in part");
        }
        self.handle(&report).await?;
        Ok(report)
    }

    /// Refreshes every source whose interval has passed.
    pub async fn refresh_due(&self) -> anyhow::Result<()> {
        for name in self.syncer.due(Utc::now()).await? {
            self.refresh_source(&name).await?;
        }
        Ok(())
    }

    /// Refreshes due sources until the process ends.
    pub async fn run(self: Arc<Self>) {
        if self.syncer.sources().is_empty() {
            return;
        }
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;
            if let Err(error) = self.refresh_due().await {
                tracing::error!(%error, "refreshing connected sources failed");
            }
        }
    }

    /// Refreshes the source of an action run in the background, after it acted.
    pub fn refresh_after(self: &Arc<Self>, agent_id: &str) {
        let Some(source) = agent_id
            .strip_prefix(ACTION_PREFIX)
            .filter(|source| self.syncer.source(source).is_some())
        else {
            return;
        };
        let service = self.clone();
        let source = source.to_owned();
        tokio::spawn(async move {
            if let Err(error) = service.refresh_source(&source).await {
                tracing::warn!(%source, %error, "refresh after an action failed");
            }
        });
    }
}

#[async_trait]
impl Connected for ConnectedService {
    async fn sources(&self) -> Result<Vec<SourceInfo>, String> {
        let statuses = self
            .syncer
            .store()
            .statuses()
            .await
            .map_err(|error| error.to_string())?;
        let now = Utc::now();
        Ok(self
            .syncer
            .sources()
            .iter()
            .map(|source| {
                let status = statuses
                    .iter()
                    .find(|status| status.source == source.name)
                    .cloned();
                let limit = Duration::from_std(source.interval * 2).unwrap_or(Duration::MAX);
                let stale = status
                    .as_ref()
                    .and_then(|status| status.last_success_at)
                    .is_none_or(|time| now - time > limit);
                SourceInfo {
                    name: source.name.clone(),
                    connector: source.connector.clone(),
                    domain: source.domain.clone(),
                    service: source.service.clone(),
                    interval_seconds: source.interval.as_secs(),
                    actions: source.actions.keys().cloned().collect(),
                    action_tools: source
                        .actions
                        .iter()
                        .map(|(action, config)| ActionTool {
                            action: action.clone(),
                            tool: source.tool(&config.tool),
                            kind: config.kind.clone(),
                            input: config.input.clone(),
                        })
                        .collect(),
                    collection_tools: source
                        .collections
                        .iter()
                        .map(|(collection, config)| CollectionTool {
                            collection: collection.clone(),
                            tool: source.tool(&config.tool),
                            kind: config.kind.clone(),
                        })
                        .collect(),
                    status,
                    stale,
                }
            })
            .collect())
    }

    async fn refresh(&self, source: Option<&str>) -> Result<Vec<RefreshResult>, String> {
        let names: Vec<String> = match source {
            Some(name) if self.syncer.source(name).is_none() => return Ok(Vec::new()),
            Some(name) => vec![name.to_owned()],
            None => self
                .syncer
                .sources()
                .iter()
                .map(|source| source.name.clone())
                .collect(),
        };
        let mut results = Vec::new();
        for name in names {
            let report = self
                .refresh_source(&name)
                .await
                .map_err(|error| error.to_string())?;
            results.push(RefreshResult {
                source: name,
                items: report.items,
                events: report.events.len(),
                errors: report.errors,
            });
        }
        Ok(results)
    }

    async fn act(
        &self,
        source: &str,
        action: &str,
        item: Option<&SyncedItem>,
        input: Value,
    ) -> Result<ActionResult, ActionError> {
        let prepared = self
            .syncer
            .prepare_action(source, action, item, &input)
            .map_err(|error| ActionError::Rejected(error.to_string()))?;
        let about = item
            .and_then(|item| item.title.clone().or(Some(item.external_id.clone())))
            .map(|title| format!(" on \"{title}\""))
            .unwrap_or_default();
        let reason = format!("You asked for {action}{about} in {source}");
        let agent = format!("{ACTION_PREFIX}{source}");
        let outcome = self
            .runtime
            .act(&agent, &prepared.tool, prepared.input, &reason)
            .await
            .map_err(|error| ActionError::Failed(error.to_string()))?;
        Ok(match outcome {
            ActionOutcome::Proposed { run, approval } => {
                if let Err(error) = self.outcomes.after_run(run).await {
                    tracing::warn!(%error, "cannot announce the proposed action");
                }
                ActionResult {
                    status: "proposed".to_owned(),
                    run_id: run,
                    approval_id: Some(approval),
                    tool: prepared.tool,
                    output: None,
                }
            }
            ActionOutcome::Done { run, ok, output } => {
                if let Err(error) = self.outcomes.after_run(run).await {
                    tracing::warn!(%error, "cannot handle the end of an action");
                }
                if let Err(error) = self.refresh_source(source).await {
                    tracing::warn!(%source, %error, "refresh after an action failed");
                }
                ActionResult {
                    status: if ok { "done" } else { "failed" }.to_owned(),
                    run_id: run,
                    approval_id: None,
                    tool: prepared.tool,
                    output: Some(output),
                }
            }
        })
    }
}
