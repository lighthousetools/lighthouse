//! Cron triggers and manual runs.
//!
//! The scheduler stores when each agent last fired. On every tick it compares
//! that with the latest scheduled time. Because of this, a run missed during a
//! restart is caught up exactly once, and only if it is at most one hour old.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use croner::Cron;
use lighthouse_core::TenantId;
use lighthouse_core::agent::{AgentDefinition, Trigger, parse_cron};
use lighthouse_core::automation::RUN_PREFIX;
use lighthouse_store::Store;
use lighthouse_store::sqlx;
use std::sync::{Arc, RwLock};

const CATCH_UP_LIMIT: Duration = Duration::hours(1);
const TICK: std::time::Duration = std::time::Duration::from_secs(15);

/// Errors from the scheduler.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// A database operation failed.
    #[error("scheduler storage error: {0}")]
    Database(#[from] sqlx::Error),
    /// A schedule is invalid.
    #[error("invalid schedule: {0}")]
    Schedule(String),
}

/// Starts agent runs.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Starts a run in the background.
    async fn start(&self, agent_id: &str, trigger: &str);
}

/// What to do with an agent at a given moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is due.
    Wait,
    /// A scheduled time passed; run now.
    Fire,
    /// A scheduled time passed too long ago; skip it.
    Skip,
}

/// Decides whether an agent is due.
pub fn decide(cron: &Cron, zone: Tz, last_fire: DateTime<Utc>, now: DateTime<Utc>) -> Decision {
    let Ok(latest) = cron.find_previous_occurrence(&now.with_timezone(&zone), true) else {
        return Decision::Wait;
    };
    let latest = latest.with_timezone(&Utc);
    if latest <= last_fire {
        Decision::Wait
    } else if now - latest <= CATCH_UP_LIMIT {
        Decision::Fire
    } else {
        Decision::Skip
    }
}

/// Fires agents on their schedules.
pub struct Scheduler {
    store: Store,
    tenant: TenantId,
    zone: Tz,
    agents: Schedules,
    runner: Arc<dyn AgentRunner>,
}

type Schedules = Arc<RwLock<Vec<(String, Cron)>>>;

fn schedules_of(agents: &[AgentDefinition]) -> Result<Vec<(String, Cron)>, SchedulerError> {
    let mut schedules = Vec::new();
    for agent in agents {
        if let Trigger::Cron(expression) = &agent.trigger {
            let cron = parse_cron(expression)
                .map_err(|error| SchedulerError::Schedule(error.to_string()))?;
            schedules.push((agent.id.clone(), cron));
        }
    }
    Ok(schedules)
}

/// Changes the schedules of a running scheduler.
#[derive(Clone, Debug)]
pub struct ScheduleHandle(Schedules);

impl ScheduleHandle {
    /// Replaces the schedules of all agents. Automations keep theirs.
    pub fn update(&self, agents: &[AgentDefinition]) -> Result<(), SchedulerError> {
        let agents = schedules_of(agents)?;
        let mut schedules = self.0.write().expect("schedules lock");
        schedules.retain(|(id, _)| id.starts_with(RUN_PREFIX));
        schedules.extend(agents);
        Ok(())
    }

    /// Replaces the schedules of all automations, given as ID and cron
    /// expression. They fire as `automation:<id>`. Agents keep theirs.
    pub fn update_automations(
        &self,
        automations: &[(String, String)],
    ) -> Result<(), SchedulerError> {
        let mut parsed = Vec::new();
        for (id, expression) in automations {
            let cron = parse_cron(expression)
                .map_err(|error| SchedulerError::Schedule(error.to_string()))?;
            parsed.push((format!("{RUN_PREFIX}{id}"), cron));
        }
        let mut schedules = self.0.write().expect("schedules lock");
        schedules.retain(|(id, _)| !id.starts_with(RUN_PREFIX));
        schedules.extend(parsed);
        Ok(())
    }
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("zone", &self.zone)
            .finish_non_exhaustive()
    }
}

