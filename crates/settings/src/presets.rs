//! Suggested providers, models and connectors for the apps.
//!
//! Model names and prices are data in `presets.toml`, not code. The server
//! never routes by them; it only hands them to the apps to prefill forms.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use utoipa::ToSchema;

const PRESETS: &str = include_str!("../presets.toml");
/// Remote MCP servers run by their vendors, checked by hand.
const CATALOG: &str = include_str!("../catalog.toml");

/// Groups of the catalog.
pub const CATEGORIES: [&str; 33] = [
    "accounting", "advertising", "ai", "analytics", "automation", "ci", "cloud", "code", "commerce",
    "communication", "crm", "database", "design", "docs", "documents", "finance", "health", "hr",
    "knowledge", "maps", "marketing", "media", "meetings", "observability", "payments", "productivity",
    "project-management", "search", "security", "social", "storage", "support", "travel",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    connectors: Vec<PresetConnector>,
}

/// A suggested model.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PresetModel {
    /// Task it suits.
    pub task: String,
    /// Model name as the provider expects it.
    pub model: String,
    /// Display name.
    pub label: String,
    /// List price in EUR per million input tokens.
    pub input_eur_per_mtok: f64,
    /// List price in EUR per million output tokens.
    pub output_eur_per_mtok: f64,
}

/// A suggested provider.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PresetProvider {
    /// Suggested name.
    pub id: String,
    /// Display name.
    pub label: String,
    /// `anthropic` or `openai`.
    pub kind: String,
    /// Base URL, if not the provider's default.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Where to create a key.
    #[serde(default)]
    pub key_url: Option<String>,
    /// Suggested models.
    #[serde(default)]
    pub models: Vec<PresetModel>,
}

/// A suggested MCP connector.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PresetConnector {
    /// Suggested name.
    pub id: String,
    /// Display name.
    pub label: String,
    /// Server URL.
    pub url: String,
    /// `none`, `bearer` or `oauth`.
    pub auth: String,
    /// Tools known to be read-only.
    #[serde(default)]
    pub read_tools: Vec<String>,
    /// Reversible tools.
    #[serde(default)]
    pub draft_tools: Vec<String>,
    /// Who runs the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// A group for browsing, such as `crm` or `storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// `dynamic` when the server registers clients itself, `preregistered` when you register an OAuth app first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
    /// The vendor's page to create a token, for `bearer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_url: Option<String>,
    /// The vendor's documentation of the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Something to know before connecting, such as a region or an admin setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PresetConnector {
    /// Whether the URL holds a part to fill in, such as `{subdomain}`.
    pub fn is_template(&self) -> bool {
        self.url.contains('{')
    }
}

/// A suggested connected source.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PresetSource {
    /// Suggested name.
    pub id: String,
    /// Display name.
    pub label: String,
    /// The source in the shape of `[sources.<name>]`, to edit.
    pub definition: serde_json::Value,
}

/// Every suggestion.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Presets {
    /// Providers with their models.
    pub providers: Vec<PresetProvider>,
    /// Connectors.
    #[serde(default)]
    pub connectors: Vec<PresetConnector>,
    /// Connected sources.
    #[serde(default)]
    pub sources: Vec<PresetSource>,
}

/// The suggestions of `presets.toml`.
pub fn presets() -> &'static Presets {
    static PARSED: OnceLock<Presets> = OnceLock::new();
    PARSED.get_or_init(|| {
        let mut presets: Presets = toml::from_str(PRESETS).expect("presets.toml is valid");
        let catalog: Catalog = toml::from_str(CATALOG).expect("catalog.toml is valid");
        for connector in catalog.connectors {
            if presets.connectors.iter().all(|known| known.id != connector.id) {
                presets.connectors.push(connector);
            }
        }
        presets
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;

    #[test]
    fn presets_pass_the_checks_of_entries() {
        let presets = presets();
        assert!(presets.providers.len() >= 2);
        for provider in &presets.providers {
            validate::name("provider", &provider.id).unwrap();
            validate::kind(&provider.kind).unwrap();
            assert!(!provider.models.is_empty());
            for model in &provider.models {
                validate::task(&model.task).unwrap();
                validate::model(&model.model).unwrap();
                validate::price("input", model.input_eur_per_mtok).unwrap();
                validate::price("output", model.output_eur_per_mtok).unwrap();
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut urls = std::collections::BTreeSet::new();
        for connector in &presets.connectors {
            assert!(ids.insert(connector.id.clone()), "{} twice", connector.id);
            assert!(urls.insert(connector.url.trim_end_matches('/').to_owned()), "{} twice", connector.url);
            validate::name("connector", &connector.id).unwrap();
            let url = connector.url.replace(['{', '}'], "");
            assert!(url.starts_with("https://"), "{}", connector.url);
            lighthouse_router::net::check_url(&url).unwrap();
            if let Some(category) = &connector.category {
                assert!(CATEGORIES.contains(&category.as_str()), "{category}");
            }
            if let Some(registration) = &connector.registration {
                assert!(["dynamic", "preregistered"].contains(&registration.as_str()), "{registration}");
            }
            assert!(crate::store::parse_auth(&connector.auth).is_some());
            validate::tools("read_tools", &connector.read_tools).unwrap();
        }
        assert!(presets.connectors.len() >= 400, "{}", presets.connectors.len());
        assert!(presets.sources.len() >= 4);
        for source in &presets.sources {
            validate::name("source", &source.id).unwrap();
            let config: lighthouse_config::SourceConfig =
                serde_json::from_value(source.definition.clone()).unwrap();
            lighthouse_sync::sources::resolve(&source.id, &config, Vec::new()).unwrap();
        }
    }
}
