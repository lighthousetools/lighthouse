//! Resolves configured sources and bundled presets.

use lighthouse_config::{ActionConfig, CollectionConfig, FieldMap, SourceConfig};
use lighthouse_core::agent::parse_duration;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const GITHUB: &str = include_str!("presets/github.toml");
const LINEAR: &str = include_str!("presets/linear.toml");
const MICROSOFT_CALENDAR: &str = include_str!("presets/microsoft-calendar.toml");
const PIPEDRIVE: &str = include_str!("presets/pipedrive.toml");
const SUPERHUMAN_MAIL: &str = include_str!("presets/superhuman-mail.toml");

/// The domains and the item kinds each one knows.
pub const DOMAINS: [(&str, &[&str]); 10] = [
    ("mail", &["thread", "message"]),
    ("calendar", &["event"]),
    ("issues", &["issue", "project", "cycle", "comment"]),
    ("code", &["pull_request", "check", "file", "review"]),
    ("deals", &["deal", "stage", "activity"]),
    ("bank", &["account", "transaction"]),
    ("invoices", &["invoice"]),
    ("documents", &["contract", "document"]),
    (
        "rooms",
        &["room", "document", "activity", "step", "milestone"],
    ),
    ("holdings", &["holding", "quote"]),
];

/// Domains that only show data. Their sources cannot have actions, so
/// Lighthouse never trades or acts on them.
pub const READ_ONLY_DOMAINS: [&str; 1] = ["holdings"];

/// Errors in a source.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceError {
    /// No preset of this name.
    #[error("source `{source_name}`: unknown preset `{preset}`")]
    UnknownPreset {
        /// Source name.
        source_name: String,
        /// Preset name.
        preset: String,
    },
    /// Something in the source is wrong.
    #[error("source `{source_name}`: {detail}")]
    Invalid {
        /// Source name.
        source_name: String,
        /// What is wrong.
        detail: String,
    },
}

/// A source with its preset applied.
#[derive(Clone, Debug)]
pub struct Source {
    /// Unique in the tenant: what items and statuses are stored under, and what
    /// the API names it by. A shared source's is its name; a personal source's
    /// is `<name>@<user>`, so members can name theirs alike.
    pub name: String,
    /// The name people gave it.
    pub label: String,
    /// The member a personal source belongs to. Its items reach only them, and
    /// it syncs with their connectors and tokens. `None` is shared in the tenant.
    pub owner: Option<uuid::Uuid>,
    /// Why it cannot refresh now, such as a connector that is switched off or
    /// a tool that is no longer listed as read-only.
    pub problem: Option<String>,
    /// Connector whose tools are called.
    pub connector: String,
    /// Domain.
    pub domain: String,
    /// The service behind the source, such as `gmail` or `github`.
    pub service: String,
    /// The owner's accounts in this service.
    pub accounts: Vec<String>,
    /// Refresh interval.
    pub interval: Duration,
    /// Collections, top-level ones first, then those with a parent.
    pub collections: Vec<(String, CollectionConfig)>,
    /// Actions by name.
    pub actions: BTreeMap<String, ActionConfig>,
}

impl Source {
    /// Returns the qualified name of a tool of this source.
    pub fn tool(&self, tool: &str) -> String {
        format!("{}.{tool}", self.connector)
    }
}

/// The key of a personal source of a member.
pub fn personal_key(name: &str, user: uuid::Uuid) -> String {
    format!("{name}@{user}")
}

/// Returns a bundled preset.
pub fn preset(name: &str) -> Option<SourceConfig> {
    let text = match name {
        "github" => GITHUB,
        "linear" => LINEAR,
        "microsoft-calendar" => MICROSOFT_CALENDAR,
        "pipedrive" => PIPEDRIVE,
        "superhuman-mail" => SUPERHUMAN_MAIL,
        _ => return None,
    };
    let mut config: SourceConfig = toml::from_str(text).ok()?;
    config.connector.clear();
    Some(config)
}

/// Names of the bundled presets.
pub const PRESETS: [&str; 5] = [
    "github",
    "linear",
    "microsoft-calendar",
    "pipedrive",
    "superhuman-mail",
];

/// Applies the preset of a configured source and checks the result.
///
/// `accounts` are the owner's names to try, such as the configured account,
/// The collections whose titles the fields of a collection look up.
fn lookups(collection: &CollectionConfig) -> impl Iterator<Item = &str> {
    collection.fields.values().filter_map(|map| match map {
        FieldMap::Rule(rule) => rule.lookup.as_deref(),
        FieldMap::Path(_) => None,
    })
}

