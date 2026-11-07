//! Refreshes sources through their connector tools.

use crate::mapping::{DERIVED_FLAGS, MappedItem, Owner, items_of, map_item, render_value, uses};
use crate::sources::Source;
use crate::store::{SyncStore, SyncedItem};
use chrono::{DateTime, Duration, Utc};
use lighthouse_config::CollectionConfig;
use lighthouse_core::RunId;
use lighthouse_mcp::{ToolHost, ToolHostFactory};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// How far back the first refresh reaches.
const FIRST_WINDOW: Duration = Duration::days(30);
/// Overlap of incremental refreshes, so nothing falls between two of them.
const OVERLAP: Duration = Duration::minutes(10);
const DEFAULT_PARENTS: usize = 20;

/// Errors of a refresh or an action.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// No source of this name.
    #[error("unknown source `{0}`")]
    UnknownSource(String),
    /// The source has no such action, or the item does not fit it.
    #[error("{0}")]
    Action(String),
    /// The cache failed.
    #[error("cache error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Something that happened in a service, for automations.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncEvent {
    /// `mail.received`, `issue.assigned`, `review.requested` or `check.failed`.
    pub name: String,
    /// IDs and a few fields of the item.
    pub payload: Value,
}

/// A check of a pull request that started or stopped failing.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckChange {
    /// Identifies the check, for alerts.
    pub key: String,
    /// Whether it fails now.
    pub failed: bool,
    /// Check name.
    pub title: String,
    /// The pull request.
    pub pull_request: Option<String>,
    /// Address of the check.
    pub url: Option<String>,
    /// Whether the pull request is the owner's.
    pub on_my_pull_request: bool,
}

/// What a refresh did.
#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    /// Source name.
    pub source: String,
    /// Items fetched.
    pub items: usize,
    /// Events for automations. Empty on the first refresh of a source.
    pub events: Vec<SyncEvent>,
    /// Checks that changed between failing and passing.
    pub checks: Vec<CheckChange>,
    /// Errors of single collections.
    pub errors: Vec<String>,
}

/// A tool call an action resolves to.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedAction {
    /// Qualified tool name.
    pub tool: String,
    /// Arguments.
    pub input: Value,
}

/// Removes keys whose value is an empty string, so optional inputs stay out.
pub fn prune_empty(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, item)| item.as_str() != Some(""))
                .map(|(key, item)| (key, prune_empty(item)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(prune_empty).collect()),
        other => other,
    }
}

fn payload(source: &Source, kind: &str, stored_id: uuid::Uuid, item: &MappedItem) -> Value {
    json!({
        "source": source.name,
        "domain": source.domain,
        "kind": kind,
        "id": stored_id,
        "external_id": item.external_id,
        "parent_id": item.parent_id,
        "title": item.title,
        "author": item.author,
        "from": item.author,
        "url": item.url,
        "flags": item.flags,
    })
}

/// Refreshes the configured sources.
pub struct Syncer {
    store: SyncStore,
    tools: Arc<dyn ToolHostFactory>,
    sources: Vec<Source>,
}

