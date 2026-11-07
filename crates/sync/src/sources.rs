//! Resolves configured sources and bundled presets.

use lighthouse_config::{ActionConfig, CollectionConfig, SourceConfig};
use lighthouse_core::agent::parse_duration;
use std::collections::BTreeMap;
use std::time::Duration;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const GITHUB: &str = include_str!("presets/github.toml");
const LINEAR: &str = include_str!("presets/linear.toml");

/// The domains and the item kinds each one knows.
pub const DOMAINS: [(&str, &[&str]); 4] = [
    ("mail", &["thread", "message"]),
    ("calendar", &["event"]),
    ("issues", &["issue", "project", "cycle", "comment"]),
    ("code", &["pull_request", "check", "file", "review"]),
];

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
    /// Source name.
    pub name: String,
    /// Connector whose tools are called.
    pub connector: String,
    /// Domain.
    pub domain: String,
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

/// Returns a bundled preset.
pub fn preset(name: &str) -> Option<SourceConfig> {
    let text = match name {
        "github" => GITHUB,
        "linear" => LINEAR,
        _ => return None,
    };
    let mut config: SourceConfig = toml::from_str(text).ok()?;
    config.connector.clear();
    Some(config)
}

/// Names of the bundled presets.
pub const PRESETS: [&str; 2] = ["github", "linear"];

/// Applies the preset of a configured source and checks the result.
///
/// `accounts` are the owner's names to try, such as the configured account,
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
    ordered.sort_by_key(|(_, collection)| collection.parent.is_some());
    Ok(Source {
        name: name.to_owned(),
        connector: config.connector.clone(),
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
    }
}