/// `owner.accounts.<connector>` and the owner's email address.
pub fn resolve(
    name: &str,
    config: &SourceConfig,
    accounts: Vec<String>,
) -> Result<Source, SourceError> {
    let invalid = |detail: String| SourceError::Invalid {
        source_name: name.to_owned(),
        detail,
    };
    let base = match &config.preset {
        Some(preset_name) => preset(preset_name).ok_or_else(|| SourceError::UnknownPreset {
            source_name: name.to_owned(),
            preset: preset_name.clone(),
        })?,
        None => SourceConfig::default(),
    };
    let domain = config
        .domain
        .clone()
        .or(base.domain)
        .ok_or_else(|| invalid("needs a domain: mail, calendar, issues or code".to_owned()))?;
    let kinds = DOMAINS
        .iter()
        .find(|(known, _)| *known == domain)
        .map(|(_, kinds)| *kinds)
        .ok_or_else(|| invalid(format!("unknown domain `{domain}`")))?;
    let mut collections = base.collections;
    collections.extend(config.collections.clone());
    let mut actions = base.actions;
    actions.extend(config.actions.clone());
    for (collection_name, collection) in &collections {
        if !kinds.contains(&collection.kind.as_str()) {
            return Err(invalid(format!(
                "collection `{collection_name}` has kind `{}`, which {domain} does not know",
                collection.kind
            )));
        }
        if !collection.fields.contains_key("external_id") {
            return Err(invalid(format!(
                "collection `{collection_name}` maps no `external_id`"
            )));
        }
        for target in lookups(collection) {
            let usable = collections
                .get(target)
                .is_some_and(|other| other.parent.is_none() && lookups(other).next().is_none());
            if !usable {
                return Err(invalid(format!(
                    "collection `{collection_name}` looks up `{target}`, which must be a top-level collection without lookups"
                )));
            }
        }
    }
    if READ_ONLY_DOMAINS.contains(&domain.as_str()) && !actions.is_empty() {
        return Err(invalid(format!(
            "{domain} sources only show data and cannot have actions"
        )));
    }
    let interval = config
        .interval
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .unwrap_or(DEFAULT_INTERVAL);
    let mut accounts: Vec<String> = config.account.iter().cloned().chain(accounts).collect();
    accounts.retain(|account| !account.is_empty());
    accounts.dedup();
    let mut ordered: Vec<(String, CollectionConfig)> = collections.into_iter().collect();
    // Children come last, and collections that others look up come first.
    let looked_up: BTreeSet<String> = ordered
        .iter()
        .flat_map(|(_, collection)| lookups(collection).map(str::to_owned))
        .collect();
    ordered
        .sort_by_key(|(name, collection)| (collection.parent.is_some(), !looked_up.contains(name)));
    let service = config
        .service
        .clone()
        .or_else(|| config.preset.clone())
        .unwrap_or_else(|| domain.clone());
    Ok(Source {
        name: name.to_owned(),
        label: name.to_owned(),
        owner: None,
        problem: None,
        connector: config.connector.clone(),
        service,
        domain,
        accounts,
        interval,
        collections: ordered,
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(preset: Option<&str>) -> SourceConfig {
        SourceConfig {
            connector: "github".to_owned(),
            preset: preset.map(str::to_owned),
            ..SourceConfig::default()
        }
    }

    #[test]
    fn presets_resolve_with_overrides() {
        for name in PRESETS {
            let resolved = resolve(name, &source(Some(name)), vec!["alex".to_owned()]).unwrap();
            assert!(!resolved.collections.is_empty(), "{name}");
            assert!(!resolved.actions.is_empty(), "{name}");
            let parents: Vec<bool> = resolved
                .collections
                .iter()
                .map(|(_, collection)| collection.parent.is_some())
                .collect();
            let mut sorted = parents.clone();
            sorted.sort();
            assert_eq!(parents, sorted, "top-level collections come first");
        }
        let mut config = source(Some("github"));
        config.account = Some("sam".to_owned());
        config.interval = Some("5m".to_owned());
        config.collections.insert(
            "mine".to_owned(),
            CollectionConfig {
                tool: "custom".to_owned(),
                kind: "pull_request".to_owned(),
                fields: [(
                    "external_id".to_owned(),
                    lighthouse_config::FieldMap::Path("id".to_owned()),
                )]
                .into(),
                ..CollectionConfig::default()
            },
        );
        let resolved = resolve("github", &config, vec!["alex".to_owned(), String::new()]).unwrap();
        assert_eq!(resolved.accounts, ["sam", "alex"]);
        assert_eq!(resolved.interval, Duration::from_secs(300));
        assert_eq!(resolved.domain, "code");
        let mine = resolved
            .collections
            .iter()
            .find(|(name, _)| name == "mine")
            .unwrap();
        assert_eq!(mine.1.tool, "custom");
        assert_eq!(resolved.tool("x"), "github.x");
    }

    #[test]
    fn linear_lists_are_read_from_their_wrappers() {
        let resolved = resolve("linear", &source(Some("linear")), Vec::new()).unwrap();
        let wrapped = [
            (
                "assigned",
                r#"{"issues":[{"id":"ENG-1"}],"hasNextPage":false}"#,
            ),
            (
                "projects",
                r#"{"projects":[{"id":"P-1"}],"hasNextPage":false}"#,
            ),
            (
                "comments",
                r#"{"comments":[{"id":"c1","author":{"name":"sam"}}]}"#,
            ),
        ];
        for (name, text) in wrapped {
            let (_, collection) = resolved
                .collections
                .iter()
                .find(|(n, _)| n == name)
                .unwrap();
            let items = crate::mapping::items_of(name, text, &collection.items).unwrap();
            assert_eq!(items.len(), 1, "{name}");
        }
    }

    #[test]
    fn looked_up_collections_come_first() {
        let resolved = resolve("crm", &source(Some("pipedrive")), Vec::new()).unwrap();
        let names: Vec<&str> = resolved
            .collections
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names.first(), Some(&"stages"));
        let mut config = source(Some("pipedrive"));
        let mut open = resolved
            .collections
            .iter()
            .find(|(name, _)| name == "open")
            .unwrap()
            .1
            .clone();
        open.fields.insert(
            "assignee".to_owned(),
            FieldMap::Rule(lighthouse_config::FieldRule {
                path: "owner_id".to_owned(),
                lookup: Some("missing".to_owned()),
                ..lighthouse_config::FieldRule::default()
            }),
        );
        config.collections.insert("open".to_owned(), open);
        assert!(
            resolve("crm", &config, Vec::new()).is_err(),
            "unknown lookup"
        );
    }

    #[test]
    fn mail_threads_and_messages_are_read_from_superhuman() {
        let resolved = resolve("mail", &source(Some("superhuman-mail")), Vec::new()).unwrap();
        assert_eq!(resolved.domain, "mail");
        let (_, inbox) = resolved
            .collections
            .iter()
            .find(|(name, _)| name == "inbox")
            .unwrap();
        let text = r#"{"threads":[{"thread_id":"t1","subject":"Hi","splits":["Important"],"messages":[{"from":"sam@example.com"}]}]}"#;
        let items = crate::mapping::items_of("list_threads", text, &inbox.items).unwrap();
        let owner = crate::mapping::Owner {
            accounts: Vec::new(),
        };
        let thread =
            crate::mapping::map_item(inbox, &items[0], &owner, chrono::Utc::now()).unwrap();
        assert_eq!(thread.author.as_deref(), Some("sam@example.com"));
        assert!(thread.flags.iter().any(|flag| flag == "important"));
        assert!(resolved.actions.contains_key("draft") && resolved.actions.contains_key("archive"));
    }

    #[test]
    fn graph_events_become_calendar_items() {
        let resolved =
            resolve("calendar", &source(Some("microsoft-calendar")), Vec::new()).unwrap();
        assert_eq!(resolved.domain, "calendar");
        let (_, events) = resolved
            .collections
            .iter()
            .find(|(name, _)| name == "events")
            .unwrap();
        let text = r#"{"value":[{"id":"AAMk1","subject":"Review","showAs":"busy",
            "start":{"dateTime":"2026-11-12T09:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-11-12T09:30:00.0000000","timeZone":"UTC"},
            "location":{"displayName":"Room 1"},"organizer":{"emailAddress":{"name":"Sam","address":"sam@example.com"}}}]}"#;
        let items = crate::mapping::items_of("get_me_calendarview", text, &events.items).unwrap();
        let owner = crate::mapping::Owner {
            accounts: Vec::new(),
        };
        let event =
            crate::mapping::map_item(events, &items[0], &owner, chrono::Utc::now()).unwrap();
        assert_eq!(
            event.starts_at.unwrap().to_rfc3339(),
            "2026-11-12T09:00:00+00:00"
        );
        assert_eq!(
            event.ends_at.unwrap().to_rfc3339(),
            "2026-11-12T09:30:00+00:00"
        );
        assert_eq!(event.state.as_deref(), Some("busy"));
        assert_eq!(event.data["location"], "Room 1");
        for action in ["create", "move", "accept", "decline"] {
            assert!(resolved.actions.contains_key(action), "{action}");
        }
    }

    #[test]
    fn broken_sources_are_refused() {
        assert!(matches!(
            resolve("x", &source(Some("nope")), Vec::new()),
            Err(SourceError::UnknownPreset { .. })
        ));
        assert!(
            resolve("x", &source(None), Vec::new()).is_err(),
            "no domain"
        );
        let mut wrong = source(None);
        wrong.domain = Some("weather".to_owned());
        assert!(resolve("x", &wrong, Vec::new()).is_err());
        let mut kind = source(None);
        kind.domain = Some("mail".to_owned());
        kind.collections.insert(
            "a".to_owned(),
            CollectionConfig {
                tool: "t".to_owned(),
                kind: "issue".to_owned(),
                ..CollectionConfig::default()
            },
        );
        assert!(resolve("x", &kind, Vec::new()).is_err());
        kind.collections.get_mut("a").unwrap().kind = "thread".to_owned();
        assert!(resolve("x", &kind, Vec::new()).is_err(), "no external_id");
        let mut interval = source(Some("github"));
        interval.interval = Some("often".to_owned());
        assert!(resolve("x", &interval, Vec::new()).is_err());
        assert!(preset("nope").is_none());
        let mut holdings = source(None);
        holdings.domain = Some("holdings".to_owned());
        holdings
            .actions
            .insert("buy".to_owned(), lighthouse_config::ActionConfig::default());
        assert!(
            resolve("x", &holdings, Vec::new()).is_err(),
            "no actions on holdings"
        );
    }
}