impl std::fmt::Debug for Syncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syncer")
            .field("sources", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl Syncer {
    /// Creates a syncer. The tools should reach the connectors directly.
    pub fn new(store: SyncStore, tools: Arc<dyn ToolHostFactory>, sources: Vec<Source>) -> Self {
        Self {
            store,
            tools,
            sources,
        }
    }

    /// Returns the cache.
    pub fn store(&self) -> &SyncStore {
        &self.store
    }

    /// Returns the sources.
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// Returns a source.
    pub fn source(&self, name: &str) -> Option<&Source> {
        self.sources.iter().find(|source| source.name == name)
    }

    /// Returns the sources whose interval has passed since their last attempt.
    pub async fn due(&self, now: DateTime<Utc>) -> Result<Vec<String>, SyncError> {
        let statuses = self.store.statuses().await?;
        Ok(self
            .sources
            .iter()
            .filter(|source| {
                let last = statuses
                    .iter()
                    .find(|status| status.source == source.name)
                    .and_then(|status| status.last_attempt_at);
                let interval = Duration::from_std(source.interval).unwrap_or(Duration::MAX);
                last.is_none_or(|last| now - last >= interval)
            })
            .map(|source| source.name.clone())
            .collect())
    }

    /// Refreshes a source. Errors of single collections are recorded and reported.
    pub async fn sync(&self, name: &str, now: DateTime<Utc>) -> Result<SyncReport, SyncError> {
        let source = self
            .source(name)
            .ok_or_else(|| SyncError::UnknownSource(name.to_owned()))?;
        let last_success = self
            .store
            .statuses()
            .await?
            .into_iter()
            .find(|status| status.source == name)
            .and_then(|status| status.last_success_at);
        let since = last_success.map_or(now - FIRST_WINDOW, |time| time - OVERLAP);
        let context = json!({
            "since": since.to_rfc3339(),
            "since_date": since.format("%Y-%m-%d").to_string(),
            "now": now.to_rfc3339(),
            "account": source.accounts.first().cloned().unwrap_or_default(),
        });
        let mut report = SyncReport {
            source: name.to_owned(),
            ..SyncReport::default()
        };
        let tools: Vec<String> = source
            .collections
            .iter()
            .map(|(_, collection)| source.tool(&collection.tool))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let host = match self.tools.open(&tools, RunId::new()).await {
            Ok(host) => host,
            Err(error) => {
                let error = error.to_string();
                self.store.record_sync(name, Err(&error)).await?;
                report.errors.push(error);
                return Ok(report);
            }
        };
        let owner = Owner {
            accounts: source.accounts.clone(),
        };
        for (collection_name, collection) in &source.collections {
            let result = self
                .collection(
                    source,
                    collection_name,
                    collection,
                    host.as_ref(),
                    &context,
                    &owner,
                    now,
                    &mut report,
                )
                .await;
            if let Err(error) = result {
                report.errors.push(format!("{collection_name}: {error}"));
            }
        }
        if last_success.is_none() {
            report.events.clear();
        }
        let outcome = match report.errors.first() {
            Some(error) => Err(error.as_str()),
            None => Ok(report.items),
        };
        self.store.record_sync(name, outcome).await?;
        Ok(report)
    }

    async fn call(
        &self,
        source: &Source,
        collection: &CollectionConfig,
        host: &dyn ToolHost,
        context: &Value,
    ) -> Result<Vec<Value>, String> {
        let template = collection.input.clone().unwrap_or_else(|| json!({}));
        let input = prune_empty(render_value(&template, context));
        let tool = source.tool(&collection.tool);
        let output = host
            .call_tool(&tool, input)
            .await
            .map_err(|error| error.to_string())?;
        if output.is_error {
            return Err(format!(
                "`{tool}` failed: {}",
                output.text.chars().take(300).collect::<String>()
            ));
        }
        items_of(&tool, &output.text, &collection.items).map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn collection(
        &self,
        source: &Source,
        name: &str,
        collection: &CollectionConfig,
        host: &dyn ToolHost,
        context: &Value,
        owner: &Owner,
        now: DateTime<Utc>,
        report: &mut SyncReport,
    ) -> Result<(), String> {
        let mut mapped = Vec::new();
        match &collection.parent {
            None => {
                let items = self.call(source, collection, host, context).await?;
                mapped.extend(
                    items
                        .iter()
                        .filter_map(|item| map_item(collection, item, owner, now)),
                );
            }
            Some(kind) => {
                let limit = collection.parents_limit.unwrap_or(DEFAULT_PARENTS);
                let parents = self
                    .store
                    .parents(&source.name, kind, i64::try_from(limit).unwrap_or(i64::MAX))
                    .await
                    .map_err(|error| error.to_string())?;
                for parent in parents {
                    let mut context = context.clone();
                    context["parent"] = parent.context();
                    let items = self.call(source, collection, host, &context).await?;
                    for item in &items {
                        if let Some(mut child) = map_item(collection, item, owner, now) {
                            child.external_id =
                                format!("{}/{}", parent.external_id, child.external_id);
                            child.parent_id = Some(parent.external_id.clone());
                            mapped.push(child);
                        }
                    }
                }
            }
        }
        report.items += mapped.len();
        let mut managed: Vec<String> = collection.flags.clone();
        managed.extend(DERIVED_FLAGS.iter().map(|flag| (*flag).to_owned()));
        let stored = self
            .store
            .upsert(
                &source.name,
                &source.domain,
                &collection.kind,
                &mapped,
                &managed,
            )
            .await
            .map_err(|error| error.to_string())?;
        for (item, stored) in mapped.iter().zip(&stored) {
            self.transitions(source, &collection.kind, item, stored, report)
                .await
                .map_err(|error| error.to_string())?;
        }
        let incremental = collection
            .input
            .as_ref()
            .is_some_and(|input| uses(input, "since"));
        let seen: Vec<String> = mapped.iter().map(|item| item.external_id.clone()).collect();
        self.store
            .mark_seen(&source.name, &collection.kind, name, &seen)
            .await
            .map_err(|error| error.to_string())?;
        if collection.parent.is_none() && !incremental {
            self.store
                .prune_flags(&source.name, &collection.kind, &seen, &collection.flags)
                .await
                .map_err(|error| error.to_string())?;
            self.store
                .prune_vanished(&source.name, &collection.kind, name, &seen)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn transitions(
        &self,
        source: &Source,
        kind: &str,
        item: &MappedItem,
        stored: &crate::store::Stored,
        report: &mut SyncReport,
    ) -> Result<(), sqlx::Error> {
        let had = |flag: &str| stored.previous_flags.iter().any(|known| known == flag);
        let has = |flag: &str| item.flags.iter().any(|known| known == flag);
        let gained = |flag: &str| has(flag) && (stored.inserted || !had(flag));
        let event = |name: &str| SyncEvent {
            name: name.to_owned(),
            payload: payload(source, kind, stored.id, item),
        };
        match kind {
            "thread" | "message" if stored.inserted && !has("from_me") => {
                report.events.push(event("mail.received"));
            }
            "issue" if gained("assigned") => report.events.push(event("issue.assigned")),
            "pull_request" if gained("review_requested") => {
                report.events.push(event("review.requested"));
            }
            "check" => {
                let failed_now = has("failed");
                let changed = if failed_now {
                    gained("failed")
                } else {
                    had("failed")
                };
                if changed {
                    let parent = match &item.parent_id {
                        Some(parent) => {
                            self.store
                                .item_by_external(&source.name, "pull_request", parent)
                                .await?
                        }
                        None => None,
                    };
                    let mine = parent.as_ref().is_some_and(|parent: &SyncedItem| {
                        parent.flags.iter().any(|flag| flag == "mine")
                    });
                    if failed_now {
                        let mut failed = event("check.failed");
                        failed.payload["on_my_pull_request"] = json!(mine);
                        report.events.push(failed);
                    }
                    report.checks.push(CheckChange {
                        key: format!("check:{}:{}", source.name, item.external_id),
                        failed: failed_now,
                        title: item
                            .title
                            .clone()
                            .unwrap_or_else(|| item.external_id.clone()),
                        pull_request: parent
                            .and_then(|parent| parent.title.or(Some(parent.external_id))),
                        url: item.url.clone(),
                        on_my_pull_request: mine,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Resolves an action of a source into a tool call.
    pub fn prepare_action(
        &self,
        source: &str,
        action: &str,
        item: Option<&SyncedItem>,
        input: &Value,
    ) -> Result<PreparedAction, SyncError> {
        let found = self
            .source(source)
            .ok_or_else(|| SyncError::UnknownSource(source.to_owned()))?;
        let config = found.actions.get(action).ok_or_else(|| {
            SyncError::Action(format!("the source `{source}` has no action `{action}`"))
        })?;
        if let Some(kind) = &config.kind {
            match item {
                Some(item) if &item.kind == kind => {}
                _ => {
                    return Err(SyncError::Action(format!(
                        "the action `{action}` needs an item of kind `{kind}`"
                    )));
                }
            }
        }
        let context = json!({
            "item": item.map(SyncedItem::context),
            "input": input,
            "account": found.accounts.first().cloned().unwrap_or_default(),
        });
        let template = config.input.clone().unwrap_or_else(|| json!({}));
        Ok(PreparedAction {
            tool: found.tool(&config.tool),
            input: prune_empty(render_value(&template, &context)),
        })
    }
}
