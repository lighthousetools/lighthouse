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

/// The unit of alerts that count failures; repeats add up.
const FAILURES: &str = lighthouse_desk::FAILURES;

/// Turns a run that did not succeed into an alert.
pub fn classify(record: &RunRecord) -> Option<NewAlert> {
    let reason = record.reason.clone().unwrap_or_default();
    let agent = &record.agent_id;
    let (value, unit) = if record.status == "limit_exceeded" && reason.starts_with("cost limit") {
        (Some(record.cost_eur), "EUR")
    } else {
        (Some(1.0), FAILURES)
    };
    let alert = |kind: &str, key: String, severity, title: String| NewAlert {
        value,
        unit: Some(unit.to_owned()),
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
    zone: chrono_tz::Tz,
    weather: Option<lighthouse_config::WeatherConfig>,
    settings: Option<lighthouse_settings::TenantSettings>,
    people: Option<Arc<dyn lighthouse_runtime::People>>,
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
    /// The owner's time zone, for the months of the budget.
    pub zone: chrono_tz::Tz,
    /// Where the owner is, for the weather in briefs.
    pub weather: Option<lighthouse_config::WeatherConfig>,
    /// Settings of the tenant and its members. Without them, the values above
    /// apply to everyone.
    pub settings: Option<lighthouse_settings::TenantSettings>,
    /// Finds the person pushes to `owner` go to: the one a run works for, or
    /// the tenant's first owner for what concerns the whole tenant.
    pub people: Option<Arc<dyn lighthouse_runtime::People>>,
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
            zone: parts.zone,
            weather: parts.weather,
            settings: parts.settings,
            people: parts.people,
        }
    }

    /// What to push about the work of a user, or of the tenant for `None`.
    fn pushes_for(&self, user: Option<Uuid>) -> Pushes {
        self.settings
            .as_ref()
            .and_then(|settings| {
                let effective = settings.effective(user);
                let (value, _) = effective.values.get(lighthouse_settings::NOTIFICATIONS)?;
                let config: NotificationsConfig = serde_json::from_value(value.clone()).ok()?;
                Pushes::from_config(&config).ok()
            })
            .unwrap_or_else(|| self.pushes.clone())
    }

    /// Where a user is, for the weather in their briefs.
    fn weather_for(&self, user: Option<Uuid>) -> Option<lighthouse_config::WeatherConfig> {
        let Some(settings) = &self.settings else {
            return self.weather.clone();
        };
        let effective = settings.effective(user);
        let (value, _) = effective.values.get(lighthouse_settings::WEATHER)?;
        let mut place: lighthouse_config::WeatherConfig =
            serde_json::from_value(value.clone()).ok()?;
        // The weather service stays what the instance configured.
        place.base_url = self.weather.as_ref().and_then(|own| own.base_url.clone());
        Some(place)
    }

    /// Warns when this month's model costs reach the share of the budget the
    /// owner set, and again critically when the budget is used up.
    pub async fn check_budget(&self) -> anyhow::Result<()> {
        let budget: lighthouse_api::Budget = self
            .desk
            .setting(lighthouse_api::BUDGET)
            .await?
            .unwrap_or_default();
        let Some(limit) = budget.monthly_eur.filter(|limit| *limit > 0.0) else {
            return Ok(());
        };
        // Budget months are the same for all members: the tenant's zone.
        let zone = self
            .settings
            .as_ref()
            .and_then(|settings| {
                settings
                    .effective(None)
                    .values
                    .get(lighthouse_settings::TIMEZONE)
                    .and_then(|(zone, _)| zone.as_str()?.parse().ok())
            })
            .unwrap_or(self.zone);
        let start = lighthouse_api::month_start(zone, Utc::now());
        let spent = self.audit.cost_since(start).await?;
        let used = spent / limit * 100.0;
        let key = format!("budget:{}", start.format("%Y-%m"));
        if used < f64::from(budget.alert_at_percent) {
            return Ok(());
        }
        let severity = if used >= 100.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        self.raise_alert(NewAlert {
            value: Some(spent),
            unit: Some("EUR".to_owned()),
            kind: "budget".to_owned(),
            key,
            severity,
            title: format!("{used:.0}% of this month's budget of {limit:.2} EUR is used"),
            detail: Some(format!(
                "Spent {spent:.2} EUR since {}",
                start.format("%Y-%m-%d")
            )),
            source: None,
        })
        .await
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

    async fn push(&self, pushes: &Pushes, user: Option<Uuid>, subject: &str, markdown: &str) {
        let subject = format!("{SUBJECT_PREFIX}: {subject}");
        let person = match &self.people {
            Some(people) => people.person(user).await,
            None => None,
        };
        for target in &pushes.targets {
            let target = match (target, &person) {
                (Delivery::Owner, Some(person)) if !person.email.is_empty() => {
                    &Delivery::Mail(person.email.clone())
                }
                _ => target,
            };
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
        let pushes = self.pushes_for(None);
        if notable && alert.severity >= pushes.alerts {
            let mut text = format!("**{}**", alert.title);
            if let Some(detail) = &alert.detail {
                text.push_str(&format!("\n\n{detail}"));
            }
            self.push(&pushes, None, &alert.title, &text).await;
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
        let user = self.audit.run_user(run).await?;
        let pushes = self.pushes_for(user);
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
            if pushes.decisions && !tools.is_empty() {
                let text = format!(
                    "{name} waits for your decision on {}: {}.",
                    if tools.len() == 1 {
                        "one proposal".to_owned()
                    } else {
                        format!("{} proposals", tools.len())
                    },
                    tools.join(", ")
                );
                self.push(&pushes, user, &format!("{name} needs a decision"), &text)
                    .await;
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
            if let Some(mut document) = self.desk.run_document(run).await? {
                if let (Some(place), true) = (
                    &self.weather_for(user),
                    document.kind == lighthouse_desk::MORNING_BRIEF
                        && document.content.weather.is_none(),
                ) && let Some(weather) = crate::weather::today(place).await
                    && let Some(updated) = self
                        .desk
                        .set_document_weather(document.id, &weather)
                        .await?
                {
                    document = updated;
                }
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
            if pushes.done {
                self.push(
                    &pushes,
                    user,
                    &format!("{name} is done"),
                    &format!("{name} finished its run."),
                )
                .await;
            }
        } else if let Some(alert) = classify(&record) {
            // A person's own agent failing is their concern, not the tenant's.
            let own = agent == crate::assistant::ASSISTANT_ID
                || self
                    .agents
                    .get_for(user, &agent)
                    .is_some_and(|known| known.layer == lighthouse_settings::Layer::Personal);
            if own {
                self.push(
                    &pushes,
                    user,
                    &alert.title,
                    alert.detail.as_deref().unwrap_or_default(),
                )
                .await;
            } else {
                self.raise_alert(alert).await?;
            }
        }
        self.check_budget().await?;
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
            let offline = (Utc::now() - node.last_seen_at).num_minutes();
            self.raise_alert(NewAlert {
                value: Some(offline as f64),
                unit: Some("minutes".to_owned()),
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
        let audit = AuditLog::new(store.clone(), TenantId::DEFAULT);
        let desk = Desk::new(store.clone(), TenantId::DEFAULT);
        let sessions = Sessions::new(store.clone(), TenantId::DEFAULT);
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
            zone: chrono_tz::UTC,
            weather: None,
            settings: None,
            people: None,
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

    struct Addresses;

    #[async_trait]
    impl lighthouse_runtime::People for Addresses {
        async fn person(&self, user: Option<Uuid>) -> Option<lighthouse_runtime::Person> {
            Some(lighthouse_runtime::Person {
                context: String::new(),
                email: user
                    .map_or("first@example.com", |_| "member@example.com")
                    .to_owned(),
                zone: chrono_tz::UTC,
            })
        }
    }

    #[derive(Default)]
    struct Targets(Mutex<Vec<String>>);

    #[async_trait]
    impl Notifier for Targets {
        async fn deliver(&self, target: &Delivery, _: &str, _: &str) -> Result<(), NotifyError> {
            self.0.lock().unwrap().push(format!("{target:?}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn owner_pushes_reach_the_member_a_run_belongs_to() {
        let mut fixture = fixture(loud()).await;
        let sam = Uuid::now_v7();
        lighthouse_store::sqlx::query(
            "INSERT INTO users (id, email, name) VALUES ($1, 'sam@example.com', 'Sam')",
        )
        .bind(sam)
        .execute(fixture.store.pool())
        .await
        .unwrap();
        let targets = Arc::new(Targets::default());
        fixture.outcomes.notifier = targets.clone();
        fixture.outcomes.people = Some(Arc::new(Addresses));
        let as_sam = fixture.audit.for_user(Some(sam));
        let run = finished(&as_sam, "brief", RunStatus::Succeeded, "").await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert_eq!(
            targets.0.lock().unwrap().as_slice(),
            ["Mail(\"member@example.com\")"]
        );
        let reason = "connector `linear` is not authorized";
        let run = finished(&fixture.audit, "brief", RunStatus::Failed, reason).await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert_eq!(
            targets.0.lock().unwrap().last().unwrap(),
            "Mail(\"first@example.com\")",
            "alerts concern the whole tenant"
        );
    }

    #[tokio::test]
    async fn pushes_and_places_follow_the_person_a_run_works_for() {
        let mut fixture = fixture(loud()).await;
        let sam = Uuid::now_v7();
        lighthouse_store::sqlx::query(
            "INSERT INTO users (id, email, name) VALUES ($1, 'sam@example.com', 'Sam')",
        )
        .bind(sam)
        .execute(fixture.store.pool())
        .await
        .unwrap();
        let settings = lighthouse_settings::TenantSettings::open(
            fixture.store.clone(),
            TenantId::DEFAULT,
            Arc::default(),
            lighthouse_settings::Options::default(),
        )
        .await
        .unwrap();
        settings
            .set_value(
                None,
                None,
                lighthouse_settings::NOTIFICATIONS,
                &json!({"deliver": ["owner"], "done": false}),
            )
            .await
            .unwrap();
        settings
            .set_value(
                Some(sam),
                Some(sam),
                lighthouse_settings::NOTIFICATIONS,
                &json!({"deliver": ["owner"], "done": true}),
            )
            .await
            .unwrap();
        settings
            .set_value(
                Some(sam),
                Some(sam),
                lighthouse_settings::WEATHER,
                &json!({"latitude": 1.0, "longitude": 2.0}),
            )
            .await
            .unwrap();
        fixture.outcomes.settings = Some(settings);
        let run = finished(&fixture.audit, "brief", RunStatus::Succeeded, "").await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert!(
            fixture.pushed.0.lock().unwrap().is_empty(),
            "the tenant wants none"
        );
        let as_sam = fixture.audit.for_user(Some(sam));
        let run = finished(&as_sam, "brief", RunStatus::Succeeded, "").await;
        fixture.outcomes.after_run(run).await.unwrap();
        assert_eq!(fixture.pushed.0.lock().unwrap().len(), 1, "Sam wants them");
        assert_eq!(
            fixture.outcomes.weather_for(Some(sam)).unwrap().longitude,
            2.0
        );
        assert!(fixture.outcomes.weather_for(None).is_none());
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
    async fn the_budget_warns_at_its_share_and_again_when_used_up() {
        let fixture = fixture(NotificationsConfig::default()).await;
        fixture.outcomes.check_budget().await.unwrap();
        let budget = lighthouse_api::Budget {
            monthly_eur: Some(1.0),
            alert_at_percent: 50,
        };
        fixture
            .desk
            .set_setting(lighthouse_api::BUDGET, &budget)
            .await
            .unwrap();
        let run = lighthouse_core::RunId::new();
        fixture
            .audit
            .start_run(run, "brief", "manual")
            .await
            .unwrap();
        let spend = |cost: f64| lighthouse_audit::NewStep {
            kind: "model".to_owned(),
            name: "text".to_owned(),
            status: "ok".to_owned(),
            cost_eur: cost,
            ..lighthouse_audit::NewStep::default()
        };
        fixture.audit.record_step(run, &spend(0.3)).await.unwrap();
        fixture.outcomes.check_budget().await.unwrap();
        assert!(
            fixture
                .desk
                .alerts(AlertView::Active, 10)
                .await
                .unwrap()
                .is_empty()
        );
        fixture.audit.record_step(run, &spend(0.3)).await.unwrap();
        fixture.outcomes.check_budget().await.unwrap();
        let alerts = fixture.desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(
            (alerts[0].kind.as_str(), alerts[0].severity),
            ("budget", Severity::Warning)
        );
        assert_eq!(alerts[0].unit.as_deref(), Some("EUR"));
        fixture.audit.record_step(run, &spend(0.5)).await.unwrap();
        fixture.outcomes.check_budget().await.unwrap();
        let alerts = fixture.desk.alerts(AlertView::Active, 10).await.unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Critical);
        assert!((alerts[0].value.unwrap() - 1.1).abs() < 1e-9);
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
