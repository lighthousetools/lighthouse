//! What happens after a run: live events, alerts and pushes to the owner.
//!
//! The apps always get an event. Pushes through the delivery channels follow
//! `[notifications]`: a run that waits for a decision ("ask before"), a finished
//! run ("done") and new alerts from a chosen severity on.

use anyhow::Context;
use chrono::{Duration, Utc};
use lighthouse_audit::{AuditLog, RunRecord};
use lighthouse_config::NotificationsConfig;
use lighthouse_core::agent::Delivery;
use lighthouse_desk::{Desk, NewAlert, Severity, SourceLink};
use lighthouse_notify::{EventBus, Notifier};
use lighthouse_runtime::AgentRegistry;
use lighthouse_sessions::Sessions;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

/// A desktop agent counts as offline after this long without a report.
const OFFLINE_AFTER: Duration = Duration::minutes(15);
const SUBJECT_PREFIX: &str = "Lighthouse";

/// What to push to the owner.
#[derive(Clone, Debug)]
pub struct Pushes {
    targets: Vec<Delivery>,
    decisions: bool,
    done: bool,
    alerts: Severity,
}

impl Pushes {
    /// Reads the settings.
    pub fn from_config(config: &NotificationsConfig) -> anyhow::Result<Self> {
        let targets = config
            .deliver
            .iter()
            .map(|target| target.parse::<Delivery>())
            .collect::<Result<_, _>>()
            .context("invalid notifications.deliver")?;
        Ok(Self {
            targets,
            decisions: config.decisions,
            done: config.done,
            alerts: config
                .alerts
                .parse()
                .map_err(|error: String| anyhow::anyhow!(error))?,
        })
    }
}

fn run_link(record: &RunRecord) -> SourceLink {
    SourceLink {
        kind: "run".to_owned(),
        reference: record.id.to_string(),
        label: Some(record.agent_id.clone()),
        url: None,
    }
}

fn quoted(text: &str, after: &str) -> Option<String> {
    let (_, rest) = text.split_once(after)?;
    let (name, _) = rest.split_once('`')?;
    Some(name.to_owned())
}

/// Turns a run that did not succeed into an alert.
pub fn classify(record: &RunRecord) -> Option<NewAlert> {
    let reason = record.reason.clone().unwrap_or_default();
    let agent = &record.agent_id;
    let alert = |kind: &str, key: String, severity, title: String| NewAlert {
        kind: kind.to_owned(),
        key,
        severity,
        title,
        detail: Some(reason.clone()).filter(|text| !text.is_empty()),
        source: Some(run_link(record)),
    };
    match record.status.as_str() {
        "failed" => {
            if let Some(connector) =
                quoted(&reason, "connector `").filter(|_| reason.contains("is not authorized"))
            {
                return Some(alert(
                    "connector",
                    format!("connector:{connector}"),
                    Severity::Critical,
                    format!("The connector {connector} needs a new authorization"),
                ));
            }
            if let Some(connector) = quoted(&reason, "cannot connect to `") {
                return Some(alert(
                    "connector",
                    format!("connector:{connector}"),
                    Severity::Warning,
                    format!("The connector {connector} cannot be reached"),
                ));
            }
            Some(alert(
                "run_failed",
                format!("run:{agent}"),
                Severity::Warning,
                format!("The agent {agent} failed"),
            ))
        }
        "limit_exceeded" if reason.starts_with("cost limit") => Some(alert(
            "cost_limit",
            format!("cost:{agent}"),
            Severity::Warning,
            format!("The agent {agent} hit its cost limit"),
        )),
        "limit_exceeded" | "rejected" => Some(alert(
            "run_failed",
            format!("run:{agent}"),
            Severity::Warning,
            format!("The agent {agent} stopped: {reason}"),
        )),
        _ => None,
    }
}

/// Reacts to the ends of runs and to the state of desktop agents.
pub struct Outcomes {
    audit: AuditLog,
    desk: Desk,
    sessions: Sessions,
    events: EventBus,
    notifier: Arc<dyn Notifier>,
    pushes: Pushes,
    agents: Arc<AgentRegistry>,
}

impl std::fmt::Debug for Outcomes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outcomes")
            .field("pushes", &self.pushes)
            .finish_non_exhaustive()
    }
}