impl Scheduler {
    /// Creates a scheduler for all agents with a cron trigger.
    pub fn new(
        store: Store,
        tenant: TenantId,
        zone: &str,
        agents: &[AgentDefinition],
        runner: Arc<dyn AgentRunner>,
    ) -> Result<Self, SchedulerError> {
        let zone = zone
            .parse()
            .map_err(|_| SchedulerError::Schedule(format!("unknown time zone `{zone}`")))?;
        Ok(Self {
            store,
            tenant,
            zone,
            agents: Arc::new(RwLock::new(schedules_of(agents)?)),
            runner,
        })
    }

    /// Returns a handle to change the schedules while the scheduler runs.
    pub fn handle(&self) -> ScheduleHandle {
        ScheduleHandle(self.agents.clone())
    }

    async fn last_fire(&self, agent: &str) -> Result<Option<DateTime<Utc>>, SchedulerError> {
        let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "SELECT last_fire_at FROM agent_schedules WHERE tenant_id = $1 AND agent_id = $2",
        )
        .bind(self.tenant.as_uuid())
        .bind(agent)
        .fetch_optional(self.store.pool())
        .await?;
        Ok(row.map(|(time,)| time))
    }

    async fn set_last_fire(&self, agent: &str, time: DateTime<Utc>) -> Result<(), SchedulerError> {
        sqlx::query(
            "INSERT INTO agent_schedules (tenant_id, agent_id, last_fire_at) VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, agent_id) DO UPDATE SET last_fire_at = $3",
        )
        .bind(self.tenant.as_uuid())
        .bind(agent)
        .bind(time)
        .execute(self.store.pool())
        .await?;
        Ok(())
    }

    /// Checks all agents once.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<(), SchedulerError> {
        let schedules = self.agents.read().expect("schedules lock").clone();
        for (agent, cron) in &schedules {
            let Some(last_fire) = self.last_fire(agent).await? else {
                self.set_last_fire(agent, now).await?;
                continue;
            };
            match decide(cron, self.zone, last_fire, now) {
                Decision::Wait => {}
                Decision::Skip => {
                    tracing::warn!(%agent, "skipping a run older than one hour");
                    self.set_last_fire(agent, now).await?;
                }
                Decision::Fire => {
                    // Saved first, so a crash cannot cause a double run.
                    self.set_last_fire(agent, now).await?;
                    self.runner.start(agent, "cron").await;
                }
            }
        }
        Ok(())
    }

    /// Runs until the task is dropped.
    pub async fn run(self) {
        loop {
            if let Err(error) = self.tick(Utc::now()).await {
                tracing::error!(%error, "scheduler tick failed");
            }
            tokio::time::sleep(TICK).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lighthouse_store::testing::fresh_store;
    use std::str::FromStr;
    use std::sync::Mutex;

    const BERLIN: Tz = chrono_tz::Europe::Berlin;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl AgentRunner for Recorder {
        async fn start(&self, agent_id: &str, trigger: &str) {
            self.0
                .lock()
                .unwrap()
                .push((agent_id.to_owned(), trigger.to_owned()));
        }
    }

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        // Friday, November 6, 2026. Berlin is one hour ahead of UTC.
        Utc.with_ymd_and_hms(2026, 11, 6, hour, minute, 0).unwrap()
    }

    fn daily() -> Cron {
        Cron::from_str("0 8 * * *").unwrap()
    }

    #[test]
    fn honors_the_time_zone() {
        assert_eq!(
            decide(&daily(), BERLIN, at(6, 59), at(7, 0)),
            Decision::Fire
        );
        assert_eq!(
            decide(&daily(), chrono_tz::UTC, at(6, 59), at(7, 0)),
            Decision::Wait
        );
    }

    #[test]
    fn fires_only_once() {
        assert_eq!(decide(&daily(), BERLIN, at(7, 0), at(7, 1)), Decision::Wait);
    }

    #[test]
    fn catches_up_within_one_hour_and_skips_older_runs() {
        assert_eq!(decide(&daily(), BERLIN, at(6, 0), at(8, 0)), Decision::Fire);
        assert_eq!(decide(&daily(), BERLIN, at(6, 0), at(8, 1)), Decision::Skip);
    }

    fn agent(trigger: &str) -> AgentDefinition {
        AgentDefinition::parse(&format!(
            "---\nname: Brief\nscope: org\ntrigger:\n  {trigger}\n---\nGo.\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn tick_records_first_sight_then_fires_once() {
        let recorder = Arc::new(Recorder::default());
        let agents = [agent("cron: \"0 8 * * *\"")];
        let scheduler = Scheduler::new(
            fresh_store().await,
            TenantId::SINGLE,
            "Europe/Berlin",
            &agents,
            recorder.clone(),
        )
        .unwrap();

        scheduler.tick(at(6, 0)).await.unwrap();
        assert!(recorder.0.lock().unwrap().is_empty());

        scheduler.tick(at(7, 0)).await.unwrap();
        scheduler.tick(at(7, 0)).await.unwrap();
        let fired = recorder.0.lock().unwrap().clone();
        assert_eq!(fired, [("brief".to_owned(), "cron".to_owned())]);
    }

    #[tokio::test]
    async fn automations_fire_beside_agents_and_updates_keep_the_other_kind() {
        let recorder = Arc::new(Recorder::default());
        let agents = [agent("cron: \"0 8 * * *\"")];
        let scheduler = Scheduler::new(
            fresh_store().await,
            TenantId::SINGLE,
            "Europe/Berlin",
            &agents,
            recorder.clone(),
        )
        .unwrap();
        let handle = scheduler.handle();
        let automations = [("follow-up".to_owned(), "0 8 * * *".to_owned())];
        handle.update_automations(&automations).unwrap();
        handle.update(&agents).unwrap();
        assert_eq!(scheduler.agents.read().unwrap().len(), 2);
        scheduler.tick(at(6, 0)).await.unwrap();
        scheduler.tick(at(7, 0)).await.unwrap();
        let mut fired = recorder.0.lock().unwrap().clone();
        fired.sort();
        assert_eq!(
            fired,
            [
                ("automation:follow-up".to_owned(), "cron".to_owned()),
                ("brief".to_owned(), "cron".to_owned())
            ]
        );
        handle.update_automations(&[]).unwrap();
        assert_eq!(scheduler.agents.read().unwrap().len(), 1);
        let bad = [("x".to_owned(), "nonsense".to_owned())];
        assert!(handle.update_automations(&bad).is_err());
    }

    #[tokio::test]
    async fn tick_skips_runs_older_than_the_limit() {
        let recorder = Arc::new(Recorder::default());
        let agents = [agent("cron: \"0 8 * * *\"")];
        let scheduler = Scheduler::new(
            fresh_store().await,
            TenantId::SINGLE,
            "Europe/Berlin",
            &agents,
            recorder.clone(),
        )
        .unwrap();
        scheduler.tick(at(6, 0)).await.unwrap();
        scheduler.tick(at(9, 0)).await.unwrap();
        scheduler.tick(at(9, 1)).await.unwrap();
        assert!(recorder.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_agents_are_not_scheduled_and_bad_zones_fail() {
        let store = fresh_store().await;
        let agents = [agent("manual: true")];
        let scheduler = Scheduler::new(
            store.clone(),
            TenantId::SINGLE,
            "UTC",
            &agents,
            Arc::new(Recorder::default()),
        )
        .unwrap();
        assert!(scheduler.agents.read().unwrap().is_empty());
        let error = Scheduler::new(
            store,
            TenantId::SINGLE,
            "Mars/Base",
            &agents,
            Arc::new(Recorder::default()),
        );
        assert!(matches!(error, Err(SchedulerError::Schedule(_))));
    }
}