/// Everything the outcomes need.
pub struct OutcomeParts {
    /// The audit log.
    pub audit: AuditLog,
    /// Alerts.
    pub desk: Desk,
    /// Desktop agents.
    pub sessions: Sessions,
    /// Live events.
    pub events: EventBus,
    /// Delivery channels.
    pub notifier: Arc<dyn Notifier>,
    /// What to push.
    pub pushes: Pushes,
    /// Agents, for names and connectors.
    pub agents: Arc<AgentRegistry>,
}

impl Outcomes {
    /// Creates the outcomes.
    pub fn new(parts: OutcomeParts) -> Self {
        Self {
            audit: parts.audit,
            desk: parts.desk,
            sessions: parts.sessions,
            events: parts.events,
            notifier: parts.notifier,
            pushes: parts.pushes,
            agents: parts.agents,
        }
    }

    fn agent_name(&self, id: &str) -> String {
        self.agents
            .get(id)
            .map_or_else(|| id.to_owned(), |known| known.agent.name.clone())
    }

    fn connectors_of(&self, id: &str) -> BTreeSet<String> {
        self.agents
            .get(id)
            .map(|known| {
                known
                    .agent
                    .tools
                    .iter()
                    .filter_map(|grant| grant.name.split_once('.'))
                    .map(|(connector, _)| connector.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn push(&self, subject: &str, markdown: &str) {
        let subject = format!("{SUBJECT_PREFIX}: {subject}");
        for target in &self.pushes.targets {
            if let Err(error) = self.notifier.deliver(target, &subject, markdown).await {
                tracing::warn!(%error, "cannot push to the owner");
            }
        }
    }

    /// Raises an alert, announces it and pushes it from the configured severity on.
    pub async fn raise_alert(&self, alert: NewAlert) -> anyhow::Result<()> {
        let raised = self.desk.raise_alert(alert).await?;
        let alert = &raised.alert;
        self.events.emit(
            "alert.raised",
            json!({"id": alert.id, "kind": alert.kind, "severity": alert.severity,
                   "new": raised.is_new}),
        );
        let notable = raised.is_new || raised.escalated;
        if notable && alert.severity >= self.pushes.alerts {
            let mut text = format!("**{}**", alert.title);
            if let Some(detail) = &alert.detail {
                text.push_str(&format!("\n\n{detail}"));
            }
            self.push(&alert.title, &text).await;
        }
        Ok(())
    }

    /// Resolves the unresolved alert of a key and announces it.
    pub async fn resolve_alerts(&self, key: &str) -> anyhow::Result<()> {
        for alert in self.desk.resolve_alert_key(key).await? {
            self.events.emit("alerts.changed", json!({"id": alert.id}));
        }
        Ok(())
    }

    /// Handles the end or pause of a run and returns it.
    pub async fn after_run(&self, run: Uuid) -> anyhow::Result<Option<RunRecord>> {
        let Some(detail) = self.audit.run_detail(run).await? else {
            return Ok(None);
        };
        let record = detail.run;
        let agent = record.agent_id.clone();
        let name = self.agent_name(&agent);
        if record.status == "waiting" {
            let tools: Vec<String> = self
                .audit
                .run_approvals(run)
                .await?
                .into_iter()
                .filter(|approval| approval.status == "pending")
                .map(|approval| approval.tool)
                .collect();
            self.events.emit(
                "run.waiting",
                json!({"run_id": run, "agent_id": agent, "proposals": tools.len()}),
            );
            if self.pushes.decisions && !tools.is_empty() {
                let text = format!(
                    "{name} waits for your decision on {}: {}.",
                    if tools.len() == 1 {
                        "one proposal".to_owned()
                    } else {
                        format!("{} proposals", tools.len())
                    },
                    tools.join(", ")
                );
                self.push(&format!("{name} needs a decision"), &text).await;
            }
            return Ok(Some(record));
        }
        self.events.emit(
            "run.finished",
            json!({"run_id": run, "agent_id": agent, "status": record.status}),
        );
        let result = record.report.as_deref().or(record.reason.as_deref());
        if let Some(message) = self
            .desk
            .finish_answer(
                run,
                record.status == "succeeded",
                result.unwrap_or_default(),
            )
            .await?
        {
            self.events
                .emit("talk.changed", json!({"thread_id": message.thread_id}));
        }
        if let Some(task) = self
            .desk
            .finish_delegation(run, &record.status, result)
            .await?
        {
            self.events.emit("tasks.changed", json!({"id": task.id}));
        }
        if record.status == "succeeded" {
            if let Some(document) = self.desk.run_document(run).await? {
                self.events.emit(
                    "document.created",
                    json!({"id": document.id, "run_id": run, "kind": document.kind,
                           "period": document.period}),
                );
            }
            self.resolve_alerts(&format!("run:{agent}")).await?;
            self.resolve_alerts(&format!("cost:{agent}")).await?;
            for connector in self.connectors_of(&agent) {
                self.resolve_alerts(&format!("connector:{connector}"))
                    .await?;
            }
            if self.pushes.done {
                self.push(
                    &format!("{name} is done"),
                    &format!("{name} finished its run."),
                )
                .await;
            }
        } else if let Some(alert) = classify(&record) {
            self.raise_alert(alert).await?;
        }
        Ok(Some(record))
    }

    /// Raises an alert for desktop agents that stopped reporting, and resolves it
    /// when they are back. Paused agents are left alone.
    pub async fn check_nodes(&self) -> anyhow::Result<()> {
        let cutoff = Utc::now() - OFFLINE_AFTER;
        for node in self.sessions.nodes().await? {
            let key = format!("node:{}", node.id);
            if node.paused || node.last_seen_at > cutoff {
                self.resolve_alerts(&key).await?;
                continue;
            }
            self.raise_alert(NewAlert {
                kind: "node_offline".to_owned(),
                key,
                severity: Severity::Warning,
                title: format!("The desktop agent {} is offline", node.name),
                detail: Some(format!("Last report at {}", node.last_seen_at.to_rfc3339())),
                source: Some(SourceLink {
                    kind: "node".to_owned(),
                    reference: node.id.to_string(),
                    label: Some(node.name.clone()),
                    url: None,
                }),
            })
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lighthouse_audit::{NewApproval, RunEnd, RunStatus};
    use lighthouse_core::{RunId, TenantId};
    use lighthouse_desk::AlertView;
    use lighthouse_notify::NotifyError;
    use lighthouse_sessions::NodeReport;
    use lighthouse_store::testing::fresh_store;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl Notifier for Recorder {
        async fn deliver(
            &self,
            _: &Delivery,
            subject: &str,
            text: &str,
        ) -> Result<(), NotifyError> {
            self.0
                .lock()
                .unwrap()
                .push((subject.to_owned(), text.to_owned()));
            Err(NotifyError::NotConfigured("test".to_owned()))
        }
    }

    struct Fixture {
        outcomes: Outcomes,
        audit: AuditLog,
        desk: Desk,
        sessions: Sessions,
        pushed: Arc<Recorder>,
        events: EventBus,
        store: lighthouse_store::Store,
        _dir: tempfile::TempDir,
    }

    async fn fixture(config: NotificationsConfig) -> Fixture {
        let store = fresh_store().await;
        let audit = AuditLog::new(store.clone(), TenantId::SINGLE);
        let desk = Desk::new(store.clone(), TenantId::SINGLE);
        let sessions = Sessions::new(store.clone(), TenantId::SINGLE);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("brief.md"),
            "---\nname: Brief\nscope: org\ntrigger:\n  manual: true\ntools:\n  - linear.list_issues\n---\nGo.\n",
        )
        .unwrap();
        let agents = Arc::new(
            AgentRegistry::load(dir.path().to_path_buf(), dir.path().join("created")).unwrap(),
        );
        let pushed = Arc::new(Recorder::default());
        let events = EventBus::new();
        let outcomes = Outcomes::new(OutcomeParts {
            audit: audit.clone(),
            desk: desk.clone(),
            sessions: sessions.clone(),
            events: events.clone(),
            notifier: pushed.clone(),
            pushes: Pushes::from_config(&config).unwrap(),
            agents,
        });
        Fixture {
            outcomes,
            audit,
            desk,
            sessions,
            pushed,
            events,
            store,
            _dir: dir,
        }
    }

    fn loud() -> NotificationsConfig {
        NotificationsConfig {
            deliver: vec!["owner".to_owned()],
            decisions: true,
            done: true,
            alerts: "warning".to_owned(),
        }
    }

    async fn finished(audit: &AuditLog, agent: &str, status: RunStatus, reason: &str) -> Uuid {
        let run = RunId::new();
        audit.start_run(run, agent, "cron").await.unwrap();
        let end = RunEnd {
            status,
            reason: Some(reason).filter(|text| !text.is_empty()),
            report: None,
        };
        audit.finish_run(run, end).await.unwrap();
        run.as_uuid()
    }

    fn record(status: &str, reason: &str) -> RunRecord {
        RunRecord {
            id: Uuid::nil(),
            agent_id: "brief".to_owned(),
            trigger: "cron".to_owned(),
            status: status.to_owned(),
            reason: Some(reason.to_owned()),
            report: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_eur: 0.0,
            activity: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    #[test]
    fn failures_map_to_alert_kinds() {
        let cases = [
            (
                "failed",
                "connector `linear` is not authorized: token expired",
                "connector:linear",
                Severity::Critical,
            ),
            (
                "failed",
                "cannot connect to `github`: refused",
                "connector:github",
                Severity::Warning,
            ),
            ("failed", "model error", "run:brief", Severity::Warning),
            (
                "limit_exceeded",
                "cost limit of 0.5 EUR exceeded",
                "cost:brief",
                Severity::Warning,
            ),
            (
                "limit_exceeded",
                "step limit of 3 exceeded",
                "run:brief",
                Severity::Warning,
            ),
            (
                "rejected",
                "would have needed an approval",
                "run:brief",
                Severity::Warning,
            ),
        ];
        for (status, reason, key, severity) in cases {
            let alert = classify(&record(status, reason)).unwrap();
            assert_eq!(
                (alert.key.as_str(), alert.severity),
                (key, severity),
                "{reason}"
            );
        }
        assert!(classify(&record("succeeded", "")).is_none());
        assert!(
            Pushes::from_config(&NotificationsConfig {
                deliver: vec!["nowhere".to_owned()],
                ..NotificationsConfig::default()
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_failure_raises_once_pushes_and_a_success_resolves() {
        let fixture = fixture(loud()).await;
        let mut events = fixture.events.subscribe();
        let reason = "connector `linear` is not authorized: expired";
        let run = finished(&fixture.audit, "brief", RunStatus::Failed, reason).await;
        fixture.outcomes.after_run(run).await.unwrap();
        let run = finished(&fixture.audit, "brief", RunStatus::Failed, reason).await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert_eq!(events.recv().await.unwrap().name, "run.finished");
        assert_eq!(events.recv().await.unwrap().name, "alert.raised");
        let alerts = fixture.desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].count, 2);
        let pushed = fixture.pushed.0.lock().unwrap().clone();
        assert_eq!(pushed.len(), 1, "a repeat is not pushed again");
        assert!(pushed[0].0.contains("linear"));

        let run = finished(&fixture.audit, "brief", RunStatus::Succeeded, "").await;
        let record = fixture.outcomes.after_run(run).await.unwrap().unwrap();
        assert_eq!(record.status, "succeeded");
        assert!(
            fixture
                .desk
                .alerts(AlertView::Active, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let pushed = fixture.pushed.0.lock().unwrap().clone();
        assert_eq!(pushed.last().unwrap().0, "Lighthouse: Brief is done");
        assert!(
            fixture
                .outcomes
                .after_run(Uuid::nil())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_stored_document_is_announced() {
        let fixture = fixture(NotificationsConfig::default()).await;
        let mut events = fixture.events.subscribe();
        let run = finished(&fixture.audit, "brief", RunStatus::Succeeded, "").await;
        let report = "```lighthouse-document\n{\"kind\": \"morning_brief\", \"title\": \"B\", \"sections\": []}\n```";
        let content = lighthouse_desk::extract_document(report)
            .1
            .unwrap()
            .unwrap();
        fixture
            .desk
            .save_document(run, "brief", "2026-11-06", &content)
            .await
            .unwrap();
        fixture.outcomes.after_run(run).await.unwrap();
        assert_eq!(events.recv().await.unwrap().name, "run.finished");
        let event = events.recv().await.unwrap();
        assert_eq!(event.name, "document.created");
        assert_eq!(event.data["period"], "2026-11-06");
    }

    #[tokio::test]
    async fn a_waiting_run_asks_for_a_decision() {
        let fixture = fixture(loud()).await;
        let mut events = fixture.events.subscribe();
        let run = RunId::new();
        fixture
            .audit
            .start_run(run, "brief", "manual")
            .await
            .unwrap();
        let proposal = NewApproval {
            id: Uuid::now_v7(),
            tool: "mail.send".to_owned(),
            input: json!({}),
            quotes_external: false,
            reason: None,
            consulted: Vec::new(),
        };
        fixture
            .audit
            .pause_run(run, "brief", &json!({}), &[proposal])
            .await
            .unwrap();
        fixture.outcomes.after_run(run.as_uuid()).await.unwrap();
        let event = events.recv().await.unwrap();
        assert_eq!(event.name, "run.waiting");
        assert_eq!(event.data["proposals"], 1);
        let pushed = fixture.pushed.0.lock().unwrap().clone();
        assert_eq!(pushed[0].0, "Lighthouse: Brief needs a decision");
        assert!(pushed[0].1.contains("mail.send"));
    }

    #[tokio::test]
    async fn the_result_of_a_delegated_run_lands_on_its_task() {
        let fixture = fixture(NotificationsConfig::default()).await;
        let task = fixture
            .desk
            .create_task(
                lighthouse_desk::NewTask {
                    title: "Draft the offer".to_owned(),
                    ..lighthouse_desk::NewTask::default()
                },
                None,
            )
            .await
            .unwrap();
        let run = RunId::new();
        fixture.audit.start_run(run, "brief", "task").await.unwrap();
        fixture
            .desk
            .delegate_task(task.id, "brief", run.as_uuid())
            .await
            .unwrap();
        let end = RunEnd {
            status: RunStatus::Succeeded,
            reason: None,
            report: Some("The draft is ready"),
        };
        fixture.audit.finish_run(run, end).await.unwrap();
        let mut events = fixture.events.subscribe();
        fixture.outcomes.after_run(run.as_uuid()).await.unwrap();
        assert_eq!(events.recv().await.unwrap().name, "run.finished");
        assert_eq!(events.recv().await.unwrap().name, "tasks.changed");
        let task = fixture.desk.task(task.id).await.unwrap().unwrap();
        assert_eq!(task.result.as_deref(), Some("The draft is ready"));
        assert_eq!(task.result_status.as_deref(), Some("succeeded"));
    }

    #[tokio::test]
    async fn quiet_settings_push_nothing() {
        let fixture = fixture(NotificationsConfig::default()).await;
        let run = finished(&fixture.audit, "brief", RunStatus::Failed, "model error").await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert!(fixture.pushed.0.lock().unwrap().is_empty());
        assert_eq!(
            fixture
                .desk
                .alerts(AlertView::Active, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn offline_desktop_agents_raise_and_returning_ones_resolve() {
        let fixture = fixture(loud()).await;
        let node = Uuid::now_v7();
        fixture
            .sessions
            .report(
                node,
                &NodeReport {
                    name: "laptop".to_owned(),
                    sessions: Vec::new(),
                },
            )
            .await
            .unwrap();
        fixture.outcomes.check_nodes().await.unwrap();
        assert!(
            fixture
                .desk
                .alerts(AlertView::Active, 10)
                .await
                .unwrap()
                .is_empty()
        );
        lighthouse_store::sqlx::query("UPDATE nodes SET last_seen_at = now() - interval '1 hour'")
            .execute(fixture.store.pool())
            .await
            .unwrap();
        fixture.outcomes.check_nodes().await.unwrap();
        let alerts = fixture.desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(alerts[0].kind, "node_offline");
        fixture
            .sessions
            .report(
                node,
                &NodeReport {
                    name: "laptop".to_owned(),
                    sessions: Vec::new(),
                },
            )
            .await
            .unwrap();
        fixture.outcomes.check_nodes().await.unwrap();
        assert!(
            fixture
                .desk
                .alerts(AlertView::Active, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
